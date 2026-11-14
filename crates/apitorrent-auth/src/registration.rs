//! Registering an app as a device with a token from the server's console, instead of
//! approving it from a signed-in browser: how a headless daemon is set up and reached from
//! the desktop app without ever opening the web UI.
//!
//! - **No users yet:** the token is the setup token the daemon prints at startup (valid
//!   ten minutes, then replaced). The app's pairing request carries it with a username;
//!   that makes the first admin, with no passkey or password yet, and approves the request
//!   as that admin's device.
//! - **Users exist:** `apitorrentd register-device [--user NAME]`, run on the server,
//!   writes a one-use token for that user to the auth database
//!   ([`create_registration_token`]) and prints it. The app's request carries it.
//!
//! Either way the device works in all of its user's spaces, with full scope: it is that
//! user's app, as a browser session would be. Key pinning and binding work as for any
//! pairing. Wrong tokens count against the caller's address like wrong passwords.
//!
//! Registration tokens aren't printed at every startup: the journal is often readable by
//! more people than the data directory (the `adm` group), and a token there would make any
//! of them an admin's device.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use apitorrent_types::auth::{DeviceRegistration, Role};

use crate::clock::ms;
use crate::secret;
use crate::store::{self, UserRow, user_by_id, user_by_name};
use crate::{Auth, AuthError, ClientMeta, Result, normalize_username, optional_label};

/// How long a registration token from `apitorrentd register-device` works.
pub const REGISTRATION_TTL: Duration = Duration::from_secs(10 * 60);

/// A token made by [`create_registration_token`], to print once.
#[derive(Clone)]
pub struct IssuedRegistration {
    pub token: String,
    /// The user whose device it registers.
    pub username: String,
    pub expires_at: i64,
}

impl std::fmt::Debug for IssuedRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedRegistration")
            .field("username", &self.username)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// Make a one-use token that registers one device for `username`, or, when no name is
/// given, for the only server admin. Opens the auth database at `db_path` itself: this is
/// for the command line on the server, while the daemon may be running.
pub fn create_registration_token(
    db_path: &Path,
    username: Option<&str>,
    now_ms: i64,
) -> Result<IssuedRegistration> {
    let conn = crate::db::open(db_path)?;
    let user = registrant(&conn, username)?;
    let token = secret::readable_token();
    let expires_at = now_ms + ms(REGISTRATION_TTL);
    conn.execute(
        "DELETE FROM registration_tokens WHERE expires_at <= ?1",
        [now_ms],
    )?;
    conn.execute(
        "INSERT INTO registration_tokens (digest, user_id, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![digest(&token).as_slice(), user.id, now_ms, expires_at],
    )?;
    store::event(
        &conn,
        now_ms,
        "registration_token_created",
        Some(&user.id),
        None,
        "on the server's command line",
    )?;
    Ok(IssuedRegistration {
        token,
        username: user.name,
        expires_at,
    })
}

fn digest(token: &str) -> secret::Digest32 {
    secret::sha256(secret::normalize_readable(token).as_bytes())
}

/// The user a registration token is for.
fn registrant(conn: &Connection, username: Option<&str>) -> Result<UserRow> {
    if let Some(name) = username {
        let name = normalize_username(name)?;
        return match user_by_name(conn, &name)? {
            Some(u) if !u.disabled => Ok(u),
            Some(_) => Err(AuthError::InvalidRequest(format!("{name} is disabled"))),
            None => Err(AuthError::InvalidRequest(format!(
                "there is no user {name}"
            ))),
        };
    }
    let mut stmt = conn.prepare("SELECT id FROM users WHERE role = 'admin'")?;
    let ids: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut admins = Vec::new();
    for id in ids {
        admins.extend(user_by_id(conn, &id)?.filter(|u| !u.disabled));
    }
    match admins.len() {
        1 => Ok(admins.remove(0)),
        0 if store::user_count(conn)? == 0 => Err(AuthError::InvalidRequest(
            "there are no users yet: use the setup token the daemon prints when it starts".into(),
        )),
        0 => Err(AuthError::InvalidRequest(
            "there is no enabled server admin: name the user with --user".into(),
        )),
        _ => {
            let mut names: Vec<String> = admins.into_iter().map(|u| u.name).collect();
            names.sort();
            Err(AuthError::InvalidRequest(format!(
                "there are several server admins ({}): name one with --user",
                names.join(", ")
            )))
        }
    }
}

impl Auth {
    /// The user a pairing request's registration makes it a device of: the first admin,
    /// made now, while there are no users; else the user its one-use token was made for.
    pub(crate) fn registered_user(
        &self,
        reg: &DeviceRegistration,
        meta: &ClientMeta,
    ) -> Result<String> {
        if self.setup_required()? {
            return self.register_first_admin(reg, meta);
        }
        let now = self.now();
        let conn = self.db();
        conn.execute(
            "DELETE FROM registration_tokens WHERE expires_at <= ?1",
            [now],
        )?;
        let user_id: Option<String> = conn
            .query_row(
                "DELETE FROM registration_tokens WHERE digest = ?1 RETURNING user_id",
                [digest(&reg.token).as_slice()],
                |r| r.get(0),
            )
            .optional()?;
        let user = match user_id {
            Some(id) => user_by_id(&conn, &id)?.filter(|u| !u.disabled),
            None => None,
        };
        drop(conn);
        let Some(user) = user else {
            let addr = crate::limit::address_key(meta.ip);
            if self.inner.limiter.fail(&addr, &crate::limit::ADDRESS) {
                self.log("lockout", None, meta, &addr);
            }
            self.log(
                "registration_failed",
                None,
                meta,
                "wrong, used or expired registration token",
            );
            return Err(AuthError::InvalidRegistrationToken);
        };
        Ok(user.id)
    }

    fn register_first_admin(&self, reg: &DeviceRegistration, meta: &ClientMeta) -> Result<String> {
        let generation = self.check_setup_token(&reg.token, meta)?;
        let username = normalize_username(reg.username.as_deref().ok_or_else(|| {
            AuthError::InvalidRequest(
                "this server has no users yet: choose a username for its first admin".into(),
            )
        })?)?;
        let display_name = optional_label(reg.display_name.as_deref(), &username, "display names")?;
        let user = UserRow {
            id: Uuid::new_v4().to_string(),
            name: username,
            display_name,
            role: Role::Admin,
            password_hash: None,
            disabled: false,
        };
        let mut conn = self.db();
        let tx = conn.transaction()?;
        if store::user_count(&tx)? > 0 {
            return Err(AuthError::AlreadySetUp);
        }
        if !self.setup_generation_live(generation) {
            return Err(AuthError::InvalidSetupToken);
        }
        store::insert_user(&tx, &user, self.now())?;
        self.log_with(
            &tx,
            "setup",
            Some(&user.id),
            meta,
            "first admin registered from an app, with the setup token",
        );
        tx.commit()?;
        drop(conn);
        self.finish_setup_token();
        tracing::info!(user = %user.name, "setup complete: first admin registered from an app");
        Ok(user.id)
    }
}

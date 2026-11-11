//! Invitations, recovery links and user management.
//!
//! - **An invitation** is a one-time link that creates a user. It carries the instance role
//!   and the spaces (with a role in each) the new user gets. A server admin may invite into
//!   any space and give any instance role; a space admin only into spaces they administer,
//!   and only the `user` instance role.
//! - **A recovery link** is built the same way, for an existing user who lost their
//!   credentials. Whoever opens it registers a new passkey (or password); the old passkeys
//!   and password are removed and every session ends. API tokens stay.
//! - Both expire (7 days by default, 30 at most), can be revoked, work once, and are stored
//!   only as the SHA-256 of their 256-bit secret. Redeeming one is rate-limited and audited
//!   like setup and sign-in: a wrong, used, revoked or expired secret counts as a failure
//!   against the caller's address.
//! - **Server admins manage users**: list them, change their instance role, disable or
//!   delete them. Nobody can do that to themselves, and the last server admin can't be
//!   demoted, disabled or deleted. Disabling ends the user's sessions at once and suspends
//!   their API tokens (they work again if the user is enabled again).

use std::fmt;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use webauthn_rs::prelude::{Passkey, RegisterPublicKeyCredential, Uuid};

use apitorrent_types::auth::{CeremonyStart, PasswordMode, Role, SessionMethod, UserInfo};
use apitorrent_types::spaces::{SpaceId, SpaceRole};
use apitorrent_types::users::{DEFAULT_LINK_DAYS, EnrollmentKind, EnrollmentState, MAX_LINK_DAYS};

use crate::clock::ms;
use crate::store::{self, PasskeyRow, UserRow};
use crate::{
    Auth, AuthError, ClientMeta, NewSession, Pending, Principal, Result, internal, limit,
    normalize_username, optional_label, parse, registration_options, secret,
};

/// Used, revoked and expired links are forgotten this long after they stopped working.
const KEEP_FINISHED: Duration = Duration::from_secs(30 * 24 * 3600);
const MAX_NOTE: usize = 200;

/// A new link. Give `secret` to the person it is for; it is never shown again.
#[derive(Clone)]
pub struct IssuedLink {
    pub secret: String,
    pub record: EnrollmentRecord,
}

impl fmt::Debug for IssuedLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuedLink")
            .field("secret", &"<redacted>")
            .field("record", &self.record)
            .finish()
    }
}

/// An invitation or recovery link. Spaces are ids here; the server names them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrollmentRecord {
    pub id: String,
    pub kind: EnrollmentKind,
    pub state: EnrollmentState,
    /// The instance role an invitation gives.
    pub role: Option<Role>,
    pub spaces: Vec<(SpaceId, SpaceRole)>,
    /// A recovery link's user, or the user an invitation created.
    pub user_id: Option<String>,
    pub user_name: Option<String>,
    pub user_display_name: Option<String>,
    /// The issuer's display name, if they still exist.
    pub created_by: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
    pub used_at: Option<i64>,
    pub note: Option<String>,
}

/// A user, as server admins manage them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRecord {
    pub info: UserInfo,
    pub disabled: bool,
    pub created_at: i64,
    pub passkeys: u32,
    pub tokens: u32,
    pub last_seen_at: Option<i64>,
    pub spaces: Vec<(SpaceId, SpaceRole)>,
}

/// How an enrollment finishes: with a passkey or (fallback mode) a password.
enum Completion {
    Passkey {
        rp_id: String,
        passkey: Box<Passkey>,
        name: String,
    },
    Password {
        hash: String,
    },
}

fn kind_str(k: EnrollmentKind) -> &'static str {
    match k {
        EnrollmentKind::Invitation => "invite",
        EnrollmentKind::Recovery => "recovery",
    }
}

fn link_lifetime(days: Option<u32>) -> Result<i64> {
    let days = days.unwrap_or(DEFAULT_LINK_DAYS);
    if !(1..=MAX_LINK_DAYS).contains(&days) {
        return Err(AuthError::InvalidRequest(format!(
            "links last 1 to {MAX_LINK_DAYS} days"
        )));
    }
    Ok(ms(Duration::from_secs(u64::from(days) * 86_400)))
}

fn clean_note(note: Option<&str>) -> Result<Option<String>> {
    let Some(n) = note.map(str::trim).filter(|n| !n.is_empty()) else {
        return Ok(None);
    };
    if n.chars().count() > MAX_NOTE || n.chars().any(char::is_control) {
        return Err(AuthError::InvalidRequest(format!(
            "notes are at most {MAX_NOTE} characters without control characters"
        )));
    }
    Ok(Some(n.to_owned()))
}

/// Whether a signed-in user may accept this link into their own account: an invitation
/// that doesn't make a server admin (joining never changes the instance role).
pub fn joinable(rec: &EnrollmentRecord) -> bool {
    rec.kind == EnrollmentKind::Invitation && rec.role != Some(Role::Admin)
}

fn is_constraint(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::ConstraintViolation)
}

fn space_roles(
    conn: &Connection,
    sql: &str,
    key: &str,
) -> rusqlite::Result<Vec<(SpaceId, SpaceRole)>> {
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt
        .query_map([key], |r| {
            Ok((r.get::<_, SpaceId>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(s, r)| SpaceRole::parse(&r).map(|r| (s, r)))
        .collect())
}

const RECORD_SQL: &str = "SELECT e.id, e.kind, e.role, e.user_id, u.name, u.display_name,
         c.display_name, e.created_at, e.expires_at, e.used_at, e.revoked_at, e.note
     FROM enrollments e
     LEFT JOIN users u ON u.id = e.user_id
     LEFT JOIN users c ON c.id = e.created_by";

fn record_row(
    conn: &Connection,
    r: &rusqlite::Row<'_>,
    now: i64,
) -> rusqlite::Result<EnrollmentRecord> {
    let id: String = r.get(0)?;
    let kind = if r.get::<_, String>(1)? == "recovery" {
        EnrollmentKind::Recovery
    } else {
        EnrollmentKind::Invitation
    };
    let expires_at: i64 = r.get(8)?;
    let used_at: Option<i64> = r.get(9)?;
    let revoked_at: Option<i64> = r.get(10)?;
    let state = if used_at.is_some() {
        EnrollmentState::Used
    } else if revoked_at.is_some() {
        EnrollmentState::Revoked
    } else if expires_at <= now {
        EnrollmentState::Expired
    } else {
        EnrollmentState::Pending
    };
    let spaces = space_roles(
        conn,
        "SELECT space_id, role FROM enrollment_spaces WHERE enrollment_id = ?1 ORDER BY space_id",
        &id,
    )?;
    Ok(EnrollmentRecord {
        id,
        kind,
        state,
        role: r.get::<_, Option<String>>(2)?.map(|s| store::role_from(&s)),
        spaces,
        user_id: r.get(3)?,
        user_name: r.get(4)?,
        user_display_name: r.get(5)?,
        created_by: r.get(6)?,
        created_at: r.get(7)?,
        expires_at,
        used_at,
        note: r.get(11)?,
    })
}

fn record(conn: &Connection, id: &str, now: i64) -> rusqlite::Result<Option<EnrollmentRecord>> {
    conn.query_row(&format!("{RECORD_SQL} WHERE e.id = ?1"), [id], |r| {
        record_row(conn, r, now)
    })
    .optional()
}

/// Server admins that are enabled, other than `except`.
fn other_admins(conn: &Connection, except: &str) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM users WHERE role = 'admin' AND disabled_at IS NULL AND id != ?1",
        [except],
        |r| r.get(0),
    )
}

const ADMINS_ONLY: AuthError = AuthError::Forbidden("only a server admin can do that");

impl Auth {
    /// A wrong, used, revoked or expired link: counted against the address and audited.
    fn bad_link(&self, meta: &ClientMeta, detail: &str) -> AuthError {
        let addr = limit::address_key(meta.ip);
        if self.inner.limiter.fail(&addr, &limit::ADDRESS) {
            tracing::warn!(addr, "auth lockout after repeated failures");
            self.log("lockout", None, meta, &addr);
        }
        self.log("enroll_failed", None, meta, detail);
        AuthError::InvalidLink
    }

    /// The pending link with this secret, if there is one.
    fn pending_link(&self, token: &str) -> Result<Option<EnrollmentRecord>> {
        let token = token.trim();
        if token.len() != 43 {
            return Ok(None);
        }
        let hash = secret::sha256(token.as_bytes());
        let conn = self.db();
        let id: Option<String> = conn
            .query_row(
                "SELECT id FROM enrollments WHERE secret_hash = ?1",
                [hash.as_slice()],
                |r| r.get(0),
            )
            .optional()?;
        let Some(id) = id else { return Ok(None) };
        Ok(record(&conn, &id, self.now())?.filter(|r| r.state == EnrollmentState::Pending))
    }

    /// A pending link for the enrollment endpoints, or the refusal (rate-limited, audited).
    fn usable_link(&self, token: &str, meta: &ClientMeta) -> Result<EnrollmentRecord> {
        self.limit_address(meta)?;
        let Some(rec) = self.pending_link(token)? else {
            return Err(self.bad_link(meta, "wrong, used, revoked or expired link"));
        };
        if rec.kind == EnrollmentKind::Recovery {
            let disabled = rec
                .user_id
                .as_deref()
                .map(|id| store::user_by_id(&self.db(), id))
                .transpose()?
                .flatten()
                .is_none_or(|u| u.disabled);
            if disabled {
                return Err(AuthError::Conflict(
                    "this account is disabled; ask a server admin to enable it first".into(),
                ));
            }
        }
        Ok(rec)
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_link(
        &self,
        actor: &Principal,
        kind: EnrollmentKind,
        role: Option<Role>,
        spaces: &[(SpaceId, SpaceRole)],
        user_id: Option<&str>,
        days: Option<u32>,
        note: Option<&str>,
        meta: &ClientMeta,
    ) -> Result<IssuedLink> {
        let lifetime = link_lifetime(days)?;
        let secret = secret::token256();
        let id = secret::public_id();
        let now = self.now();
        let creator = (!matches!(actor.credential, crate::Credential::Local))
            .then_some(actor.user.id.as_str());
        let mut conn = self.db();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM enrollments WHERE COALESCE(used_at, revoked_at, expires_at) < ?1",
            [now - ms(KEEP_FINISHED)],
        )?;
        if kind == EnrollmentKind::Recovery {
            // One recovery link per user: a new one replaces any still waiting.
            tx.execute(
                "UPDATE enrollments SET revoked_at = ?2 WHERE kind = 'recovery' AND user_id = ?1
                 AND used_at IS NULL AND revoked_at IS NULL",
                params![user_id, now],
            )?;
        }
        tx.execute(
            "INSERT INTO enrollments (id, kind, secret_hash, created_by, created_at, expires_at,
                 role, user_id, note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                kind_str(kind),
                secret::sha256(secret.as_bytes()).as_slice(),
                creator,
                now,
                now + lifetime,
                role.map(store::role_str),
                user_id,
                note
            ],
        )?;
        for (space, r) in spaces {
            tx.execute(
                "INSERT INTO enrollment_spaces (enrollment_id, space_id, role) VALUES (?1, ?2, ?3)",
                params![id, space, r.as_str()],
            )?;
        }
        let detail = match kind {
            EnrollmentKind::Invitation => format!(
                "{id}: role {}, spaces {}",
                role.map_or("user", store::role_str),
                spaces
                    .iter()
                    .map(|(s, r)| format!("{s}={}", r.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            EnrollmentKind::Recovery => format!("{id}: for {}", user_id.unwrap_or_default()),
        };
        let event = match kind {
            EnrollmentKind::Invitation => "invitation_created",
            EnrollmentKind::Recovery => "recovery_issued",
        };
        self.log_with(&tx, event, creator, meta, &detail);
        let rec = record(&tx, &id, now)?.ok_or_else(|| internal("link vanished"))?;
        tx.commit()?;
        Ok(IssuedLink {
            secret,
            record: rec,
        })
    }

    /// Invite someone. A server admin may give any instance role and any spaces; anyone
    /// else only the `user` role, and only spaces they administer. Needs a browser session
    /// that signed in recently, as creating an API token does.
    pub fn create_invitation(
        &self,
        actor: &Principal,
        role: Role,
        spaces: &[(SpaceId, SpaceRole)],
        days: Option<u32>,
        note: Option<&str>,
        meta: &ClientMeta,
    ) -> Result<IssuedLink> {
        let mut seen = std::collections::HashSet::new();
        if !spaces.iter().all(|(s, _)| seen.insert(*s)) {
            return Err(AuthError::InvalidRequest("a space is listed twice".into()));
        }
        if !actor.server_admin() {
            if role != Role::User {
                return Err(AuthError::Forbidden(
                    "only a server admin can choose an invitation's server role",
                ));
            }
            if spaces.is_empty() {
                return Err(AuthError::InvalidRequest(
                    "choose the space to invite into".into(),
                ));
            }
            for (space, _) in spaces {
                if self.space_role(actor, *space)? != Some(SpaceRole::Admin) {
                    return Err(AuthError::Forbidden(
                        "you can only invite into spaces you administer",
                    ));
                }
            }
        }
        self.require_recent(actor)?;
        let note = clean_note(note)?;
        self.insert_link(
            actor,
            EnrollmentKind::Invitation,
            Some(role),
            spaces,
            None,
            days,
            note.as_deref(),
            meta,
        )
    }

    /// A recovery link for a user who lost their credentials. Server admins only, with a
    /// recent sign-in. It replaces any earlier recovery link for the same user.
    pub fn create_recovery(
        &self,
        actor: &Principal,
        user_id: &str,
        days: Option<u32>,
        meta: &ClientMeta,
    ) -> Result<IssuedLink> {
        if !actor.server_admin() {
            return Err(ADMINS_ONLY);
        }
        self.require_recent(actor)?;
        if store::user_by_id(&self.db(), user_id)?.is_none() {
            return Err(AuthError::NotFound);
        }
        self.insert_link(
            actor,
            EnrollmentKind::Recovery,
            None,
            &[],
            Some(user_id),
            days,
            None,
            meta,
        )
    }

    /// Every invitation and recovery link, newest first (server admins), or with `space`,
    /// the invitations into that space, each showing only that space (its admins).
    pub fn enrollments(
        &self,
        actor: &Principal,
        space: Option<SpaceId>,
    ) -> Result<Vec<EnrollmentRecord>> {
        match space {
            None if !actor.server_admin() => return Err(ADMINS_ONLY),
            Some(s) if self.space_role(actor, s)? != Some(SpaceRole::Admin) => {
                return Err(AuthError::Forbidden(
                    "only an admin of this space can see its invitations",
                ));
            }
            _ => {}
        }
        let now = self.now();
        let conn = self.db();
        let mut stmt = conn.prepare(&format!("{RECORD_SQL} ORDER BY e.created_at DESC"))?;
        let all = stmt
            .query_map([], |r| record_row(&conn, r, now))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(match space {
            None => all,
            Some(s) => all
                .into_iter()
                .filter(|r| r.kind == EnrollmentKind::Invitation)
                .filter_map(|mut r| {
                    r.spaces.retain(|(id, _)| *id == s);
                    (!r.spaces.is_empty()).then_some(r)
                })
                .collect(),
        })
    }

    /// Revoke a link that hasn't been used. Server admins may revoke any; a space admin an
    /// invitation that gives only the `user` role and only spaces they administer.
    pub fn revoke_enrollment(&self, actor: &Principal, id: &str, meta: &ClientMeta) -> Result<()> {
        let now = self.now();
        let rec = record(&self.db(), id, now)?.ok_or(AuthError::NotFound)?;
        if !actor.server_admin() {
            let mut administered = 0;
            for (space, _) in &rec.spaces {
                if self.space_role(actor, *space)? == Some(SpaceRole::Admin) {
                    administered += 1;
                }
            }
            if rec.kind != EnrollmentKind::Invitation || administered == 0 {
                return Err(AuthError::NotFound);
            }
            if administered < rec.spaces.len() || rec.role != Some(Role::User) {
                return Err(AuthError::Forbidden(
                    "this invitation also grants what only a server admin can revoke",
                ));
            }
        }
        match rec.state {
            EnrollmentState::Pending => {}
            EnrollmentState::Used => {
                return Err(AuthError::Conflict("it was already used".into()));
            }
            EnrollmentState::Revoked => {
                return Err(AuthError::Conflict("it was already revoked".into()));
            }
            EnrollmentState::Expired => {
                return Err(AuthError::Conflict("it has already expired".into()));
            }
        }
        let conn = self.db();
        conn.execute(
            "UPDATE enrollments SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
            params![id, now],
        )?;
        let user = (!matches!(actor.credential, crate::Credential::Local))
            .then_some(actor.user.id.as_str());
        self.log_with(&conn, "enrollment_revoked", user, meta, id);
        Ok(())
    }

    // ----- Redeeming a link -----

    /// What a link grants, for the page that opens it. Rate-limited and audited: a wrong,
    /// used, revoked or expired link is a failed attempt.
    pub fn enroll_info(&self, token: &str, meta: &ClientMeta) -> Result<EnrollmentRecord> {
        self.usable_link(token, meta)
    }

    /// Start registering a passkey with a link. An invitation takes the username the person
    /// picks; a recovery link is for its user, whatever `username` says.
    pub fn start_enroll(
        &self,
        host: &str,
        token: &str,
        username: &str,
        display_name: Option<&str>,
        meta: &ClientMeta,
    ) -> Result<CeremonyStart> {
        let rec = self.usable_link(token, meta)?;
        let rp = self.rp(host)?;
        let (user_id, username, display_name) = self.enrollee(&rec, username, display_name)?;
        let (ccr, reg) = rp
            .webauthn
            .start_passkey_registration(user_id, &username, &display_name, None)
            .map_err(internal)?;
        let options = registration_options(&ccr)?;
        let ceremony_id = self.store_ceremony(
            &rp.id,
            Pending::Enroll {
                reg,
                enrollment: rec.id,
                user_id,
                username,
                display_name,
            },
        )?;
        Ok(CeremonyStart {
            ceremony_id,
            options,
        })
    }

    /// The user a link enrolls: a new one for an invitation (with a username that must be
    /// free), the link's own for recovery.
    fn enrollee(
        &self,
        rec: &EnrollmentRecord,
        username: &str,
        display_name: Option<&str>,
    ) -> Result<(Uuid, String, String)> {
        match rec.kind {
            EnrollmentKind::Invitation => {
                let username = normalize_username(username)?;
                let display_name = optional_label(display_name, &username, "display names")?;
                if store::user_by_name(&self.db(), &username)?.is_some() {
                    return Err(AuthError::Conflict("that username is taken".into()));
                }
                Ok((Uuid::new_v4(), username, display_name))
            }
            EnrollmentKind::Recovery => {
                let id = rec.user_id.as_deref().ok_or(AuthError::InvalidLink)?;
                let user = store::user_by_id(&self.db(), id)?.ok_or(AuthError::InvalidLink)?;
                let uuid = Uuid::parse_str(&user.id).map_err(internal)?;
                Ok((uuid, user.name, user.display_name))
            }
        }
    }

    /// Finish a link's passkey registration: the user exists (or has new credentials) and
    /// is signed in. `replaces` is the browser's current session, if any, which ends.
    pub fn finish_enroll(
        &self,
        host: &str,
        ceremony_id: &str,
        credential: &Value,
        passkey_name: Option<&str>,
        meta: &ClientMeta,
        replaces: Option<&Principal>,
    ) -> Result<NewSession> {
        self.limit_address(meta)?;
        let rp = self.rp(host)?;
        let Pending::Enroll {
            reg,
            enrollment,
            user_id,
            username,
            display_name,
        } = self.take_ceremony(ceremony_id, &rp.id)?
        else {
            return Err(AuthError::InvalidRequest(
                "not an enrollment ceremony".into(),
            ));
        };
        let name = optional_label(passkey_name, "Passkey", "passkey names")?;
        let cred: RegisterPublicKeyCredential = parse(credential)?;
        let passkey = rp
            .webauthn
            .finish_passkey_registration(&cred, &reg)
            .map_err(|e| self.bad_link(meta, &format!("registration failed: {e}")))?;
        self.complete_enrollment(
            &enrollment,
            &user_id.to_string(),
            username,
            display_name,
            &Completion::Passkey {
                rp_id: rp.id.clone(),
                passkey: Box::new(passkey),
                name,
            },
            meta,
            replaces,
        )
    }

    /// Enroll with a password instead of a passkey ([`PasswordMode::Fallback`] only). The
    /// password is hashed here, so async callers should run this on a blocking thread.
    pub fn enroll_with_password(
        &self,
        token: &str,
        username: &str,
        display_name: Option<&str>,
        password: &str,
        meta: &ClientMeta,
        replaces: Option<&Principal>,
    ) -> Result<NewSession> {
        if self.inner.password_mode != PasswordMode::Fallback {
            return Err(AuthError::Unavailable(
                "password sign-in is disabled on this instance; use a passkey".into(),
            ));
        }
        let rec = self.usable_link(token, meta)?;
        let (user_id, username, display_name) = self.enrollee(&rec, username, display_name)?;
        let hash = self.hash_password(password)?;
        self.complete_enrollment(
            &rec.id,
            &user_id.to_string(),
            username,
            display_name,
            &Completion::Password { hash },
            meta,
            replaces,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn complete_enrollment(
        &self,
        enrollment: &str,
        user_id: &str,
        username: String,
        display_name: String,
        how: &Completion,
        meta: &ClientMeta,
        replaces: Option<&Principal>,
    ) -> Result<NewSession> {
        let now = self.now();
        let mut conn = self.db();
        let tx = conn.transaction()?;
        // Checked again inside the transaction: the link may have been used, revoked or
        // expired since the ceremony started.
        let Some(rec) =
            record(&tx, enrollment, now)?.filter(|r| r.state == EnrollmentState::Pending)
        else {
            drop(tx);
            drop(conn);
            return Err(self.bad_link(meta, "link stopped working during enrollment"));
        };
        let (password_hash, method) = match how {
            Completion::Passkey { .. } => (None, SessionMethod::Passkey),
            Completion::Password { hash } => (Some(hash.clone()), SessionMethod::Password),
        };
        let mut ended_sessions = false;
        match rec.kind {
            EnrollmentKind::Invitation => {
                let user = UserRow {
                    id: user_id.to_owned(),
                    name: username,
                    display_name,
                    role: rec.role.unwrap_or(Role::User),
                    password_hash,
                    disabled: false,
                };
                match store::insert_user(&tx, &user, now) {
                    Ok(()) => {}
                    Err(e) if is_constraint(&e) => {
                        return Err(AuthError::Conflict("that username is taken".into()));
                    }
                    Err(e) => return Err(e.into()),
                }
                for (space, role) in &rec.spaces {
                    tx.execute(
                        "INSERT INTO space_members (user_id, space_id, role) VALUES (?1, ?2, ?3)",
                        params![user_id, space, role.as_str()],
                    )?;
                }
                if let Some((first, _)) = rec.spaces.first() {
                    tx.execute(
                        "UPDATE users SET last_space_id = ?2 WHERE id = ?1",
                        params![user_id, first],
                    )?;
                }
            }
            EnrollmentKind::Recovery => {
                let user = rec
                    .user_id
                    .as_deref()
                    .filter(|u| *u == user_id)
                    .map(|u| store::user_by_id(&tx, u))
                    .transpose()?
                    .flatten()
                    .filter(|u| !u.disabled)
                    .ok_or(AuthError::InvalidLink)?;
                // The old credentials are lost (or worse): they go, and so does every
                // session they opened.
                tx.execute("DELETE FROM passkeys WHERE user_id = ?1", [&user.id])?;
                store::set_password_hash(&tx, &user.id, password_hash.as_deref())?;
                ended_sessions =
                    tx.execute("DELETE FROM sessions WHERE user_id = ?1", [&user.id])? > 0;
            }
        }
        if let Completion::Passkey {
            rp_id,
            passkey,
            name,
        } = how
        {
            let row = PasskeyRow {
                id: secret::public_id(),
                user_id: user_id.to_owned(),
                rp_id: rp_id.clone(),
                name: name.clone(),
                passkey: serde_json::to_string(passkey.as_ref()).map_err(internal)?,
                created_at: now,
                last_used_at: None,
            };
            match store::insert_passkey(&tx, &row, passkey.cred_id().as_ref()) {
                Ok(()) => {}
                Err(e) if is_constraint(&e) => {
                    return Err(AuthError::Conflict(
                        "that passkey is already registered".into(),
                    ));
                }
                Err(e) => return Err(e.into()),
            }
        }
        let used = tx.execute(
            "UPDATE enrollments SET used_at = ?2, user_id = ?3
             WHERE id = ?1 AND used_at IS NULL AND revoked_at IS NULL",
            params![enrollment, now, user_id],
        )?;
        if used != 1 {
            return Err(AuthError::InvalidLink);
        }
        let session = self.new_session(&tx, user_id, method, false, meta, replaces)?;
        let (event, detail) = match rec.kind {
            EnrollmentKind::Invitation => ("enrolled", format!("invitation {enrollment}")),
            EnrollmentKind::Recovery => ("recovered", format!("recovery link {enrollment}")),
        };
        let detail = match how {
            Completion::Passkey { .. } => format!("{detail}, passkey"),
            Completion::Password { .. } => format!("{detail}, password"),
        };
        self.log_with(&tx, event, Some(user_id), meta, &detail);
        tx.commit()?;
        drop(conn);
        if ended_sessions {
            self.bump_revocations();
        }
        self.inner.limiter.succeed(&limit::address_key(meta.ip));
        tracing::info!(user = %user_id, kind = event, "enrollment link used");
        Ok(session)
    }

    // ----- Joining with an existing account -----

    /// Accept an invitation into the caller's own account instead of creating a user: they
    /// get its spaces, each with the higher of the role they have and the role it gives.
    /// The instance role never changes, so an invitation that makes a server admin can't
    /// be joined this way. Needs a browser session; rate-limited, one-time and audited like
    /// the other ways of using a link. An invitation that would grant nothing new is
    /// refused and stays usable. Returns what changed.
    pub fn accept_invitation(
        &self,
        actor: &Principal,
        token: &str,
        meta: &ClientMeta,
    ) -> Result<Vec<(SpaceId, SpaceRole)>> {
        if actor.session().is_none() || actor.pending_second_factor() {
            return Err(AuthError::Forbidden(
                "join an invitation from a signed-in browser",
            ));
        }
        let rec = self.usable_link(token, meta)?;
        if !joinable(&rec) {
            return Err(match rec.kind {
                EnrollmentKind::Recovery => AuthError::InvalidRequest(
                    "a recovery link can't be joined; it is for its own account".into(),
                ),
                EnrollmentKind::Invitation => AuthError::Conflict(
                    "this invitation makes a new server admin, so it can only create a new \
                     account; a server admin promotes existing users on the Users page"
                        .into(),
                ),
            });
        }
        let user_id = actor.user.id.as_str();
        let now = self.now();
        let mut conn = self.db();
        let tx = conn.transaction()?;
        // Checked again inside the transaction, as for enrolling.
        let Some(rec) = record(&tx, &rec.id, now)?.filter(|r| r.state == EnrollmentState::Pending)
        else {
            drop(tx);
            drop(conn);
            return Err(self.bad_link(meta, "link stopped working while joining"));
        };
        let mut granted = Vec::new();
        for &(space, role) in &rec.spaces {
            let have: Option<String> = tx
                .query_row(
                    "SELECT role FROM space_members WHERE user_id = ?1 AND space_id = ?2",
                    params![user_id, space],
                    |r| r.get(0),
                )
                .optional()?;
            if have.as_deref().and_then(SpaceRole::parse) >= Some(role) {
                continue;
            }
            tx.execute(
                "INSERT INTO space_members (user_id, space_id, role) VALUES (?1, ?2, ?3)
                 ON CONFLICT (user_id, space_id) DO UPDATE SET role = excluded.role",
                params![user_id, space, role.as_str()],
            )?;
            granted.push((space, role));
        }
        if granted.is_empty() {
            return Err(AuthError::Conflict(
                "you already have everything this invitation gives; it is still unused, so \
                 it can go to someone else"
                    .into(),
            ));
        }
        tx.execute(
            "UPDATE users SET last_space_id = ?2 WHERE id = ?1",
            params![user_id, granted[0].0],
        )?;
        let used = tx.execute(
            "UPDATE enrollments SET used_at = ?2, user_id = ?3
             WHERE id = ?1 AND used_at IS NULL AND revoked_at IS NULL",
            params![rec.id, now, user_id],
        )?;
        if used != 1 {
            return Err(AuthError::InvalidLink);
        }
        let detail = format!(
            "invitation {}: {}",
            rec.id,
            granted
                .iter()
                .map(|(s, r)| format!("{s}={}", r.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        );
        self.log_with(&tx, "joined", Some(user_id), meta, &detail);
        tx.commit()?;
        drop(conn);
        self.inner.limiter.succeed(&limit::address_key(meta.ip));
        tracing::info!(user = %user_id, "invitation joined with an existing account");
        Ok(granted)
    }

    // ----- Managing users -----

    /// Every user, by name, with what server admins need to manage them.
    pub fn managed_users(&self, actor: &Principal) -> Result<Vec<UserRecord>> {
        if !actor.server_admin() {
            return Err(ADMINS_ONLY);
        }
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT u.id, u.name, u.display_name, u.role, u.password_hash IS NOT NULL,
                 u.disabled_at IS NOT NULL, u.created_at,
                 (SELECT COUNT(*) FROM passkeys p WHERE p.user_id = u.id),
                 (SELECT COUNT(*) FROM api_tokens t WHERE t.user_id = u.id),
                 (SELECT MAX(s.last_seen_at) FROM sessions s
                  WHERE s.user_id = u.id AND s.pending_second_factor = 0)
             FROM users u ORDER BY u.name",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(UserRecord {
                    info: UserInfo {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        display_name: r.get(2)?,
                        role: store::role_from(&r.get::<_, String>(3)?),
                        has_password: r.get(4)?,
                    },
                    disabled: r.get(5)?,
                    created_at: r.get(6)?,
                    passkeys: r.get(7)?,
                    tokens: r.get(8)?,
                    last_seen_at: r.get(9)?,
                    spaces: Vec::new(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|mut u| {
                u.spaces = space_roles(
                    &conn,
                    "SELECT space_id, role FROM space_members WHERE user_id = ?1 ORDER BY space_id",
                    &u.info.id,
                )?;
                Ok(u)
            })
            .collect()
    }

    /// Checks shared by changing, disabling and deleting a user: a server admin with a
    /// recent sign-in, acting on someone else who exists.
    fn manageable(&self, actor: &Principal, user_id: &str, what: &'static str) -> Result<UserRow> {
        if !actor.server_admin() {
            return Err(ADMINS_ONLY);
        }
        self.require_recent(actor)?;
        if actor.user.id == user_id {
            return Err(AuthError::Forbidden(what));
        }
        store::user_by_id(&self.db(), user_id)?.ok_or(AuthError::NotFound)
    }

    fn last_admin(&self, user: &UserRow) -> Result<bool> {
        Ok(user.role == Role::Admin && !user.disabled && other_admins(&self.db(), &user.id)? == 0)
    }

    /// Change a user's instance role (`admin` is a server admin). Not your own, and never
    /// the last server admin's.
    pub fn set_user_role(
        &self,
        actor: &Principal,
        user_id: &str,
        role: Role,
        meta: &ClientMeta,
    ) -> Result<()> {
        let user = self.manageable(actor, user_id, "you can't change your own role")?;
        if user.role == role {
            return Ok(());
        }
        if role != Role::Admin && self.last_admin(&user)? {
            return Err(AuthError::Conflict(
                "that is the last server admin; make someone else one first".into(),
            ));
        }
        let conn = self.db();
        conn.execute(
            "UPDATE users SET role = ?2 WHERE id = ?1",
            params![user_id, store::role_str(role)],
        )?;
        self.log_with(
            &conn,
            "user_role_changed",
            Some(&actor.user.id),
            meta,
            &format!("{}: {}", user.name, store::role_str(role)),
        );
        drop(conn);
        // Open sockets re-check who they belong to.
        self.bump_revocations();
        Ok(())
    }

    /// Disable or enable a user. Disabling ends their sessions at once and suspends their
    /// API tokens. Not yourself, and never the last enabled server admin.
    pub fn set_user_disabled(
        &self,
        actor: &Principal,
        user_id: &str,
        disabled: bool,
        meta: &ClientMeta,
    ) -> Result<()> {
        let user = self.manageable(actor, user_id, "you can't disable yourself")?;
        if user.disabled == disabled {
            return Ok(());
        }
        if disabled && self.last_admin(&user)? {
            return Err(AuthError::Conflict(
                "that is the last server admin; it can't be disabled".into(),
            ));
        }
        let conn = self.db();
        if disabled {
            conn.execute(
                "UPDATE users SET disabled_at = ?2 WHERE id = ?1",
                params![user_id, self.now()],
            )?;
            conn.execute("DELETE FROM sessions WHERE user_id = ?1", [user_id])?;
        } else {
            conn.execute(
                "UPDATE users SET disabled_at = NULL WHERE id = ?1",
                [user_id],
            )?;
        }
        self.log_with(
            &conn,
            if disabled {
                "user_disabled"
            } else {
                "user_enabled"
            },
            Some(&actor.user.id),
            meta,
            &user.name,
        );
        drop(conn);
        if disabled {
            self.bump_revocations();
        }
        Ok(())
    }

    /// Delete a user with their passkeys, sessions, tokens and memberships. Not yourself,
    /// and never the last server admin.
    pub fn delete_user(&self, actor: &Principal, user_id: &str, meta: &ClientMeta) -> Result<()> {
        let user = self.manageable(actor, user_id, "you can't delete yourself")?;
        if user.role == Role::Admin && other_admins(&self.db(), &user.id)? == 0 {
            return Err(AuthError::Conflict(
                "that is the last server admin; it can't be deleted".into(),
            ));
        }
        let conn = self.db();
        conn.execute("DELETE FROM users WHERE id = ?1", [user_id])?;
        self.log_with(
            &conn,
            "user_deleted",
            Some(&actor.user.id),
            meta,
            &format!("{} ({})", user.name, user.id),
        );
        drop(conn);
        self.bump_revocations();
        Ok(())
    }
}

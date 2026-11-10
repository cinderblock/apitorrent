//! Row-level access to `auth.db`.

use rusqlite::{Connection, OptionalExtension, params};

use apitorrent_types::auth::{
    ApiTokenInfo, PasskeyInfo, Role, SessionInfo, SessionMethod, TokenScope, UserInfo,
};

use crate::secret::Digest32;

pub(crate) fn role_str(r: Role) -> &'static str {
    match r {
        Role::Admin => "admin",
        Role::User => "user",
        Role::Viewer => "viewer",
    }
}

pub(crate) fn role_from(s: &str) -> Role {
    match s {
        "admin" => Role::Admin,
        "user" => Role::User,
        // Unknown roles get the least access.
        _ => Role::Viewer,
    }
}

pub(crate) fn method_str(m: SessionMethod) -> &'static str {
    match m {
        SessionMethod::Passkey => "passkey",
        SessionMethod::Password => "password",
        SessionMethod::PasskeyAndPassword => "passkey+password",
    }
}

fn method_from(s: &str) -> SessionMethod {
    match s {
        "passkey" => SessionMethod::Passkey,
        "passkey+password" => SessionMethod::PasskeyAndPassword,
        _ => SessionMethod::Password,
    }
}

pub(crate) fn scope_str(s: TokenScope) -> &'static str {
    match s {
        TokenScope::Read => "read",
        TokenScope::Full => "full",
    }
}

fn scope_from(s: &str) -> TokenScope {
    if s == "full" {
        TokenScope::Full
    } else {
        TokenScope::Read
    }
}

#[derive(Debug, Clone)]
pub(crate) struct UserRow {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub role: Role,
    pub password_hash: Option<String>,
    /// Disabled users can't sign in, and their sessions and tokens don't work.
    pub disabled: bool,
}

impl UserRow {
    pub(crate) fn info(&self) -> UserInfo {
        UserInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            display_name: self.display_name.clone(),
            role: self.role,
            has_password: self.password_hash.is_some(),
        }
    }
}

const USER_COLS: &str = "id, name, display_name, role, password_hash, disabled_at IS NOT NULL";

fn user_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<UserRow> {
    Ok(UserRow {
        id: r.get(0)?,
        name: r.get(1)?,
        display_name: r.get(2)?,
        role: role_from(&r.get::<_, String>(3)?),
        password_hash: r.get(4)?,
        disabled: r.get(5)?,
    })
}

pub(crate) fn user_count(c: &Connection) -> rusqlite::Result<i64> {
    c.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
}

pub(crate) fn insert_user(c: &Connection, u: &UserRow, now: i64) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO users (id, name, display_name, role, password_hash, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            u.id,
            u.name,
            u.display_name,
            role_str(u.role),
            u.password_hash,
            now
        ],
    )?;
    Ok(())
}

pub(crate) fn user_by_id(c: &Connection, id: &str) -> rusqlite::Result<Option<UserRow>> {
    c.query_row(
        &format!("SELECT {USER_COLS} FROM users WHERE id = ?1"),
        [id],
        user_row,
    )
    .optional()
}

pub(crate) fn user_by_name(c: &Connection, name: &str) -> rusqlite::Result<Option<UserRow>> {
    c.query_row(
        &format!("SELECT {USER_COLS} FROM users WHERE name = ?1"),
        [name],
        user_row,
    )
    .optional()
}

pub(crate) fn set_password_hash(
    c: &Connection,
    user: &str,
    hash: Option<&str>,
) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE users SET password_hash = ?2 WHERE id = ?1",
        params![user, hash],
    )?;
    Ok(())
}

#[derive(Debug, Clone)]
pub(crate) struct PasskeyRow {
    pub id: String,
    pub user_id: String,
    pub rp_id: String,
    pub name: String,
    pub passkey: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

impl PasskeyRow {
    pub(crate) fn info(&self) -> PasskeyInfo {
        PasskeyInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            rp_id: self.rp_id.clone(),
            created_at: self.created_at,
            last_used_at: self.last_used_at,
        }
    }
}

const PASSKEY_COLS: &str = "id, user_id, rp_id, name, passkey, created_at, last_used_at";

fn passkey_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PasskeyRow> {
    Ok(PasskeyRow {
        id: r.get(0)?,
        user_id: r.get(1)?,
        rp_id: r.get(2)?,
        name: r.get(3)?,
        passkey: r.get(4)?,
        created_at: r.get(5)?,
        last_used_at: r.get(6)?,
    })
}

pub(crate) fn insert_passkey(
    c: &Connection,
    row: &PasskeyRow,
    credential_id: &[u8],
) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO passkeys (id, user_id, credential_id, rp_id, name, passkey, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            row.id,
            row.user_id,
            credential_id,
            row.rp_id,
            row.name,
            row.passkey,
            row.created_at
        ],
    )?;
    Ok(())
}

pub(crate) fn passkey_by_credential(
    c: &Connection,
    credential_id: &[u8],
) -> rusqlite::Result<Option<PasskeyRow>> {
    c.query_row(
        &format!("SELECT {PASSKEY_COLS} FROM passkeys WHERE credential_id = ?1"),
        [credential_id],
        passkey_row,
    )
    .optional()
}

pub(crate) fn passkeys_for_user(
    c: &Connection,
    user: &str,
    rp_id: Option<&str>,
) -> rusqlite::Result<Vec<PasskeyRow>> {
    let mut stmt = c.prepare(&format!(
        "SELECT {PASSKEY_COLS} FROM passkeys
         WHERE user_id = ?1 AND (?2 IS NULL OR rp_id = ?2) ORDER BY created_at"
    ))?;
    stmt.query_map(params![user, rp_id], passkey_row)?.collect()
}

pub(crate) fn update_passkey_use(
    c: &Connection,
    id: &str,
    passkey: Option<&str>,
    now: i64,
) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE passkeys SET last_used_at = ?2, passkey = COALESCE(?3, passkey) WHERE id = ?1",
        params![id, now, passkey],
    )?;
    Ok(())
}

pub(crate) fn rename_passkey(
    c: &Connection,
    user: &str,
    id: &str,
    name: &str,
) -> rusqlite::Result<bool> {
    Ok(c.execute(
        "UPDATE passkeys SET name = ?3 WHERE id = ?1 AND user_id = ?2",
        params![id, user, name],
    )? == 1)
}

pub(crate) fn delete_passkey(c: &Connection, user: &str, id: &str) -> rusqlite::Result<bool> {
    Ok(c.execute(
        "DELETE FROM passkeys WHERE id = ?1 AND user_id = ?2",
        params![id, user],
    )? == 1)
}

#[derive(Debug, Clone)]
pub(crate) struct SessionRow {
    pub public_id: String,
    pub user_id: String,
    pub method: SessionMethod,
    pub pending_second_factor: bool,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub authenticated_at: i64,
    pub expires_at: i64,
    pub user_agent: Option<String>,
    pub ip: Option<String>,
}

const SESSION_COLS: &str = "public_id, user_id, method, pending_second_factor, created_at, \
                            last_seen_at, authenticated_at, expires_at, user_agent, ip";

fn session_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        public_id: r.get(0)?,
        user_id: r.get(1)?,
        method: method_from(&r.get::<_, String>(2)?),
        pending_second_factor: r.get(3)?,
        created_at: r.get(4)?,
        last_seen_at: r.get(5)?,
        authenticated_at: r.get(6)?,
        expires_at: r.get(7)?,
        user_agent: r.get(8)?,
        ip: r.get(9)?,
    })
}

pub(crate) fn insert_session(
    c: &Connection,
    hash: &Digest32,
    s: &SessionRow,
) -> rusqlite::Result<()> {
    c.execute(
        &format!("INSERT INTO sessions (id_hash, {SESSION_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"),
        params![
            hash.as_slice(),
            s.public_id,
            s.user_id,
            method_str(s.method),
            s.pending_second_factor,
            s.created_at,
            s.last_seen_at,
            s.authenticated_at,
            s.expires_at,
            s.user_agent,
            s.ip
        ],
    )?;
    Ok(())
}

pub(crate) fn session_by_hash(
    c: &Connection,
    hash: &Digest32,
) -> rusqlite::Result<Option<SessionRow>> {
    c.query_row(
        &format!("SELECT {SESSION_COLS} FROM sessions WHERE id_hash = ?1"),
        [hash.as_slice()],
        session_row,
    )
    .optional()
}

pub(crate) fn touch_session(c: &Connection, hash: &Digest32, now: i64) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE sessions SET last_seen_at = ?2 WHERE id_hash = ?1",
        params![hash.as_slice(), now],
    )?;
    Ok(())
}

pub(crate) fn delete_session(c: &Connection, hash: &Digest32) -> rusqlite::Result<()> {
    c.execute("DELETE FROM sessions WHERE id_hash = ?1", [hash.as_slice()])?;
    Ok(())
}

/// Drop sessions past their absolute expiry, or idle for longer than `idle_ms`.
pub(crate) fn delete_expired_sessions(
    c: &Connection,
    now: i64,
    idle_ms: i64,
) -> rusqlite::Result<()> {
    c.execute(
        "DELETE FROM sessions WHERE expires_at <= ?1 OR last_seen_at <= ?2",
        params![now, now.saturating_sub(idle_ms)],
    )?;
    Ok(())
}

pub(crate) fn sessions_for_user(c: &Connection, user: &str) -> rusqlite::Result<Vec<SessionRow>> {
    let mut stmt = c.prepare(&format!(
        "SELECT {SESSION_COLS} FROM sessions
         WHERE user_id = ?1 AND pending_second_factor = 0 ORDER BY last_seen_at DESC"
    ))?;
    stmt.query_map([user], session_row)?.collect()
}

pub(crate) fn delete_session_by_public_id(
    c: &Connection,
    user: &str,
    id: &str,
) -> rusqlite::Result<bool> {
    Ok(c.execute(
        "DELETE FROM sessions WHERE public_id = ?1 AND user_id = ?2",
        params![id, user],
    )? == 1)
}

impl SessionRow {
    pub(crate) fn info(&self, current: bool) -> SessionInfo {
        SessionInfo {
            id: self.public_id.clone(),
            method: self.method,
            created_at: self.created_at,
            last_seen_at: self.last_seen_at,
            expires_at: self.expires_at,
            user_agent: self.user_agent.clone(),
            ip: self.ip.clone(),
            current,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TokenRow {
    pub user_id: String,
    pub info: ApiTokenInfo,
}

const TOKEN_COLS: &str =
    "id, user_id, name, scope, prefix, created_at, expires_at, last_used_at, space_id";

fn token_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<TokenRow> {
    Ok(TokenRow {
        user_id: r.get(1)?,
        info: ApiTokenInfo {
            id: r.get(0)?,
            name: r.get(2)?,
            scope: scope_from(&r.get::<_, String>(3)?),
            prefix: r.get(4)?,
            created_at: r.get(5)?,
            expires_at: r.get(6)?,
            last_used_at: r.get(7)?,
            space: r.get(8)?,
        },
    })
}

pub(crate) fn insert_token(c: &Connection, t: &TokenRow, hash: &Digest32) -> rusqlite::Result<()> {
    let i = &t.info;
    c.execute(
        "INSERT INTO api_tokens (id, user_id, name, scope, token_hash, prefix, created_at, expires_at,
             space_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            i.id,
            t.user_id,
            i.name,
            scope_str(i.scope),
            hash.as_slice(),
            i.prefix,
            i.created_at,
            i.expires_at,
            i.space
        ],
    )?;
    Ok(())
}

pub(crate) fn token_by_hash(c: &Connection, hash: &Digest32) -> rusqlite::Result<Option<TokenRow>> {
    c.query_row(
        &format!("SELECT {TOKEN_COLS} FROM api_tokens WHERE token_hash = ?1"),
        [hash.as_slice()],
        token_row,
    )
    .optional()
}

pub(crate) fn touch_token(c: &Connection, id: &str, now: i64) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE api_tokens SET last_used_at = ?2 WHERE id = ?1",
        params![id, now],
    )?;
    Ok(())
}

pub(crate) fn tokens_for_user(c: &Connection, user: &str) -> rusqlite::Result<Vec<TokenRow>> {
    let mut stmt = c.prepare(&format!(
        "SELECT {TOKEN_COLS} FROM api_tokens WHERE user_id = ?1 ORDER BY created_at"
    ))?;
    stmt.query_map([user], token_row)?.collect()
}

pub(crate) fn delete_token(c: &Connection, user: &str, id: &str) -> rusqlite::Result<bool> {
    Ok(c.execute(
        "DELETE FROM api_tokens WHERE id = ?1 AND user_id = ?2",
        params![id, user],
    )? == 1)
}

/// Append to the audit log.
pub(crate) fn event(
    c: &Connection,
    now: i64,
    kind: &str,
    user: Option<&str>,
    ip: Option<&str>,
    detail: &str,
) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO auth_events (at, kind, user_id, ip, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![now, kind, user, ip, detail],
    )?;
    Ok(())
}

pub(crate) fn prune_events(c: &Connection, before: i64) -> rusqlite::Result<()> {
    c.execute("DELETE FROM auth_events WHERE at < ?1", [before])?;
    Ok(())
}

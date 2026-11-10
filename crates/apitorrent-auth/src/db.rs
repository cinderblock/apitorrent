//! `auth.db`: SQLite in WAL mode with embedded, forward-only migrations.
//!
//! Kept apart from the instance database so credentials can be backed up, restored or wiped
//! on their own, and so the two schemas evolve independently.

use std::path::Path;

use rusqlite::Connection;

/// Each entry upgrades the schema by one version. Never edit a released migration; add a
/// new one. Times are milliseconds since the Unix epoch.
const MIGRATIONS: &[&str] = &[
    // 1: users, passkeys, sessions, API tokens, audit events.
    "CREATE TABLE users (
         id TEXT PRIMARY KEY,                 -- UUID; also the WebAuthn user handle
         name TEXT NOT NULL UNIQUE,           -- normalised login name
         display_name TEXT NOT NULL,
         role TEXT NOT NULL CHECK (role IN ('admin', 'user', 'viewer')),
         password_hash TEXT,                  -- argon2id PHC string
         created_at INTEGER NOT NULL
     );
     CREATE TABLE passkeys (
         id TEXT PRIMARY KEY,
         user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
         credential_id BLOB NOT NULL UNIQUE,
         rp_id TEXT NOT NULL,
         name TEXT NOT NULL,
         passkey TEXT NOT NULL,               -- webauthn-rs Passkey, JSON
         created_at INTEGER NOT NULL,
         last_used_at INTEGER
     );
     CREATE INDEX passkeys_user ON passkeys(user_id);
     CREATE TABLE sessions (
         id_hash BLOB PRIMARY KEY,            -- SHA-256 of the cookie value
         public_id TEXT NOT NULL UNIQUE,
         user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
         method TEXT NOT NULL,
         pending_second_factor INTEGER NOT NULL DEFAULT 0,
         created_at INTEGER NOT NULL,
         last_seen_at INTEGER NOT NULL,
         authenticated_at INTEGER NOT NULL,
         expires_at INTEGER NOT NULL,
         user_agent TEXT,
         ip TEXT
     );
     CREATE INDEX sessions_user ON sessions(user_id);
     CREATE TABLE api_tokens (
         id TEXT PRIMARY KEY,
         user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
         name TEXT NOT NULL,
         scope TEXT NOT NULL CHECK (scope IN ('read', 'full')),
         token_hash BLOB NOT NULL UNIQUE,     -- SHA-256 of the token
         prefix TEXT NOT NULL,
         created_at INTEGER NOT NULL,
         expires_at INTEGER,
         last_used_at INTEGER
     );
     CREATE INDEX api_tokens_user ON api_tokens(user_id);
     CREATE TABLE auth_events (
         id INTEGER PRIMARY KEY,
         at INTEGER NOT NULL,
         kind TEXT NOT NULL,
         user_id TEXT,
         ip TEXT,
         detail TEXT
     );",
    // 2: spaces. Each user holds a role per space (`space_id` is the instance database's
    // space id; 1 is `default`), and every API token is bound to one space. Everyone keeps
    // their role in `default`, where everything was before, and so do their tokens.
    "CREATE TABLE space_members (
         user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
         space_id INTEGER NOT NULL,
         role TEXT NOT NULL CHECK (role IN ('admin', 'user', 'viewer')),
         PRIMARY KEY (user_id, space_id)
     );
     INSERT INTO space_members (user_id, space_id, role) SELECT id, 1, role FROM users;
     ALTER TABLE api_tokens ADD COLUMN space_id INTEGER NOT NULL DEFAULT 1;
     ALTER TABLE users ADD COLUMN last_space_id INTEGER;",
];

pub(crate) fn open(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    let conn = init(conn)?;
    restrict_permissions(path);
    Ok(conn)
}

fn init(mut conn: Connection) -> rusqlite::Result<Connection> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // Deleted rows (revoked tokens, ended sessions) are overwritten, not left in free pages.
    conn.pragma_update(None, "secure_delete", "ON")?;
    migrate(&mut conn)?;
    Ok(conn)
}

fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let current = schema_version(conn)?;
    if current > MIGRATIONS.len() {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "auth database schema {current} is newer than this build supports ({})",
            MIGRATIONS.len()
        )));
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", i64::try_from(i + 1).expect("fits"))?;
        tx.commit()?;
        tracing::info!(version = i + 1, "auth database migrated");
    }
    Ok(())
}

pub(crate) fn schema_version(conn: &Connection) -> rusqlite::Result<usize> {
    let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    Ok(usize::try_from(v).unwrap_or(0))
}

/// Owner-only on Unix. On Windows the file inherits the data directory's ACL, which is
/// per-user under `%APPDATA%`.
#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
    let mut files = vec![path.to_path_buf()];
    if let Some(name) = name {
        files.push(path.with_file_name(format!("{name}-wal")));
        files.push(path.with_file_name(format!("{name}-shm")));
    }
    for p in files {
        if p.exists() {
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
        }
    }
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.db");
        let conn = open(&path).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), MIGRATIONS.len());
        drop(conn);
        let conn = open(&path).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), MIGRATIONS.len());
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn refuses_a_newer_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.db");
        let conn = open(&path).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        drop(conn);
        assert!(open(&path).is_err());
    }
}

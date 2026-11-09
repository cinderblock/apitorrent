//! What the host keeps on disk, under the plugin directory (`<data dir>/plugins`):
//!
//! - `plugins.db` (SQLite, WAL): installed plugins, their versions and grants, settings,
//!   each plugin's key-value data and its schedule.
//! - It also remembers which first-party plugins that ship with the daemon it has offered
//!   (installed for approval), so one an admin uninstalled isn't offered again.
//! - `bundles/<sha256>.js`: the code of each installed version, named by its hash. Content
//!   addressing keeps plugin ids and version strings out of file names (Windows reserves
//!   names like `con` and `nul`), and the hash is checked on every load.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use apitorrent_plugin_api::{PluginCapabilities, PluginManifest};

#[derive(Debug, thiserror::Error)]
pub(crate) enum StoreError {
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("stored data is corrupt: {0}")]
    Corrupt(String),
}

pub(crate) type Result<T> = std::result::Result<T, StoreError>;

const MIGRATIONS: &[&str] = &[
    "
    CREATE TABLE plugins (
        id TEXT PRIMARY KEY,
        enabled INTEGER NOT NULL DEFAULT 0,
        active_version TEXT,
        granted TEXT NOT NULL DEFAULT '{}',
        pending_version TEXT,
        settings TEXT NOT NULL DEFAULT '{}',
        error TEXT,
        installed_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE versions (
        plugin_id TEXT NOT NULL REFERENCES plugins(id) ON DELETE CASCADE,
        version TEXT NOT NULL,
        manifest TEXT NOT NULL,
        sha256 TEXT NOT NULL,
        installed_at INTEGER NOT NULL,
        PRIMARY KEY (plugin_id, version)
    );
    CREATE TABLE kv (
        plugin_id TEXT NOT NULL,
        key TEXT NOT NULL,
        value TEXT NOT NULL,
        PRIMARY KEY (plugin_id, key)
    ) WITHOUT ROWID;
    CREATE TABLE schedule (
        plugin_id TEXT NOT NULL,
        job TEXT NOT NULL,
        last_run INTEGER NOT NULL,
        PRIMARY KEY (plugin_id, job)
    ) WITHOUT ROWID;
",
    "
    CREATE TABLE offers (
        plugin_id TEXT PRIMARY KEY,
        version TEXT NOT NULL,
        offered_at INTEGER NOT NULL
    );
",
];

/// One installed plugin's row.
#[derive(Debug, Clone)]
pub(crate) struct PluginRow {
    pub id: String,
    pub enabled: bool,
    pub active_version: Option<String>,
    pub granted: PluginCapabilities,
    pub pending_version: Option<String>,
    pub settings: serde_json::Value,
    pub error: Option<String>,
    pub installed_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct VersionRow {
    pub manifest: PluginManifest,
    pub sha256: String,
}

#[derive(Debug)]
pub(crate) struct Store {
    conn: Mutex<Connection>,
    bundles: PathBuf,
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn json<T: serde::de::DeserializeOwned>(text: &str, what: &str) -> Result<T> {
    serde_json::from_str(text).map_err(|e| StoreError::Corrupt(format!("{what}: {e}")))
}

fn to_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("plugin data serialises")
}

impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir.join("bundles"))?;
        let conn = Connection::open(dir.join("plugins.db"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let version = usize::try_from(version).unwrap_or(usize::MAX);
        if version > MIGRATIONS.len() {
            return Err(StoreError::Corrupt(format!(
                "plugins.db is schema version {version}, newer than this build knows"
            )));
        }
        for (i, m) in MIGRATIONS.iter().enumerate().skip(version) {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(m)?;
            tx.pragma_update(
                None,
                "user_version",
                i64::try_from(i + 1).unwrap_or(i64::MAX),
            )?;
            tx.commit()?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
            bundles: dir.join("bundles"),
        })
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().expect("plugin store lock poisoned")
    }

    fn bundle_path(&self, sha256: &str) -> PathBuf {
        self.bundles.join(format!("{sha256}.js"))
    }

    /// Write a bundle's code (if not already there) and return its hash.
    pub fn put_code(&self, code: &str) -> Result<String> {
        let sha = sha256_hex(code.as_bytes());
        let path = self.bundle_path(&sha);
        if path.exists() && self.read_code(&sha).is_ok() {
            return Ok(sha);
        }
        let tmp = self.bundles.join(format!("{sha}.tmp"));
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(code.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        Ok(sha)
    }

    /// Read a bundle, checking its hash.
    pub fn read_code(&self, sha256: &str) -> Result<String> {
        let code = fs::read_to_string(self.bundle_path(sha256))?;
        if sha256_hex(code.as_bytes()) != sha256 {
            return Err(StoreError::Corrupt(format!(
                "bundle {sha256} doesn't match its hash"
            )));
        }
        Ok(code)
    }

    /// Delete bundle files no version refers to.
    pub fn collect_garbage(&self) -> Result<()> {
        let used: Vec<String> = {
            let db = self.db();
            let mut stmt = db.prepare("SELECT sha256 FROM versions")?;
            stmt.query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        for entry in fs::read_dir(&self.bundles)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let stem = name
                .strip_suffix(".js")
                .or_else(|| name.strip_suffix(".tmp"))
                .unwrap_or(&name);
            if !used.iter().any(|u| u == stem) {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(())
    }

    pub fn plugins(&self) -> Result<Vec<PluginRow>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT id, enabled, active_version, granted, pending_version, settings, error,
                    installed_at, updated_at FROM plugins ORDER BY id",
        )?;
        let raw = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, bool>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, i64>(8)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        raw.into_iter()
            .map(
                |(id, enabled, active, granted, pending, settings, error, ia, ua)| {
                    Ok(PluginRow {
                        granted: json(&granted, "granted capabilities")?,
                        settings: json(&settings, "settings")?,
                        id,
                        enabled,
                        active_version: active,
                        pending_version: pending,
                        error,
                        installed_at: ia,
                        updated_at: ua,
                    })
                },
            )
            .collect()
    }

    pub fn plugin(&self, id: &str) -> Result<Option<PluginRow>> {
        Ok(self.plugins()?.into_iter().find(|p| p.id == id))
    }

    pub fn version(&self, id: &str, version: &str) -> Result<Option<VersionRow>> {
        let db = self.db();
        let row: Option<(String, String)> = db
            .query_row(
                "SELECT manifest, sha256 FROM versions WHERE plugin_id = ?1 AND version = ?2",
                params![id, version],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        row.map(|(m, sha256)| {
            Ok(VersionRow {
                manifest: json(&m, "manifest")?,
                sha256,
            })
        })
        .transpose()
    }

    /// Record a version (its code must already be stored with [`Self::put_code`]).
    /// Creates the plugin row for a new plugin.
    pub fn add_version(&self, manifest: &PluginManifest, sha256: &str) -> Result<()> {
        let now = now_ms();
        let mut db = self.db();
        let tx = db.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO plugins (id, installed_at, updated_at) VALUES (?1, ?2, ?2)",
            params![manifest.id, now],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO versions (plugin_id, version, manifest, sha256, installed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                manifest.id,
                manifest.version,
                to_json(manifest),
                sha256,
                now
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Point the plugin at its active and pending versions, and drop every other version.
    pub fn set_versions(
        &self,
        id: &str,
        active: Option<&str>,
        granted: &PluginCapabilities,
        pending: Option<&str>,
    ) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        tx.execute(
            "UPDATE plugins SET active_version = ?2, granted = ?3, pending_version = ?4,
                    updated_at = ?5 WHERE id = ?1",
            params![id, active, to_json(granted), pending, now_ms()],
        )?;
        tx.execute(
            "DELETE FROM versions WHERE plugin_id = ?1
               AND version IS NOT ?2 AND version IS NOT ?3",
            params![id, active, pending],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        self.db().execute(
            "UPDATE plugins SET enabled = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, enabled, now_ms()],
        )?;
        Ok(())
    }

    pub fn set_error(&self, id: &str, error: Option<&str>) -> Result<()> {
        self.db().execute(
            "UPDATE plugins SET error = ?2 WHERE id = ?1",
            params![id, error],
        )?;
        Ok(())
    }

    pub fn set_settings(&self, id: &str, settings: &serde_json::Value) -> Result<()> {
        self.db().execute(
            "UPDATE plugins SET settings = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, to_json(settings), now_ms()],
        )?;
        Ok(())
    }

    /// Delete a plugin with all its versions, settings, data and schedule.
    pub fn remove(&self, id: &str) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        tx.execute("DELETE FROM kv WHERE plugin_id = ?1", [id])?;
        tx.execute("DELETE FROM schedule WHERE plugin_id = ?1", [id])?;
        tx.execute("DELETE FROM versions WHERE plugin_id = ?1", [id])?;
        tx.execute("DELETE FROM plugins WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    // Key-value data. Every call names the plugin; the ops take the id from the plugin's
    // own context, never from JavaScript.

    pub fn kv_get(&self, plugin: &str, key: &str) -> Result<Option<String>> {
        Ok(self
            .db()
            .query_row(
                "SELECT value FROM kv WHERE plugin_id = ?1 AND key = ?2",
                params![plugin, key],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Store a value unless that would take the plugin over `quota` bytes (keys plus
    /// values). Returns whether it was stored.
    pub fn kv_set(&self, plugin: &str, key: &str, value: &str, quota: usize) -> Result<bool> {
        let mut db = self.db();
        let tx = db.transaction()?;
        let others: i64 = tx.query_row(
            "SELECT COALESCE(SUM(length(CAST(key AS BLOB)) + length(CAST(value AS BLOB))), 0)
               FROM kv WHERE plugin_id = ?1 AND key != ?2",
            params![plugin, key],
            |r| r.get(0),
        )?;
        let total = usize::try_from(others).unwrap_or(usize::MAX) + key.len() + value.len();
        if total > quota {
            return Ok(false);
        }
        tx.execute(
            "INSERT OR REPLACE INTO kv (plugin_id, key, value) VALUES (?1, ?2, ?3)",
            params![plugin, key, value],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn kv_delete(&self, plugin: &str, key: &str) -> Result<bool> {
        Ok(self.db().execute(
            "DELETE FROM kv WHERE plugin_id = ?1 AND key = ?2",
            params![plugin, key],
        )? > 0)
    }

    /// Keys starting with `prefix`, in order, at most `limit`.
    pub fn kv_list(&self, plugin: &str, prefix: &str, limit: usize) -> Result<Vec<String>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT key FROM kv WHERE plugin_id = ?1 AND substr(key, 1, ?3) = ?2
              ORDER BY key LIMIT ?4",
        )?;
        let keys = stmt
            .query_map(
                params![
                    plugin,
                    prefix,
                    i64::try_from(prefix.chars().count()).unwrap_or(i64::MAX),
                    i64::try_from(limit).unwrap_or(i64::MAX)
                ],
                |r| r.get(0),
            )?
            .collect::<rusqlite::Result<_>>()?;
        Ok(keys)
    }

    pub fn last_run(&self, plugin: &str, job: &str) -> Result<Option<i64>> {
        Ok(self
            .db()
            .query_row(
                "SELECT last_run FROM schedule WHERE plugin_id = ?1 AND job = ?2",
                params![plugin, job],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn set_last_run(&self, plugin: &str, job: &str, at: i64) -> Result<()> {
        self.db().execute(
            "INSERT OR REPLACE INTO schedule (plugin_id, job, last_run) VALUES (?1, ?2, ?3)",
            params![plugin, job, at],
        )?;
        Ok(())
    }

    /// The last version of a bundled plugin that was offered, if any was. Kept when the
    /// plugin is uninstalled.
    pub fn offered(&self, plugin: &str) -> Result<Option<String>> {
        Ok(self
            .db()
            .query_row(
                "SELECT version FROM offers WHERE plugin_id = ?1",
                params![plugin],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn set_offered(&self, plugin: &str, version: &str) -> Result<()> {
        self.db().execute(
            "INSERT OR REPLACE INTO offers (plugin_id, version, offered_at) VALUES (?1, ?2, ?3)",
            params![plugin, version, now_ms()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_is_per_plugin_with_a_quota() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        assert!(s.kv_set("a", "k", "\"1\"", 100).unwrap());
        assert!(s.kv_set("b", "k", "\"2\"", 100).unwrap());
        assert_eq!(s.kv_get("a", "k").unwrap().as_deref(), Some("\"1\""));
        assert_eq!(s.kv_get("b", "k").unwrap().as_deref(), Some("\"2\""));
        assert_eq!(s.kv_get("c", "k").unwrap(), None);
        // Replacing a value counts only the new one.
        assert!(s.kv_set("a", "k", &"x".repeat(98), 100).unwrap());
        assert!(!s.kv_set("a", "k2", "xx", 100).unwrap());
        assert!(s.kv_set("a", "feed:1", "1", 1000).unwrap());
        assert!(s.kv_set("a", "feed:2", "1", 1000).unwrap());
        assert!(s.kv_set("a", "feee", "1", 1000).unwrap());
        assert_eq!(s.kv_list("a", "feed:", 10).unwrap(), ["feed:1", "feed:2"]);
        assert_eq!(s.kv_list("a", "", 2).unwrap().len(), 2);
        assert!(s.kv_delete("a", "feed:1").unwrap());
        assert!(!s.kv_delete("a", "feed:1").unwrap());
        // A `%` or `_` in a prefix is literal.
        assert!(s.kv_set("a", "50%", "1", 1000).unwrap());
        assert_eq!(s.kv_list("a", "5%", 10).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn bundles_are_content_addressed_and_checked() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let sha = s.put_code("export default {}").unwrap();
        assert_eq!(s.read_code(&sha).unwrap(), "export default {}");
        fs::write(
            dir.path().join("bundles").join(format!("{sha}.js")),
            "tampered",
        )
        .unwrap();
        assert!(matches!(s.read_code(&sha), Err(StoreError::Corrupt(_))));
        // Unreferenced bundles are collected.
        s.collect_garbage().unwrap();
        assert!(s.read_code(&sha).is_err());
    }
}

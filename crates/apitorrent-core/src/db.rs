//! The instance database: SQLite in WAL mode with embedded, forward-only migrations.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

/// Each entry upgrades the schema by one version. Never edit a released migration; add a
/// new one.
const MIGRATIONS: &[&str] = &[
    // 1: torrents and instance metadata.
    "
    CREATE TABLE torrents (
        id               TEXT PRIMARY KEY,
        info_hash_v1     TEXT,
        info_hash_v2     TEXT,
        name             TEXT NOT NULL,
        torrent          BLOB NOT NULL,
        save_path        TEXT NOT NULL,
        label            TEXT,
        added_at         INTEGER NOT NULL,
        paused           INTEGER NOT NULL DEFAULT 0,
        have             BLOB,
        file_priorities  BLOB,
        downloaded_total INTEGER NOT NULL DEFAULT 0,
        uploaded_total   INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE meta (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    ",
];

/// A torrent as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentRow {
    pub id: String,
    pub info_hash_v1: Option<String>,
    pub info_hash_v2: Option<String>,
    pub name: String,
    pub torrent: Vec<u8>,
    pub save_path: String,
    pub label: Option<String>,
    pub added_at: i64,
    pub paused: bool,
    /// The resume bitfield in wire format, with the piece count implied by the torrent.
    pub have: Option<Vec<u8>>,
    pub file_priorities: Option<Vec<u8>>,
    pub downloaded_total: u64,
    pub uploaded_total: u64,
}

#[derive(Debug)]
pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Self> {
        // WAL lets readers run alongside the writer. FULL sync makes every committed change
        // survive power loss; writes here are infrequent enough that the cost doesn't matter.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let mut db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&mut self) -> rusqlite::Result<()> {
        let current = self.schema_version()?;
        if current > MIGRATIONS.len() {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "database schema {current} is newer than this build supports ({})",
                MIGRATIONS.len()
            )));
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", i64::try_from(i + 1).expect("fits"))?;
            tx.commit()?;
            tracing::info!(version = i + 1, "database migrated");
        }
        Ok(())
    }

    pub fn schema_version(&self) -> rusqlite::Result<usize> {
        let v: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        Ok(usize::try_from(v).unwrap_or(0))
    }

    pub fn insert_torrent(&self, row: &TorrentRow) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO torrents (id, info_hash_v1, info_hash_v2, name, torrent, save_path,
                 label, added_at, paused, have, file_priorities, downloaded_total, uploaded_total)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                row.id,
                row.info_hash_v1,
                row.info_hash_v2,
                row.name,
                row.torrent,
                row.save_path,
                row.label,
                row.added_at,
                row.paused,
                row.have,
                row.file_priorities,
                to_i64(row.downloaded_total),
                to_i64(row.uploaded_total),
            ],
        )?;
        Ok(())
    }

    pub fn contains(&self, id: &str) -> rusqlite::Result<bool> {
        self.conn
            .query_row("SELECT 1 FROM torrents WHERE id = ?1", [id], |_| Ok(()))
            .optional()
            .map(|r| r.is_some())
    }

    pub fn torrents(&self) -> rusqlite::Result<Vec<TorrentRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, info_hash_v1, info_hash_v2, name, torrent, save_path, label, added_at,
                    paused, have, file_priorities, downloaded_total, uploaded_total
             FROM torrents ORDER BY added_at",
        )?;
        stmt.query_map([], |r| {
            Ok(TorrentRow {
                id: r.get(0)?,
                info_hash_v1: r.get(1)?,
                info_hash_v2: r.get(2)?,
                name: r.get(3)?,
                torrent: r.get(4)?,
                save_path: r.get(5)?,
                label: r.get(6)?,
                added_at: r.get(7)?,
                paused: r.get(8)?,
                have: r.get(9)?,
                file_priorities: r.get(10)?,
                downloaded_total: from_i64(r.get(11)?),
                uploaded_total: from_i64(r.get(12)?),
            })
        })?
        .collect()
    }

    /// Save resume state for many torrents in one transaction.
    pub fn save_resume(&mut self, rows: &[(String, Vec<u8>, u64, u64)]) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE torrents SET have = ?2, downloaded_total = ?3, uploaded_total = ?4
                 WHERE id = ?1",
            )?;
            for (id, have, down, up) in rows {
                stmt.execute(params![id, have, to_i64(*down), to_i64(*up)])?;
            }
        }
        tx.commit()
    }

    pub fn set_paused(&self, id: &str, paused: bool) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET paused = ?2 WHERE id = ?1",
            params![id, paused],
        )?;
        Ok(())
    }

    pub fn set_file_priorities(&self, id: &str, prios: &[u8]) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET file_priorities = ?2 WHERE id = ?1",
            params![id, prios],
        )?;
        Ok(())
    }

    pub fn set_label(&self, id: &str, label: Option<&str>) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET label = ?2 WHERE id = ?1",
            params![id, label],
        )?;
        Ok(())
    }

    pub fn remove_torrent(&self, id: &str) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM torrents WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn meta(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

/// SQLite integers are signed. Byte totals never get near `i64::MAX`.
fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn from_i64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str) -> TorrentRow {
        TorrentRow {
            id: id.into(),
            info_hash_v1: Some(id.into()),
            info_hash_v2: None,
            name: "name".into(),
            torrent: vec![1, 2, 3],
            save_path: "/downloads".into(),
            label: None,
            added_at: 1,
            paused: false,
            have: None,
            file_priorities: None,
            downloaded_total: 0,
            uploaded_total: 0,
        }
    }

    #[test]
    fn crud_and_resume() {
        let mut db = Database::open_in_memory().unwrap();
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len());
        db.insert_torrent(&row("a")).unwrap();
        db.insert_torrent(&row("b")).unwrap();
        assert!(db.insert_torrent(&row("a")).is_err(), "ids are unique");
        assert!(db.contains("a").unwrap());

        db.save_resume(&[("a".into(), vec![0xff], 10, 20)]).unwrap();
        db.set_paused("b", true).unwrap();
        db.set_label("b", Some("linux")).unwrap();
        let rows = db.torrents().unwrap();
        assert_eq!(rows[0].have.as_deref(), Some(&[0xff][..]));
        assert_eq!((rows[0].downloaded_total, rows[0].uploaded_total), (10, 20));
        assert!(rows[1].paused);
        assert_eq!(rows[1].label.as_deref(), Some("linux"));

        db.remove_torrent("a").unwrap();
        assert_eq!(db.torrents().unwrap().len(), 1);

        db.set_meta("k", "v1").unwrap();
        db.set_meta("k", "v2").unwrap();
        assert_eq!(db.meta("k").unwrap().as_deref(), Some("v2"));
    }

    #[test]
    fn reopening_keeps_data_and_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        Database::open(&path)
            .unwrap()
            .insert_torrent(&row("a"))
            .unwrap();
        let db = Database::open(&path).unwrap();
        assert_eq!(db.torrents().unwrap().len(), 1);
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len());
    }
}

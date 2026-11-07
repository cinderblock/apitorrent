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
    // 2: where each file's data lives (save, held, part store), and the held area.
    "
    ALTER TABLE torrents ADD COLUMN placements BLOB;
    ALTER TABLE torrents ADD COLUMN held_path TEXT;
    ALTER TABLE torrents ADD COLUMN unwanted_policy INTEGER NOT NULL DEFAULT 0;
    ",
    // 3: queue order and per-torrent limits. Existing torrents queue in the order added.
    "
    ALTER TABLE torrents ADD COLUMN queue_position INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE torrents ADD COLUMN download_rate INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE torrents ADD COLUMN upload_rate INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE torrents ADD COLUMN max_connections INTEGER;
    UPDATE torrents SET queue_position = (
        SELECT COUNT(*) FROM torrents AS t
        WHERE t.added_at < torrents.added_at
           OR (t.added_at = torrents.added_at AND t.id < torrents.id)
    );
    ",
    // 4: magnet links. Until the metadata arrives, `torrent` is empty.
    "
    ALTER TABLE torrents ADD COLUMN magnet TEXT;
    ",
    // 5: naming: 0 portable, 1 native (case-sensitive filesystem), 2 native (case-insensitive).
    "
    ALTER TABLE torrents ADD COLUMN naming INTEGER NOT NULL DEFAULT 0;
    ",
    // 6: the content library: files linked from other torrents, and which torrents a pack
    // has taken over.
    "
    ALTER TABLE torrents ADD COLUMN linked BLOB;
    CREATE TABLE superseded (
        torrent    TEXT PRIMARY KEY,
        by_torrent TEXT NOT NULL
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
    /// One byte per file: 0 save, 1 held, 2 part store.
    pub placements: Option<Vec<u8>>,
    pub held_path: Option<String>,
    /// 0 hold, 1 delete.
    pub unwanted_policy: u8,
    pub queue_position: u32,
    pub download_rate: u64,
    pub upload_rate: u64,
    pub max_connections: Option<u32>,
    /// The magnet link it was added from, if any.
    pub magnet: Option<String>,
    /// 0 portable, 1 native case-sensitive, 2 native case-insensitive.
    pub naming: u8,
    /// Indices of files still linked to another torrent's copy, as little-endian u32s.
    pub linked: Option<Vec<u8>>,
}

/// Resume state saved periodically for one torrent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeRow {
    pub id: String,
    pub have: Vec<u8>,
    pub placements: Vec<u8>,
    pub linked: Vec<u8>,
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
                 label, added_at, paused, have, file_priorities, downloaded_total, uploaded_total,
                 placements, held_path, unwanted_policy, queue_position, download_rate,
                 upload_rate, max_connections, magnet, naming, linked)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                 ?17, ?18, ?19, ?20, ?21, ?22, ?23)",
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
                row.placements,
                row.held_path,
                row.unwanted_policy,
                row.queue_position,
                to_i64(row.download_rate),
                to_i64(row.upload_rate),
                row.max_connections,
                row.magnet,
                row.naming,
                row.linked,
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
                    paused, have, file_priorities, downloaded_total, uploaded_total,
                    placements, held_path, unwanted_policy, queue_position, download_rate,
                    upload_rate, max_connections, magnet, naming, linked
             FROM torrents ORDER BY queue_position, added_at",
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
                placements: r.get(13)?,
                held_path: r.get(14)?,
                unwanted_policy: r.get(15)?,
                queue_position: r.get(16)?,
                download_rate: from_i64(r.get(17)?),
                upload_rate: from_i64(r.get(18)?),
                max_connections: r.get(19)?,
                magnet: r.get(20)?,
                naming: r.get(21)?,
                linked: r.get(22)?,
            })
        })?
        .collect()
    }

    /// Save resume state for many torrents in one transaction.
    pub fn save_resume(&mut self, rows: &[ResumeRow]) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE torrents SET have = ?2, placements = ?3, downloaded_total = ?4,
                     uploaded_total = ?5, linked = ?6
                 WHERE id = ?1",
            )?;
            for r in rows {
                stmt.execute(params![
                    r.id,
                    r.have,
                    r.placements,
                    to_i64(r.downloaded_total),
                    to_i64(r.uploaded_total),
                    r.linked
                ])?;
            }
        }
        tx.commit()
    }

    pub fn set_queue_positions(&mut self, positions: &[(String, u32)]) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare("UPDATE torrents SET queue_position = ?2 WHERE id = ?1")?;
            for (id, pos) in positions {
                stmt.execute(params![id, pos])?;
            }
        }
        tx.commit()
    }

    pub fn set_limits(
        &self,
        id: &str,
        download_rate: u64,
        upload_rate: u64,
        max_connections: Option<u32>,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET download_rate = ?2, upload_rate = ?3, max_connections = ?4
             WHERE id = ?1",
            params![
                id,
                to_i64(download_rate),
                to_i64(upload_rate),
                max_connections
            ],
        )?;
        Ok(())
    }

    pub fn set_unwanted_policy(&self, id: &str, policy: u8) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET unwanted_policy = ?2 WHERE id = ?1",
            params![id, policy],
        )?;
        Ok(())
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

    /// Store the metadata fetched for a magnet link.
    pub fn set_metadata(
        &self,
        id: &str,
        torrent: &[u8],
        name: &str,
        info_hash_v1: Option<&str>,
        info_hash_v2: Option<&str>,
        file_priorities: &[u8],
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET torrent = ?2, name = ?3, info_hash_v1 = ?4, info_hash_v2 = ?5,
                 file_priorities = ?6
             WHERE id = ?1",
            params![
                id,
                torrent,
                name,
                info_hash_v1,
                info_hash_v2,
                file_priorities
            ],
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
        self.conn.execute(
            "DELETE FROM superseded WHERE torrent = ?1 OR by_torrent = ?1",
            [id],
        )?;
        Ok(())
    }

    /// Record that `by` holds every file of `torrent`.
    pub fn set_superseded(&self, torrent: &str, by: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO superseded (torrent, by_torrent) VALUES (?1, ?2)
             ON CONFLICT(torrent) DO UPDATE SET by_torrent = excluded.by_torrent",
            params![torrent, by],
        )?;
        Ok(())
    }

    pub fn superseded(&self) -> rusqlite::Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT torrent, by_torrent FROM superseded")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect()
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
            placements: None,
            held_path: Some("/data/held/a".into()),
            unwanted_policy: 0,
            queue_position: 0,
            download_rate: 0,
            upload_rate: 0,
            max_connections: None,
            magnet: None,
            naming: 0,
            linked: None,
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

        db.save_resume(&[ResumeRow {
            id: "a".into(),
            have: vec![0xff],
            placements: vec![0, 2],
            linked: Vec::new(),
            downloaded_total: 10,
            uploaded_total: 20,
        }])
        .unwrap();
        db.set_unwanted_policy("b", 1).unwrap();
        db.set_queue_positions(&[("a".into(), 5), ("b".into(), 2)])
            .unwrap();
        db.set_limits("a", 1000, 2000, Some(30)).unwrap();
        db.set_paused("b", true).unwrap();
        db.set_label("b", Some("linux")).unwrap();
        let rows = db.torrents().unwrap();
        assert_eq!(rows[1].have.as_deref(), Some(&[0xff][..]));
        assert_eq!((rows[1].downloaded_total, rows[1].uploaded_total), (10, 20));
        // Rows come back in queue order: b (2) before a (5).
        assert_eq!(rows[0].id, "b");
        assert_eq!(rows[1].placements.as_deref(), Some(&[0, 2][..]));
        assert_eq!(rows[1].held_path.as_deref(), Some("/data/held/a"));
        assert_eq!(rows[0].unwanted_policy, 1);
        assert_eq!(
            (
                rows[1].download_rate,
                rows[1].upload_rate,
                rows[1].max_connections
            ),
            (1000, 2000, Some(30))
        );
        assert!(rows[0].paused);
        assert_eq!(rows[0].label.as_deref(), Some("linux"));

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

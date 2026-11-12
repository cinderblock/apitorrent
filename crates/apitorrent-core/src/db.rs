//! The instance database: SQLite in WAL mode with embedded, forward-only migrations.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use apitorrent_types::spaces::{Space, SpaceId, SpaceSettings};

use crate::Key;

mod library;
pub use library::{FolderFile, FolderRow};

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
    // 7: per-file paths that differ from the naming policy's (files renamed in Deluge, or
    // laid out the way libtorrent did), as a JSON list of path component lists.
    "
    ALTER TABLE torrents ADD COLUMN file_paths TEXT;
    ",
    // 8: fingerprints of files on disk (see `fingerprint`), keyed by absolute path.
    "
    CREATE TABLE fingerprints (
        path        TEXT PRIMARY KEY,
        size        INTEGER NOT NULL,
        mtime_ns    INTEGER NOT NULL,
        pieces_root BLOB NOT NULL,
        sha256      BLOB NOT NULL,
        computed_at INTEGER NOT NULL
    );
    ",
    // 9: seeding time and seed limits per torrent (see `seeding`). `limits` is JSON.
    "
    CREATE TABLE seeding (
        torrent   TEXT PRIMARY KEY REFERENCES torrents(id) ON DELETE CASCADE,
        seed_secs INTEGER NOT NULL DEFAULT 0,
        limits    TEXT,
        reached   INTEGER NOT NULL DEFAULT 0
    );
    ",
    // 10: spaces. Everything so far goes into the `default` space (id 1), and a torrent is
    // keyed by its space and id, so the same torrent can be in several spaces. `settings`
    // is JSON (`apitorrent_types::SpaceSettings`). The child tables are copied before their
    // parent is dropped: dropping `torrents` first would cascade into `seeding`.
    "
    CREATE TABLE spaces (
        id          INTEGER PRIMARY KEY,
        slug        TEXT NOT NULL UNIQUE,
        name        TEXT NOT NULL,
        description TEXT NOT NULL DEFAULT '',
        settings    TEXT NOT NULL DEFAULT '{}',
        created_at  INTEGER NOT NULL
    );
    INSERT INTO spaces (id, slug, name, description, settings, created_at)
        VALUES (1, 'default', 'Default', '', '{}', 0);
    CREATE TABLE torrents_v10 (
        space_id         INTEGER NOT NULL DEFAULT 1 REFERENCES spaces(id),
        id               TEXT NOT NULL,
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
        uploaded_total   INTEGER NOT NULL DEFAULT 0,
        placements       BLOB,
        held_path        TEXT,
        unwanted_policy  INTEGER NOT NULL DEFAULT 0,
        queue_position   INTEGER NOT NULL DEFAULT 0,
        download_rate    INTEGER NOT NULL DEFAULT 0,
        upload_rate      INTEGER NOT NULL DEFAULT 0,
        max_connections  INTEGER,
        magnet           TEXT,
        naming           INTEGER NOT NULL DEFAULT 0,
        linked           BLOB,
        file_paths       TEXT,
        PRIMARY KEY (space_id, id)
    );
    INSERT INTO torrents_v10 (space_id, id, info_hash_v1, info_hash_v2, name, torrent,
        save_path, label, added_at, paused, have, file_priorities, downloaded_total,
        uploaded_total, placements, held_path, unwanted_policy, queue_position, download_rate,
        upload_rate, max_connections, magnet, naming, linked, file_paths)
    SELECT 1, id, info_hash_v1, info_hash_v2, name, torrent, save_path, label, added_at,
        paused, have, file_priorities, downloaded_total, uploaded_total, placements,
        held_path, unwanted_policy, queue_position, download_rate, upload_rate,
        max_connections, magnet, naming, linked, file_paths
    FROM torrents;
    CREATE TABLE seeding_v10 (
        space_id  INTEGER NOT NULL DEFAULT 1,
        torrent   TEXT NOT NULL,
        seed_secs INTEGER NOT NULL DEFAULT 0,
        limits    TEXT,
        reached   INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (space_id, torrent),
        FOREIGN KEY (space_id, torrent) REFERENCES torrents_v10(space_id, id) ON DELETE CASCADE
    );
    INSERT INTO seeding_v10 (space_id, torrent, seed_secs, limits, reached)
        SELECT 1, torrent, seed_secs, limits, reached FROM seeding;
    CREATE TABLE superseded_v10 (
        space_id   INTEGER NOT NULL DEFAULT 1,
        torrent    TEXT NOT NULL,
        by_torrent TEXT NOT NULL,
        PRIMARY KEY (space_id, torrent)
    );
    INSERT INTO superseded_v10 (space_id, torrent, by_torrent)
        SELECT 1, torrent, by_torrent FROM superseded;
    DROP TABLE seeding;
    DROP TABLE superseded;
    DROP TABLE torrents;
    ALTER TABLE torrents_v10 RENAME TO torrents;
    ALTER TABLE seeding_v10 RENAME TO seeding;
    ALTER TABLE superseded_v10 RENAME TO superseded;
    CREATE INDEX torrents_by_id ON torrents(id);
    ",
    // 11: each entry's own tracker list, once edited (JSON tiers; NULL = the .torrent's).
    "
    ALTER TABLE torrents ADD COLUMN trackers TEXT;
    ",
    // 12: library folders (files outside torrents that new torrents may reuse) and what
    // their last walk found; see `db::library`. A file's root and SHA-256 are NULL until it
    // is fingerprinted, and cleared when its size or time changes.
    "
    CREATE TABLE library_folders (
        id           INTEGER PRIMARY KEY,
        path         TEXT NOT NULL UNIQUE,
        added_at     INTEGER NOT NULL,
        last_scan_at INTEGER,
        error        TEXT
    );
    CREATE TABLE library_files (
        folder_id   INTEGER NOT NULL REFERENCES library_folders(id) ON DELETE CASCADE,
        path        TEXT NOT NULL,
        size        INTEGER NOT NULL,
        mtime_ns    INTEGER NOT NULL,
        pieces_root BLOB,
        sha256      BLOB,
        PRIMARY KEY (folder_id, path)
    );
    CREATE INDEX library_files_by_size ON library_files(size);
    CREATE INDEX library_files_by_root ON library_files(pieces_root);
    ",
    // 13: where the content library supplied a file's bytes from (see `share`), per
    // torrent id (the swarm's files, whatever spaces hold it). `method`: 0 hard link,
    // 1 clone, 2 copy, 3 piece sharing.
    "
    CREATE TABLE file_reuse (
        torrent     TEXT NOT NULL,
        file        INTEGER NOT NULL,
        source      TEXT NOT NULL,
        source_name TEXT NOT NULL,
        method      INTEGER NOT NULL,
        bytes       INTEGER NOT NULL,
        PRIMARY KEY (torrent, file)
    );
    ",
];

/// Where the content library supplied one file's bytes from (see `share`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReuseRow {
    pub torrent: String,
    pub file: usize,
    pub source: String,
    pub source_name: String,
    /// 0 hard link, 1 clone, 2 copy, 3 piece sharing.
    pub method: u8,
    pub bytes: u64,
}

/// A torrent as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentRow {
    pub space: SpaceId,
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
    /// Per-file paths overriding the naming policy, as JSON.
    pub file_paths: Option<String>,
    /// The entry's announce tiers as JSON, once edited; `None` uses the `.torrent`'s own.
    pub trackers: Option<String>,
}

/// Resume state saved periodically for one torrent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeRow {
    pub key: Key,
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

    /// Open an existing database without changing it: read-only, and only if its schema is
    /// this build's (`Ok(None)` otherwise, or when there is no file).
    pub fn open_read_only(path: &Path) -> rusqlite::Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let db = Self { conn };
        Ok((db.schema_version()? == MIGRATIONS.len()).then_some(db))
    }

    /// The torrent ids a space has (with metadata or not).
    pub fn torrent_ids(
        &self,
        space: SpaceId,
    ) -> rusqlite::Result<std::collections::HashSet<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM torrents WHERE space_id = ?1")?;
        stmt.query_map([space], |r| r.get(0))?.collect()
    }

    /// The end of a space's queue.
    pub fn next_queue_position(&self, space: SpaceId) -> rusqlite::Result<u32> {
        let max: Option<i64> = self.conn.query_row(
            "SELECT MAX(queue_position) FROM torrents WHERE space_id = ?1",
            [space],
            |r| r.get(0),
        )?;
        Ok(max.map_or(0, |m| u32::try_from(m + 1).unwrap_or(u32::MAX)))
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
                 upload_rate, max_connections, magnet, naming, linked, file_paths, space_id,
                 trackers)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                 ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)",
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
                row.file_paths,
                row.space,
                row.trackers,
            ],
        )?;
        Ok(())
    }

    pub fn contains(&self, key: &Key) -> rusqlite::Result<bool> {
        self.conn
            .query_row(
                "SELECT 1 FROM torrents WHERE space_id = ?1 AND id = ?2",
                params![key.space, key.id],
                |_| Ok(()),
            )
            .optional()
            .map(|r| r.is_some())
    }

    /// A torrent's `.torrent` file, from any space that has it; `None` for an unknown id or
    /// a magnet link still waiting for its metadata (stored empty).
    pub fn torrent_file(&self, id: &str) -> rusqlite::Result<Option<Vec<u8>>> {
        Ok(self
            .conn
            .query_row(
                "SELECT torrent FROM torrents WHERE id = ?1 AND length(torrent) > 0 LIMIT 1",
                [id],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()?
            .filter(|t| !t.is_empty()))
    }

    /// Every torrent of every space, oldest first within each queue position.
    pub fn torrents(&self) -> rusqlite::Result<Vec<TorrentRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, info_hash_v1, info_hash_v2, name, torrent, save_path, label, added_at,
                    paused, have, file_priorities, downloaded_total, uploaded_total,
                    placements, held_path, unwanted_policy, queue_position, download_rate,
                    upload_rate, max_connections, magnet, naming, linked, file_paths, space_id,
                    trackers
             FROM torrents ORDER BY queue_position, added_at, space_id",
        )?;
        stmt.query_map([], |r| {
            Ok(TorrentRow {
                space: r.get(24)?,
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
                file_paths: r.get(23)?,
                trackers: r.get(25)?,
            })
        })?
        .collect()
    }

    /// Save resume state for many torrents in one transaction.
    pub fn save_resume(&mut self, rows: &[ResumeRow]) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE torrents SET have = NULLIF(?2, x''), placements = NULLIF(?3, x''),
                     downloaded_total = ?4, uploaded_total = ?5, linked = NULLIF(?6, x'')
                 WHERE id = ?1 AND space_id = ?7",
            )?;
            for r in rows {
                stmt.execute(params![
                    r.key.id,
                    r.have,
                    r.placements,
                    to_i64(r.downloaded_total),
                    to_i64(r.uploaded_total),
                    r.linked,
                    r.key.space
                ])?;
            }
        }
        tx.commit()
    }

    pub fn set_queue_positions(&mut self, positions: &[(Key, u32)]) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE torrents SET queue_position = ?3 WHERE space_id = ?1 AND id = ?2",
            )?;
            for (key, pos) in positions {
                stmt.execute(params![key.space, key.id, pos])?;
            }
        }
        tx.commit()
    }

    pub fn set_limits(
        &self,
        key: &Key,
        download_rate: u64,
        upload_rate: u64,
        max_connections: Option<u32>,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET download_rate = ?2, upload_rate = ?3, max_connections = ?4
             WHERE id = ?1 AND space_id = ?5",
            params![
                key.id,
                to_i64(download_rate),
                to_i64(upload_rate),
                max_connections,
                key.space
            ],
        )?;
        Ok(())
    }

    pub fn set_unwanted_policy(&self, key: &Key, policy: u8) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET unwanted_policy = ?3 WHERE space_id = ?1 AND id = ?2",
            params![key.space, key.id, policy],
        )?;
        Ok(())
    }

    pub fn set_paused(&self, key: &Key, paused: bool) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET paused = ?3 WHERE space_id = ?1 AND id = ?2",
            params![key.space, key.id, paused],
        )?;
        Ok(())
    }

    pub fn set_file_priorities(&self, key: &Key, prios: &[u8]) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET file_priorities = ?3 WHERE space_id = ?1 AND id = ?2",
            params![key.space, key.id, prios],
        )?;
        Ok(())
    }

    pub fn set_save_path(&self, key: &Key, save_path: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET save_path = ?3 WHERE space_id = ?1 AND id = ?2",
            params![key.space, key.id, save_path],
        )?;
        Ok(())
    }

    /// Store the metadata fetched for a magnet link.
    pub fn set_metadata(
        &self,
        key: &Key,
        torrent: &[u8],
        name: &str,
        info_hash_v1: Option<&str>,
        info_hash_v2: Option<&str>,
        file_priorities: &[u8],
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET torrent = ?2, name = ?3, info_hash_v1 = ?4, info_hash_v2 = ?5,
                 file_priorities = ?6
             WHERE id = ?1 AND space_id = ?7",
            params![
                key.id,
                torrent,
                name,
                info_hash_v1,
                info_hash_v2,
                file_priorities,
                key.space
            ],
        )?;
        Ok(())
    }

    /// An entry's own announce tiers (JSON), or `None` for the `.torrent`'s.
    pub fn set_trackers(&self, key: &Key, trackers: Option<&str>) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET trackers = ?3 WHERE space_id = ?1 AND id = ?2",
            params![key.space, key.id, trackers],
        )?;
        Ok(())
    }

    pub fn set_label(&self, key: &Key, label: Option<&str>) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE torrents SET label = ?3 WHERE space_id = ?1 AND id = ?2",
            params![key.space, key.id, label],
        )?;
        Ok(())
    }

    pub fn remove_torrent(&self, key: &Key) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM torrents WHERE space_id = ?1 AND id = ?2",
            params![key.space, key.id],
        )?;
        self.conn.execute(
            "DELETE FROM superseded WHERE space_id = ?1 AND (torrent = ?2 OR by_torrent = ?2)",
            params![key.space, key.id],
        )?;
        Ok(())
    }

    /// Record that `by` (in the same space) holds every file of `torrent`.
    pub fn set_superseded(&self, torrent: &Key, by: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO superseded (space_id, torrent, by_torrent) VALUES (?1, ?2, ?3)
             ON CONFLICT(space_id, torrent) DO UPDATE SET by_torrent = excluded.by_torrent",
            params![torrent.space, torrent.id, by],
        )?;
        Ok(())
    }

    pub fn superseded(&self) -> rusqlite::Result<Vec<(Key, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT space_id, torrent, by_torrent FROM superseded")?;
        stmt.query_map([], |r| {
            Ok((Key::new(r.get(0)?, r.get::<_, String>(1)?), r.get(2)?))
        })?
        .collect()
    }

    // ----- spaces ---------------------------------------------------------------------

    /// Every space, by id. Settings that don't parse read as the defaults.
    pub fn spaces(&self) -> rusqlite::Result<Vec<Space>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, slug, name, description, settings FROM spaces ORDER BY id")?;
        stmt.query_map([], |r| {
            let settings: String = r.get(4)?;
            Ok(Space {
                id: r.get(0)?,
                slug: r.get(1)?,
                name: r.get(2)?,
                description: r.get(3)?,
                settings: serde_json::from_str(&settings).unwrap_or_default(),
            })
        })?
        .collect()
    }

    /// Create a space; returns its id.
    pub fn insert_space(
        &self,
        slug: &str,
        name: &str,
        description: &str,
        settings: &SpaceSettings,
        now: i64,
    ) -> rusqlite::Result<SpaceId> {
        let json = serde_json::to_string(settings).expect("space settings serialize");
        self.conn.execute(
            "INSERT INTO spaces (slug, name, description, settings, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![slug, name, description, json, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn update_space(
        &self,
        id: SpaceId,
        name: &str,
        description: &str,
        settings: &SpaceSettings,
    ) -> rusqlite::Result<bool> {
        let json = serde_json::to_string(settings).expect("space settings serialize");
        Ok(self.conn.execute(
            "UPDATE spaces SET name = ?2, description = ?3, settings = ?4 WHERE id = ?1",
            params![id, name, description, json],
        )? == 1)
    }

    /// Delete an empty space. Refused (`Ok(false)`) while it holds torrents.
    pub fn delete_space(&self, id: SpaceId) -> rusqlite::Result<bool> {
        let used: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM torrents WHERE space_id = ?1",
            [id],
            |r| r.get(0),
        )?;
        if used > 0 {
            return Ok(false);
        }
        Ok(self
            .conn
            .execute("DELETE FROM spaces WHERE id = ?1", [id])?
            == 1)
    }

    pub fn set_fingerprint(
        &self,
        path: &str,
        fp: &crate::fingerprint::Fingerprint,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO fingerprints (path, size, mtime_ns, pieces_root, sha256, computed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(path) DO UPDATE SET size = excluded.size,
                 mtime_ns = excluded.mtime_ns, pieces_root = excluded.pieces_root,
                 sha256 = excluded.sha256, computed_at = excluded.computed_at",
            params![
                path,
                i64::try_from(fp.size).unwrap_or(i64::MAX),
                fp.mtime_ns,
                fp.pieces_root.as_bytes().as_slice(),
                fp.sha256.as_slice(),
                crate::fingerprint::now_secs(),
            ],
        )?;
        Ok(())
    }

    // ----- file reuse -----------------------------------------------------------------

    /// Record reused bytes of a file. A file already recorded keeps its source and method
    /// and adds the bytes (a bookend after a link, say).
    pub fn add_reuse(&mut self, rows: &[ReuseRow]) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        for r in rows {
            tx.execute(
                "INSERT INTO file_reuse (torrent, file, source, source_name, method, bytes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(torrent, file) DO UPDATE SET bytes = bytes + excluded.bytes",
                params![
                    r.torrent,
                    i64::try_from(r.file).unwrap_or(i64::MAX),
                    r.source,
                    r.source_name,
                    r.method,
                    i64::try_from(r.bytes).unwrap_or(i64::MAX),
                ],
            )?;
        }
        tx.commit()
    }

    /// Every file reuse record. Malformed rows are skipped.
    pub fn reuse(&self) -> rusqlite::Result<Vec<ReuseRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT torrent, file, source, source_name, method, bytes FROM file_reuse",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        Ok(rows
            .filter_map(Result::ok)
            .filter_map(|(torrent, file, source, source_name, method, bytes)| {
                Some(ReuseRow {
                    torrent,
                    file: usize::try_from(file).ok()?,
                    source,
                    source_name,
                    method: u8::try_from(method).ok()?,
                    bytes: u64::try_from(bytes).ok()?,
                })
            })
            .collect())
    }

    /// Forget a torrent's reuse records (when its last entry is removed).
    pub fn delete_reuse(&self, torrent: &str) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM file_reuse WHERE torrent = ?1", [torrent])?;
        Ok(())
    }

    /// Every stored fingerprint. Malformed rows are skipped.
    pub fn fingerprints(&self) -> rusqlite::Result<Vec<(String, crate::fingerprint::Fingerprint)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, size, mtime_ns, pieces_root, sha256 FROM fingerprints")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Vec<u8>>(3)?,
                r.get::<_, Vec<u8>>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (path, size, mtime_ns, root, sha) = row?;
            let (Ok(root), Ok(sha256), Ok(size)) = (
                <[u8; 32]>::try_from(root.as_slice()),
                <[u8; 32]>::try_from(sha.as_slice()),
                u64::try_from(size),
            ) else {
                continue;
            };
            out.push((
                path,
                crate::fingerprint::Fingerprint {
                    size,
                    mtime_ns,
                    pieces_root: apitorrent_metainfo::Sha256Hash(root),
                    sha256,
                },
            ));
        }
        Ok(out)
    }

    pub fn delete_fingerprints(&mut self, paths: &[String]) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        for p in paths {
            tx.execute("DELETE FROM fingerprints WHERE path = ?1", [p])?;
        }
        tx.commit()
    }

    /// Every torrent's seeding state. Rows whose limits don't parse keep no limits.
    pub fn seed_states(&self) -> rusqlite::Result<Vec<(Key, crate::seeding::SeedState)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT torrent, seed_secs, limits, reached, space_id FROM seeding")?;
        stmt.query_map([], |r| {
            let limits: Option<String> = r.get(2)?;
            Ok((
                Key::new(r.get(4)?, r.get::<_, String>(0)?),
                crate::seeding::SeedState {
                    seed_ms: u64::try_from(r.get::<_, i64>(1)?).unwrap_or(0) * 1000,
                    limits: limits.and_then(|l| serde_json::from_str(&l).ok()),
                    reached: r.get::<_, i64>(3)? != 0,
                },
            ))
        })?
        .collect()
    }

    /// Store seeding states, in one transaction. Torrents no longer here are skipped.
    pub fn save_seed_states(
        &mut self,
        rows: &[(Key, crate::seeding::SeedState)],
    ) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO seeding (torrent, seed_secs, limits, reached, space_id)
                 SELECT ?1, ?2, ?3, ?4, ?5
                 WHERE EXISTS (SELECT 1 FROM torrents WHERE id = ?1 AND space_id = ?5)
                 ON CONFLICT(space_id, torrent) DO UPDATE SET seed_secs = excluded.seed_secs,
                     limits = excluded.limits, reached = excluded.reached",
            )?;
            for (key, s) in rows {
                let limits = s
                    .limits
                    .map(|l| serde_json::to_string(&l).expect("limits serialize"));
                stmt.execute(params![
                    key.id,
                    to_i64(s.seed_ms / 1000),
                    limits,
                    i64::from(s.reached),
                    key.space
                ])?;
            }
        }
        tx.commit()
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
            space: 1,
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
            file_paths: None,
            trackers: None,
        }
    }

    #[test]
    fn an_entrys_own_trackers_are_kept() {
        let db = Database::open_in_memory().unwrap();
        db.insert_torrent(&row("a")).unwrap();
        let a = Key::new(1, "a");
        db.set_trackers(&a, Some(r#"[["udp://t:1"]]"#)).unwrap();
        assert_eq!(
            db.torrents().unwrap()[0].trackers.as_deref(),
            Some(r#"[["udp://t:1"]]"#)
        );
        db.set_trackers(&a, None).unwrap();
        assert_eq!(db.torrents().unwrap()[0].trackers, None);
    }

    #[test]
    fn crud_and_resume() {
        let mut db = Database::open_in_memory().unwrap();
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len());
        db.insert_torrent(&row("a")).unwrap();
        db.insert_torrent(&row("b")).unwrap();
        assert!(db.insert_torrent(&row("a")).is_err(), "ids are unique");
        let (a, b) = (Key::new(1, "a"), Key::new(1, "b"));
        assert!(db.contains(&a).unwrap());

        db.save_resume(&[ResumeRow {
            key: a.clone(),
            have: vec![0xff],
            placements: vec![0, 2],
            linked: Vec::new(),
            downloaded_total: 10,
            uploaded_total: 20,
        }])
        .unwrap();
        db.set_unwanted_policy(&b, 1).unwrap();
        db.set_queue_positions(&[(a.clone(), 5), (b.clone(), 2)])
            .unwrap();
        db.set_limits(&a, 1000, 2000, Some(30)).unwrap();
        db.set_paused(&b, true).unwrap();
        db.set_label(&b, Some("linux")).unwrap();
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

        db.remove_torrent(&a).unwrap();
        assert_eq!(db.torrents().unwrap().len(), 1);

        db.set_meta("k", "v1").unwrap();
        db.set_meta("k", "v2").unwrap();
        assert_eq!(db.meta("k").unwrap().as_deref(), Some("v2"));
    }

    #[test]
    fn one_torrent_in_two_spaces() {
        let mut db = Database::open_in_memory().unwrap();
        let spaces = db.spaces().unwrap();
        assert_eq!(spaces.len(), 1);
        assert_eq!((spaces[0].id, spaces[0].slug.as_str()), (1, "default"));
        let tv = db
            .insert_space("tv", "TV", "", &SpaceSettings::default(), 5)
            .unwrap();
        assert!(
            db.insert_space("tv", "Again", "", &SpaceSettings::default(), 5)
                .is_err(),
            "slugs are unique"
        );
        db.insert_torrent(&row("a")).unwrap();
        db.insert_torrent(&TorrentRow {
            space: tv,
            save_path: "/tv".into(),
            ..row("a")
        })
        .unwrap();
        db.save_seed_states(&[(Key::new(tv, "a"), crate::seeding::SeedState::default())])
            .unwrap();
        assert!(!db.delete_space(tv).unwrap(), "a space with torrents stays");
        db.set_label(&Key::new(tv, "a"), Some("shows")).unwrap();
        let rows = db.torrents().unwrap();
        assert_eq!(rows.len(), 2);
        let theirs = rows.iter().find(|r| r.space == tv).unwrap();
        assert_eq!(theirs.label.as_deref(), Some("shows"));
        assert!(rows.iter().find(|r| r.space == 1).unwrap().label.is_none());
        db.remove_torrent(&Key::new(tv, "a")).unwrap();
        assert!(db.seed_states().unwrap().is_empty(), "seeding rows cascade");
        assert!(db.contains(&Key::new(1, "a")).unwrap());
        assert!(db.delete_space(tv).unwrap());
    }

    /// Reuse records add up per file, keep their first source and method, and go with
    /// their torrent.
    #[test]
    fn reuse_records_add_up() {
        let mut db = Database::open_in_memory().unwrap();
        let row = |torrent: &str, file, method, bytes| ReuseRow {
            torrent: torrent.into(),
            file,
            source: "src".into(),
            source_name: "Source".into(),
            method,
            bytes,
        };
        db.add_reuse(&[row("a", 0, 0, 100), row("a", 1, 3, 5), row("b", 0, 1, 7)])
            .unwrap();
        db.add_reuse(&[row("a", 0, 3, 20)]).unwrap();
        let mut rows = db.reuse().unwrap();
        rows.sort_by(|x, y| (&x.torrent, x.file).cmp(&(&y.torrent, y.file)));
        assert_eq!(rows[0], row("a", 0, 0, 120), "a bookend after a hard link");
        assert_eq!(rows.len(), 3);
        db.delete_reuse("a").unwrap();
        assert_eq!(db.reuse().unwrap(), [row("b", 0, 1, 7)]);
    }

    /// A database from before spaces: everything lands in `default`, seeding rows and all.
    #[test]
    fn upgrading_puts_everything_in_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "foreign_keys", "ON").unwrap();
            for sql in &MIGRATIONS[..9] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 9).unwrap();
            conn.execute(
                "INSERT INTO torrents (id, name, torrent, save_path, added_at, label)
                 VALUES ('a', 'A', x'01', '/d', 1, 'linux')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO seeding (torrent, seed_secs, reached) VALUES ('a', 60, 1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO superseded (torrent, by_torrent) VALUES ('a', 'b')",
                [],
            )
            .unwrap();
        }
        let db = Database::open(&path).unwrap();
        let rows = db.torrents().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].space, rows[0].id.as_str()), (1, "a"));
        assert_eq!(rows[0].label.as_deref(), Some("linux"));
        let seeding = db.seed_states().unwrap();
        assert_eq!(seeding.len(), 1);
        assert_eq!(seeding[0].0, Key::new(1, "a"));
        assert_eq!(seeding[0].1.seed_ms, 60_000);
        assert_eq!(
            db.superseded().unwrap(),
            [(Key::new(1, "a"), "b".to_owned())]
        );
        assert_eq!(db.spaces().unwrap()[0].slug, "default");
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

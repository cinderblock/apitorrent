//! `automation.db`: candidates, rules and what they took, aliases, channels and
//! subscriptions. SQLite in WAL mode with its own forward-only migrations, apart from
//! `apitorrent.db` so neither file's migrations can collide with the other's.
//!
//! Specs (rules, channels, subscriptions) are stored as JSON: they are only ever read
//! whole, and new optional fields need no migration.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use apitorrent_types::automation::{
    AliasMethod, ChannelSpec, ReleaseCandidate, RuleItem, RuleItemState, RuleSpec, StoredCandidate,
    SubscriptionSpec,
};
use apitorrent_types::spaces::SpaceId;

const MIGRATIONS: &[&str] = &[
    // 1: everything.
    "
    CREATE TABLE candidates (
        source     TEXT NOT NULL,
        id         TEXT NOT NULL,
        candidate  TEXT NOT NULL,
        metainfo   BLOB,
        first_seen INTEGER NOT NULL,
        last_seen  INTEGER NOT NULL,
        PRIMARY KEY (source, id)
    );
    CREATE TABLE rules (
        id         TEXT PRIMARY KEY,
        spec       TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        error      TEXT
    );
    CREATE TABLE rule_items (
        rule_id       TEXT NOT NULL REFERENCES rules(id) ON DELETE CASCADE,
        source        TEXT NOT NULL,
        candidate_id  TEXT NOT NULL,
        title         TEXT NOT NULL,
        version       TEXT,
        torrent_id    TEXT,
        state         TEXT NOT NULL,
        error         TEXT,
        taken_at      INTEGER NOT NULL,
        updated_at    INTEGER NOT NULL,
        rank          INTEGER,
        files_pending INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (rule_id, source, candidate_id)
    );
    CREATE TABLE aliases (
        rule_id      TEXT PRIMARY KEY REFERENCES rules(id) ON DELETE CASCADE,
        path         TEXT NOT NULL,
        torrent_id   TEXT,
        file         TEXT,
        source       TEXT,
        candidate_id TEXT,
        method       TEXT,
        updated_at   INTEGER,
        error        TEXT
    );
    CREATE TABLE channels (
        id           TEXT PRIMARY KEY,
        spec         TEXT NOT NULL,
        revision     INTEGER NOT NULL DEFAULT 0,
        entries_hash BLOB,
        entries      INTEGER NOT NULL DEFAULT 0,
        updated_at   INTEGER NOT NULL
    );
    CREATE TABLE subscriptions (
        id            TEXT PRIMARY KEY,
        spec          TEXT NOT NULL,
        token         TEXT,
        publisher_key TEXT,
        revision      INTEGER,
        entries       INTEGER NOT NULL DEFAULT 0,
        last_checked  INTEGER,
        last_error    TEXT,
        created_at    INTEGER NOT NULL
    );
    ",
    // 2: spaces. Rules, channels and subscriptions belong to one (1 is `default`, which
    // holds everything from before). A candidate belongs to the space whose source offered
    // it, or to every space (0) when its source is global.
    "
    ALTER TABLE rules ADD COLUMN space_id INTEGER NOT NULL DEFAULT 1;
    ALTER TABLE channels ADD COLUMN space_id INTEGER NOT NULL DEFAULT 1;
    ALTER TABLE subscriptions ADD COLUMN space_id INTEGER NOT NULL DEFAULT 1;
    CREATE TABLE candidates_v2 (
        space_id   INTEGER NOT NULL DEFAULT 0,
        source     TEXT NOT NULL,
        id         TEXT NOT NULL,
        candidate  TEXT NOT NULL,
        metainfo   BLOB,
        first_seen INTEGER NOT NULL,
        last_seen  INTEGER NOT NULL,
        PRIMARY KEY (space_id, source, id)
    );
    INSERT INTO candidates_v2 (space_id, source, id, candidate, metainfo, first_seen, last_seen)
        SELECT CASE WHEN source LIKE 'channel:%' THEN 1 ELSE 0 END,
               source, id, candidate, metainfo, first_seen, last_seen
        FROM candidates;
    DROP TABLE candidates;
    ALTER TABLE candidates_v2 RENAME TO candidates;
    ",
];

/// Candidates of every space: what a global source (a plugin that isn't space-aware)
/// offers.
pub const ALL_SPACES: SpaceId = 0;

/// A stored rule.
#[derive(Debug, Clone)]
pub struct RuleRow {
    pub id: String,
    pub space: SpaceId,
    pub spec: RuleSpec,
    pub created_at: i64,
    pub updated_at: i64,
    pub error: Option<String>,
}

/// What a rule took, with the bookkeeping the API doesn't show.
#[derive(Debug, Clone)]
pub struct ItemRow {
    pub item: RuleItem,
    pub updated_at: i64,
    /// 1 for the newest at the last pass; `None` once it no longer matches.
    pub rank: Option<u32>,
    /// A magnet link taken with a file selection: priorities wait for the metadata.
    pub files_pending: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AliasRow {
    pub path: String,
    pub torrent_id: Option<String>,
    pub file: Option<String>,
    pub source: Option<String>,
    pub candidate_id: Option<String>,
    pub method: Option<AliasMethod>,
    pub updated_at: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ChannelRow {
    pub id: String,
    pub space: SpaceId,
    pub spec: ChannelSpec,
    pub revision: u64,
    pub entries_hash: Option<Vec<u8>>,
    pub entries: u32,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct SubscriptionRow {
    pub id: String,
    pub space: SpaceId,
    /// Stored without its token.
    pub spec: SubscriptionSpec,
    pub token: Option<String>,
    pub publisher_key: Option<String>,
    pub revision: Option<u64>,
    pub entries: u32,
    pub last_checked: Option<i64>,
    pub last_error: Option<String>,
    pub created_at: i64,
}

/// A `channels` row as read: id, spec, revision, entries hash, entries, updated, space.
type ChannelColumns = (String, String, i64, Option<Vec<u8>>, i64, i64, SpaceId);

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("stored JSON doesn't parse: {0}")]
    Json(#[from] serde_json::Error),
}

pub type DbResult<T> = Result<T, DbError>;

#[derive(Debug)]
pub struct AutoDb {
    conn: Connection,
}

fn state_str(s: RuleItemState) -> &'static str {
    match s {
        RuleItemState::Active => "active",
        RuleItemState::Superseded => "superseded",
        RuleItemState::Gone => "gone",
        RuleItemState::Failed => "failed",
    }
}

fn state_from(s: &str) -> RuleItemState {
    match s {
        "superseded" => RuleItemState::Superseded,
        "gone" => RuleItemState::Gone,
        "failed" => RuleItemState::Failed,
        _ => RuleItemState::Active,
    }
}

fn method_str(m: AliasMethod) -> &'static str {
    match m {
        AliasMethod::HardLink => "hardLink",
        AliasMethod::Copy => "copy",
    }
}

fn method_from(s: &str) -> Option<AliasMethod> {
    match s {
        "hardLink" => Some(AliasMethod::HardLink),
        "copy" => Some(AliasMethod::Copy),
        _ => None,
    }
}

fn u64_of(v: Option<i64>) -> Option<u64> {
    v.and_then(|v| u64::try_from(v).ok())
}

/// Make the database and its WAL files readable by the owner only.
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

fn i64_of(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

impl AutoDb {
    pub fn open(path: &Path) -> DbResult<Self> {
        let conn = Connection::open(path)?;
        let db = Self::init(conn)?;
        // It holds subscription tokens for other instances: owner-only, like `auth.db`. On
        // Windows the data directory is already per-user.
        restrict_permissions(path);
        Ok(db)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> DbResult<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> DbResult<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let mut db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&mut self) -> DbResult<()> {
        let current: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        let current = usize::try_from(current).unwrap_or(0);
        if current > MIGRATIONS.len() {
            return Err(DbError::Sqlite(rusqlite::Error::InvalidParameterName(
                format!(
                    "automation.db is at schema version {current}, newer than this build \
                     knows ({})",
                    MIGRATIONS.len()
                ),
            )));
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", i64::try_from(i + 1).expect("fits"))?;
            tx.commit()?;
        }
        Ok(())
    }

    // --- Candidates ---------------------------------------------------------------------

    /// Insert or update candidates from `source`. `metainfo` (one per candidate) replaces
    /// the stored `.torrent` when given.
    pub fn upsert_candidates(
        &mut self,
        space: SpaceId,
        source: &str,
        candidates: &[(ReleaseCandidate, Option<Vec<u8>>)],
        now: i64,
    ) -> DbResult<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO candidates (source, id, candidate, metainfo, first_seen, last_seen,
                     space_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)
                 ON CONFLICT (space_id, source, id) DO UPDATE SET
                    candidate = excluded.candidate,
                    metainfo = COALESCE(excluded.metainfo, candidates.metainfo),
                    last_seen = excluded.last_seen",
            )?;
            for (c, meta) in candidates {
                stmt.execute(params![
                    source,
                    c.id,
                    serde_json::to_string(c)?,
                    meta.as_deref(),
                    now,
                    space
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn candidate_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(String, String, bool, i64, i64)> {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    }

    /// Candidates a space sees (its own and every space's; all of them for `None`), newest
    /// first by when they were last seen; from `source` only if given.
    pub fn candidates(
        &self,
        space: Option<SpaceId>,
        source: Option<&str>,
    ) -> DbResult<Vec<StoredCandidate>> {
        let sql = "SELECT source, candidate, metainfo IS NOT NULL, first_seen, last_seen
                   FROM candidates WHERE (?1 IS NULL OR source = ?1)
                       AND (?2 IS NULL OR space_id = ?2 OR space_id = 0)
                   ORDER BY last_seen DESC, source, id";
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map(params![source, space], Self::candidate_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(source, json, has_metainfo, first_seen, last_seen)| {
                Ok(StoredCandidate {
                    source,
                    candidate: serde_json::from_str(&json)?,
                    has_metainfo,
                    first_seen,
                    last_seen,
                })
            })
            .collect()
    }

    pub fn candidate_metainfo(
        &self,
        space: SpaceId,
        source: &str,
        id: &str,
    ) -> DbResult<Option<Vec<u8>>> {
        Ok(self
            .conn
            .query_row(
                "SELECT metainfo FROM candidates WHERE source = ?1 AND id = ?2
                     AND (space_id = ?3 OR space_id = 0)
                 ORDER BY space_id DESC LIMIT 1",
                params![source, id, space],
                |r| r.get::<_, Option<Vec<u8>>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Forget a source's candidates in one space (every space's for `None`).
    pub fn delete_candidates(&self, space: Option<SpaceId>, source: &str) -> DbResult<usize> {
        Ok(self.conn.execute(
            "DELETE FROM candidates WHERE source = ?1 AND (?2 IS NULL OR space_id = ?2)",
            params![source, space],
        )?)
    }

    /// Forget candidates not seen since `before`, except ones a rule still keeps.
    pub fn prune_candidates(&self, before: i64) -> DbResult<usize> {
        Ok(self.conn.execute(
            "DELETE FROM candidates WHERE last_seen < ?1 AND NOT EXISTS (
                SELECT 1 FROM rule_items i
                WHERE i.source = candidates.source AND i.candidate_id = candidates.id
                  AND i.state = 'active')",
            params![before],
        )?)
    }

    /// Keep at most `max` candidates per source and space, dropping the longest unseen.
    pub fn cap_candidates(&self, space: SpaceId, source: &str, max: usize) -> DbResult<usize> {
        Ok(self.conn.execute(
            "DELETE FROM candidates WHERE source = ?1 AND space_id = ?3 AND id NOT IN (
                SELECT id FROM candidates WHERE source = ?1 AND space_id = ?3
                ORDER BY last_seen DESC, id LIMIT ?2)",
            params![source, i64::try_from(max).unwrap_or(i64::MAX), space],
        )?)
    }

    // --- Rules --------------------------------------------------------------------------

    pub fn insert_rule(&self, space: SpaceId, id: &str, spec: &RuleSpec, now: i64) -> DbResult<()> {
        self.conn.execute(
            "INSERT INTO rules (id, spec, created_at, updated_at, space_id)
             VALUES (?1, ?2, ?3, ?3, ?4)",
            params![id, serde_json::to_string(spec)?, now, space],
        )?;
        Ok(())
    }

    /// Whether a space has rules, channels or subscriptions.
    pub fn space_in_use(&self, space: SpaceId) -> DbResult<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT (SELECT COUNT(*) FROM rules WHERE space_id = ?1)
                  + (SELECT COUNT(*) FROM channels WHERE space_id = ?1)
                  + (SELECT COUNT(*) FROM subscriptions WHERE space_id = ?1)",
            params![space],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Returns whether the rule exists.
    pub fn update_rule(&self, id: &str, spec: &RuleSpec, now: i64) -> DbResult<bool> {
        Ok(self.conn.execute(
            "UPDATE rules SET spec = ?2, updated_at = ?3, error = NULL WHERE id = ?1",
            params![id, serde_json::to_string(spec)?, now],
        )? > 0)
    }

    pub fn set_rule_error(&self, id: &str, error: Option<&str>) -> DbResult<()> {
        self.conn.execute(
            "UPDATE rules SET error = ?2 WHERE id = ?1",
            params![id, error],
        )?;
        Ok(())
    }

    pub fn delete_rule(&self, id: &str) -> DbResult<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM rules WHERE id = ?1", params![id])?
            > 0)
    }

    pub fn rules(&self) -> DbResult<Vec<RuleRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, spec, created_at, updated_at, error, space_id FROM rules
             ORDER BY created_at, id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get::<_, SpaceId>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(id, spec, created_at, updated_at, error, space)| {
                Ok(RuleRow {
                    id,
                    space,
                    spec: serde_json::from_str(&spec)?,
                    created_at,
                    updated_at,
                    error,
                })
            })
            .collect()
    }

    pub fn rule(&self, id: &str) -> DbResult<Option<RuleRow>> {
        Ok(self.rules()?.into_iter().find(|r| r.id == id))
    }

    pub fn items(&self, rule_id: &str) -> DbResult<Vec<ItemRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT source, candidate_id, title, version, torrent_id, state, error, taken_at,
                    updated_at, rank, files_pending
             FROM rule_items WHERE rule_id = ?1
             ORDER BY rank IS NULL, rank, taken_at DESC",
        )?;
        let rows = stmt
            .query_map(params![rule_id], |r| {
                Ok(ItemRow {
                    item: RuleItem {
                        source: r.get(0)?,
                        candidate_id: r.get(1)?,
                        title: r.get(2)?,
                        version: r.get(3)?,
                        torrent_id: r.get(4)?,
                        state: state_from(&r.get::<_, String>(5)?),
                        error: r.get(6)?,
                        taken_at: r.get(7)?,
                    },
                    updated_at: r.get(8)?,
                    rank: r
                        .get::<_, Option<i64>>(9)?
                        .and_then(|v| u32::try_from(v).ok()),
                    files_pending: r.get(10)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn save_item(&self, rule_id: &str, row: &ItemRow) -> DbResult<()> {
        let i = &row.item;
        self.conn.execute(
            "INSERT INTO rule_items (rule_id, source, candidate_id, title, version, torrent_id,
                state, error, taken_at, updated_at, rank, files_pending)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT (rule_id, source, candidate_id) DO UPDATE SET
                title = excluded.title, version = excluded.version,
                torrent_id = excluded.torrent_id, state = excluded.state,
                error = excluded.error, updated_at = excluded.updated_at,
                rank = excluded.rank, files_pending = excluded.files_pending",
            params![
                rule_id,
                i.source,
                i.candidate_id,
                i.title,
                i.version,
                i.torrent_id,
                state_str(i.state),
                i.error,
                i.taken_at,
                row.updated_at,
                row.rank.map(i64::from),
                row.files_pending,
            ],
        )?;
        Ok(())
    }

    // --- Aliases ------------------------------------------------------------------------

    pub fn alias(&self, rule_id: &str) -> DbResult<Option<AliasRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT path, torrent_id, file, source, candidate_id, method, updated_at, error
                 FROM aliases WHERE rule_id = ?1",
                params![rule_id],
                |r| {
                    Ok(AliasRow {
                        path: r.get(0)?,
                        torrent_id: r.get(1)?,
                        file: r.get(2)?,
                        source: r.get(3)?,
                        candidate_id: r.get(4)?,
                        method: r
                            .get::<_, Option<String>>(5)?
                            .as_deref()
                            .and_then(method_from),
                        updated_at: r.get(6)?,
                        error: r.get(7)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn save_alias(&self, rule_id: &str, a: &AliasRow) -> DbResult<()> {
        self.conn.execute(
            "INSERT INTO aliases (rule_id, path, torrent_id, file, source, candidate_id, method,
                updated_at, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT (rule_id) DO UPDATE SET
                path = excluded.path, torrent_id = excluded.torrent_id, file = excluded.file,
                source = excluded.source, candidate_id = excluded.candidate_id,
                method = excluded.method, updated_at = excluded.updated_at,
                error = excluded.error",
            params![
                rule_id,
                a.path,
                a.torrent_id,
                a.file,
                a.source,
                a.candidate_id,
                a.method.map(method_str),
                a.updated_at,
                a.error
            ],
        )?;
        Ok(())
    }

    pub fn delete_alias(&self, rule_id: &str) -> DbResult<()> {
        self.conn
            .execute("DELETE FROM aliases WHERE rule_id = ?1", params![rule_id])?;
        Ok(())
    }

    /// Whether any rule's alias was made at `path` (so it may be replaced).
    pub fn alias_owns(&self, path: &str) -> DbResult<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM aliases WHERE path = ?1 AND updated_at IS NOT NULL",
                params![path],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    // --- Channels -----------------------------------------------------------------------

    fn channel_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<ChannelColumns> {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
        ))
    }

    pub fn channels(&self) -> DbResult<Vec<ChannelRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, spec, revision, entries_hash, entries, updated_at, space_id FROM channels
             ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], Self::channel_from)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(
                |(id, spec, revision, entries_hash, entries, updated_at, space)| {
                    Ok(ChannelRow {
                        id,
                        space,
                        spec: serde_json::from_str(&spec)?,
                        revision: u64_of(Some(revision)).unwrap_or(0),
                        entries_hash,
                        entries: u32::try_from(entries).unwrap_or(0),
                        updated_at,
                    })
                },
            )
            .collect()
    }

    pub fn channel(&self, id: &str) -> DbResult<Option<ChannelRow>> {
        Ok(self.channels()?.into_iter().find(|c| c.id == id))
    }

    /// Returns false if the id is taken.
    pub fn insert_channel(
        &self,
        space: SpaceId,
        id: &str,
        spec: &ChannelSpec,
        now: i64,
    ) -> DbResult<bool> {
        Ok(self.conn.execute(
            "INSERT OR IGNORE INTO channels (id, spec, updated_at, space_id)
             VALUES (?1, ?2, ?3, ?4)",
            params![id, serde_json::to_string(spec)?, now, space],
        )? > 0)
    }

    /// Changing what a channel selects bumps nothing by itself; the next feed does, if the
    /// entries changed.
    pub fn update_channel(&self, id: &str, spec: &ChannelSpec) -> DbResult<bool> {
        Ok(self.conn.execute(
            "UPDATE channels SET spec = ?2 WHERE id = ?1",
            params![id, serde_json::to_string(spec)?],
        )? > 0)
    }

    pub fn set_channel_revision(
        &self,
        id: &str,
        revision: u64,
        entries_hash: &[u8],
        entries: u32,
        now: i64,
    ) -> DbResult<()> {
        self.conn.execute(
            "UPDATE channels SET revision = ?2, entries_hash = ?3, entries = ?4, updated_at = ?5
             WHERE id = ?1",
            params![id, i64_of(revision), entries_hash, entries, now],
        )?;
        Ok(())
    }

    pub fn delete_channel(&self, id: &str) -> DbResult<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM channels WHERE id = ?1", params![id])?
            > 0)
    }

    // --- Subscriptions ------------------------------------------------------------------

    pub fn subscriptions(&self) -> DbResult<Vec<SubscriptionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, spec, token, publisher_key, revision, entries, last_checked, last_error,
                    created_at, space_id
             FROM subscriptions ORDER BY created_at, id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, SpaceId>(9)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(
                |(
                    id,
                    spec,
                    token,
                    publisher_key,
                    revision,
                    entries,
                    last_checked,
                    last_error,
                    created_at,
                    space,
                )| {
                    Ok(SubscriptionRow {
                        id,
                        space,
                        spec: serde_json::from_str(&spec)?,
                        token,
                        publisher_key,
                        revision: u64_of(revision),
                        entries: u32::try_from(entries).unwrap_or(0),
                        last_checked,
                        last_error,
                        created_at,
                    })
                },
            )
            .collect()
    }

    pub fn subscription(&self, id: &str) -> DbResult<Option<SubscriptionRow>> {
        Ok(self.subscriptions()?.into_iter().find(|s| s.id == id))
    }

    /// Insert or replace a subscription's settings, token and pinned key, keeping its
    /// progress.
    pub fn save_subscription(&self, row: &SubscriptionRow) -> DbResult<()> {
        let mut spec = row.spec.clone();
        spec.token = None;
        self.conn.execute(
            "INSERT INTO subscriptions (id, spec, token, publisher_key, created_at, space_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (id) DO UPDATE SET spec = excluded.spec, token = excluded.token,
                publisher_key = excluded.publisher_key",
            params![
                row.id,
                serde_json::to_string(&spec)?,
                row.token,
                row.publisher_key,
                row.created_at,
                row.space
            ],
        )?;
        Ok(())
    }

    /// Record a poll. `revision` and `entries` only when a feed was accepted; a newly
    /// pinned key with `publisher_key`.
    pub fn record_poll(
        &self,
        id: &str,
        now: i64,
        error: Option<&str>,
        accepted: Option<(u64, u32)>,
        publisher_key: Option<&str>,
    ) -> DbResult<()> {
        self.conn.execute(
            "UPDATE subscriptions SET last_checked = ?2, last_error = ?3 WHERE id = ?1",
            params![id, now, error],
        )?;
        if let Some((revision, entries)) = accepted {
            self.conn.execute(
                "UPDATE subscriptions SET revision = ?2, entries = ?3 WHERE id = ?1",
                params![id, i64_of(revision), entries],
            )?;
        }
        if let Some(k) = publisher_key {
            self.conn.execute(
                "UPDATE subscriptions SET publisher_key = ?2 WHERE id = ?1 AND publisher_key IS NULL",
                params![id, k],
            )?;
        }
        Ok(())
    }

    pub fn delete_subscription(&self, id: &str) -> DbResult<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM subscriptions WHERE id = ?1", params![id])?
            > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apitorrent_types::automation::CandidateLinks;

    fn cand(id: &str) -> ReleaseCandidate {
        ReleaseCandidate {
            id: id.into(),
            title: format!("release {id}"),
            version: Some(id.into()),
            published: None,
            links: CandidateLinks {
                magnet: Some("magnet:?xt=urn:btih:00".into()),
                ..CandidateLinks::default()
            },
            metadata: serde_json::Map::new(),
        }
    }

    #[test]
    fn candidates_upsert_keep_metainfo_and_prune() {
        let mut db = AutoDb::open_in_memory().unwrap();
        db.upsert_candidates(
            ALL_SPACES,
            "plugin:x",
            &[(cand("1"), Some(vec![1, 2])), (cand("2"), None)],
            10,
        )
        .unwrap();
        // A later submission without metainfo keeps the stored one.
        db.upsert_candidates(ALL_SPACES, "plugin:x", &[(cand("1"), None)], 20)
            .unwrap();
        assert_eq!(
            db.candidate_metainfo(1, "plugin:x", "1").unwrap(),
            Some(vec![1, 2])
        );
        let all = db.candidates(None, None).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].candidate.id, "1", "most recently seen first");
        assert_eq!(all[0].first_seen, 10);
        assert!(all[0].has_metainfo);
        assert_eq!(db.prune_candidates(15).unwrap(), 1);
        db.upsert_candidates(
            ALL_SPACES,
            "plugin:x",
            &[(cand("3"), None), (cand("4"), None)],
            30,
        )
        .unwrap();
        assert_eq!(db.cap_candidates(ALL_SPACES, "plugin:x", 2).unwrap(), 1);
        assert_eq!(db.candidates(None, Some("plugin:x")).unwrap().len(), 2);
    }

    #[test]
    fn a_space_sees_its_own_candidates_and_global_ones() {
        let mut db = AutoDb::open_in_memory().unwrap();
        db.upsert_candidates(ALL_SPACES, "plugin:global", &[(cand("g"), None)], 1)
            .unwrap();
        db.upsert_candidates(2, "plugin:rss", &[(cand("a"), None)], 1)
            .unwrap();
        db.upsert_candidates(3, "plugin:rss", &[(cand("a"), None)], 1)
            .unwrap();
        let ids = |space| {
            let mut v: Vec<String> = db
                .candidates(Some(space), None)
                .unwrap()
                .into_iter()
                .map(|c| format!("{}/{}", c.source, c.candidate.id))
                .collect();
            v.sort();
            v
        };
        assert_eq!(ids(2), ["plugin:global/g", "plugin:rss/a"]);
        assert_eq!(ids(1), ["plugin:global/g"]);
        assert_eq!(db.candidates(None, None).unwrap().len(), 3);
        assert_eq!(db.delete_candidates(Some(2), "plugin:rss").unwrap(), 1);
        assert_eq!(ids(3), ["plugin:global/g", "plugin:rss/a"]);
    }
}

//! apitorrent's core: the torrent registry above the engine.
//!
//! The core owns the instance database, adds and restores torrents in the engine, keeps the
//! revisioned store that clients sync from, and saves resume state. The API server is a thin
//! layer over [`Core`].

pub mod adapters;
pub mod automation;
pub mod db;
pub mod dedupe;
pub mod deluge_import;
pub mod fingerprint;
pub mod folders;
pub mod import;
mod jobs;
pub mod library;
mod library_index;
pub mod passkeys;
pub mod plugin_api;
pub mod ports;
mod roots;
pub mod seeding;
pub mod settings;
mod share;
pub mod spaces;
pub mod store;
pub mod takeover;

pub use jobs::Progress;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use tokio::sync::{Notify, broadcast, oneshot, watch};

use apitorrent_engine::bitfield::Bitfield;
use apitorrent_engine::layout::Layout;
use apitorrent_engine::metadata::FetchStatus;
use apitorrent_engine::storage::Placement;
use apitorrent_engine::{
    AddTorrent, EntryKey, EntryParams, EntryStats, Session, SessionConfig, TorrentHandle,
    TorrentState as EngineState, UnwantedPolicy as EnginePolicy,
};
use apitorrent_metainfo::sanitize::Naming;
use apitorrent_metainfo::{Magnet, Torrent};
use apitorrent_types::bulk::{BulkEdit, BulkFailure, BulkReport, TorrentSettings};
use apitorrent_types::spaces::{DEFAULT_SPACE, Space, SpaceId};
use apitorrent_types::{
    AddSource, AddTorrentRequest, FilePlacement, FilePriority, QueueMove, Settings, TorrentDetail,
    TorrentFile, TorrentId, TorrentLimits, TorrentState, TorrentSummary, UnwantedPolicy,
};

use crate::db::{Database, ResumeRow, TorrentRow};
pub use crate::store::{Patch, Snapshot, Store};

/// How often summaries are refreshed from the engine.
const REFRESH: Duration = Duration::from_secs(1);
/// How often resume state is saved.
const SAVE_EVERY: Duration = Duration::from_secs(30);
/// How often the fingerprinter looks for files to fingerprint.
const FINGERPRINT_EVERY: Duration = Duration::from_secs(5 * 60);
/// How often superseded torrents are checked for retirement.
const RETIRE_EVERY: Duration = Duration::from_secs(60);

/// A torrent entry: a torrent in one space. The same torrent (info-hash) can be in several
/// spaces; they share one swarm in the engine (see `Swarm`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    pub space: SpaceId,
    pub id: TorrentId,
}

impl Key {
    pub fn new(space: SpaceId, id: impl Into<TorrentId>) -> Self {
        Self {
            space,
            id: id.into(),
        }
    }

    /// The entry's key in its engine torrent: the space id (one entry per space).
    fn engine(&self) -> EntryKey {
        u64::try_from(self.space).unwrap_or(0)
    }
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.space, self.id)
    }
}

/// The name a space's limits have in the engine.
fn engine_space(space: SpaceId) -> String {
    space.to_string()
}

#[derive(Debug, Clone)]
pub struct CoreConfig {
    /// Where the database lives.
    pub data_dir: PathBuf,
    /// The default save path for new torrents.
    pub download_dir: PathBuf,
    pub session: SessionConfig,
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("invalid torrent: {0}")]
    InvalidTorrent(String),
    #[error("the torrent is already added")]
    Duplicate,
    #[error("no such torrent")]
    NotFound,
    #[error("not supported yet: {0}")]
    Unsupported(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// The client it was imported from still runs it, and couldn't be made to stop.
    #[error("{0}")]
    OtherOwner(String),
    /// The same work is already running (an import into the space, say).
    #[error("{0}")]
    Busy(String),
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl CoreError {
    /// The stable error code used in API responses.
    pub fn code(&self) -> &'static str {
        match self {
            CoreError::InvalidTorrent(_) => "invalid_torrent",
            CoreError::Duplicate => "duplicate",
            CoreError::NotFound => "not_found",
            CoreError::Unsupported(_) => "unsupported",
            CoreError::InvalidRequest(_) => "invalid_request",
            CoreError::OtherOwner(_) => "other_owner",
            CoreError::Busy(_) => "busy",
            CoreError::Database(_) | CoreError::Io(_) => "internal",
        }
    }
}

struct Entry {
    torrent: Arc<Torrent>,
    /// The swarm's engine torrent, shared with the torrent's entries in other spaces.
    handle: TorrentHandle,
    save_path: PathBuf,
    /// This entry's announce list (from its own `.torrent`).
    trackers: Vec<Vec<String>>,
    /// Files placed in this entry's save path while it isn't the swarm's primary (see
    /// `place_secondaries`). The primary's files are the engine's own.
    placed: Vec<bool>,
    /// Placing a file failed; not tried again before then.
    place_retry: Option<std::time::Instant>,
    label: Option<String>,
    added_at: i64,
    paused: bool,
    file_priorities: Vec<u8>,
    unwanted: UnwantedPolicy,
    /// Totals from previous sessions; the engine's counters start at zero each run.
    downloaded_base: u64,
    uploaded_base: u64,
    queue_position: u32,
    limits: TorrentLimits,
    /// Paused by the queue (not by the user) until a slot frees up.
    queued: bool,
    /// Since when a running torrent has been below the queue's slow rate.
    slow_since: Option<std::time::Instant>,
    group: Option<String>,
    naming: u8,
    superseded_by: Option<TorrentId>,
    /// Imported from Deluge and not taken over yet: the Deluge config directory.
    in_deluge: Option<String>,
    /// Imported from a `.torrent` file Deluge left but didn't list: its config directory.
    leftover_from: Option<String>,
    /// Per-file paths overriding the naming policy.
    file_paths: Option<Vec<Vec<String>>>,
}

/// The parts of a pending magnet that the torrent replacing it takes over.
struct PendingSnapshot {
    held_path: PathBuf,
    paused: bool,
    unwanted: UnwantedPolicy,
    label: Option<String>,
    added_at: i64,
    queue_position: u32,
}

/// A magnet link waiting for its metadata.
struct Pending {
    magnet: Magnet,
    uri: String,
    save_path: PathBuf,
    held_path: PathBuf,
    label: Option<String>,
    added_at: i64,
    paused: bool,
    unwanted: UnwantedPolicy,
    queue_position: u32,
    naming: u8,
    /// Progress of the fetch, and a way to cancel it; `None` while paused.
    fetch: Option<(watch::Receiver<FetchStatus>, oneshot::Sender<()>)>,
}

/// A torrent as the library sees it: id, metainfo, save path, naming, custom paths,
/// handle.
type LibrarySnapshot = (
    String,
    Arc<Torrent>,
    PathBuf,
    u8,
    Option<Vec<Vec<String>>>,
    TorrentHandle,
);

/// One engine torrent per info-hash, whatever the number of spaces holding it. Its storage
/// is the primary's: the oldest entry. The other entries get their files by link or copy as
/// they complete.
struct Swarm {
    handle: TorrentHandle,
    primary: SpaceId,
}

/// Everything needed to start a torrent in the engine and track it.
struct Spawn {
    key: Key,
    /// Peers to try straight away (found while fetching a magnet's metadata).
    peers: Vec<SocketAddr>,
    torrent: Arc<Torrent>,
    /// The entry's announce list (from its own `.torrent`).
    trackers: Vec<Vec<String>>,
    save_path: PathBuf,
    held_path: PathBuf,
    have: Option<Bitfield>,
    placements: Option<Vec<Placement>>,
    paused: bool,
    file_priorities: Vec<u8>,
    unwanted: UnwantedPolicy,
    label: Option<String>,
    added_at: i64,
    totals: (u64, u64),
    queue_position: u32,
    limits: TorrentLimits,
    /// Start paused by the queue; the next queue pass decides.
    queued: bool,
    naming: u8,
    /// Files hard-linked from other torrents (copy-on-write in the engine).
    linked: Vec<usize>,
    /// Per-file paths overriding the naming policy.
    file_paths: Option<Vec<Vec<String>>>,
}

struct Inner {
    config: CoreConfig,
    db: Arc<Mutex<Database>>,
    session: Session,
    torrents: RwLock<HashMap<Key, Entry>>,
    pending: RwLock<HashMap<Key, Pending>>,
    /// The engine torrents, by torrent id. Locked after `torrents` when both are needed.
    swarms: Mutex<HashMap<TorrentId, Swarm>>,
    spaces: RwLock<BTreeMap<SpaceId, Space>>,
    store: Store,
    stop: Notify,
    settings: RwLock<Settings>,
    /// Torrents whose library matches may supersede others, decided once their first
    /// check is done (see `settle_supersedes`).
    supersede_pending: Mutex<HashMap<Key, Vec<library::SupersedeCandidate>>>,
    /// Fingerprints by absolute path (see `fingerprint`), mirrored in the database.
    fingerprints: RwLock<HashMap<PathBuf, fingerprint::Fingerprint>>,
    /// Set at shutdown, so a fingerprint in progress stops reading.
    stopping: Arc<std::sync::atomic::AtomicBool>,
    /// Seeding time and seed limits per torrent (see `seeding`), saved with resume state.
    seeding: Mutex<HashMap<Key, seeding::SeedState>>,
    /// When seeding time was last counted.
    seed_clock: Mutex<std::time::Instant>,
    /// Storage roots; only their share locators change while running.
    roots: RwLock<Vec<apitorrent_types::StorageRoot>>,
    /// Rules, aliases and channels (see `automation`).
    automation: automation::Automation,
    /// Tracker adapters for pack switchover (see `adapters`).
    adapters: adapters::AdapterState,
    /// Retroactive dedupe and library folders (see `library_index`).
    library: library_index::LibraryState,
    /// Where the content library supplied files' bytes from (see `share`).
    reuse: share::ReuseBook,
    /// Long work running in the background, such as a Deluge import (see `jobs`).
    jobs: Arc<jobs::Jobs>,
}

pub(crate) const SETTINGS_KEY: &str = "settings";
/// The DHT's node id and good nodes, base64 of its own encoding.
const DHT_STATE_KEY: &str = "dht_state";

/// A running instance. Cheap to clone.
#[derive(Clone)]
pub struct Core {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Core {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Core")
            .field("data_dir", &self.inner.config.data_dir)
            .finish_non_exhaustive()
    }
}

impl Core {
    /// Open the database, start the engine and restore every saved torrent.
    pub async fn start(config: CoreConfig) -> Result<Self, CoreError> {
        std::fs::create_dir_all(&config.data_dir)?;
        std::fs::create_dir_all(&config.download_dir)?;
        let db_path = config.data_dir.join("apitorrent.db");
        let db = tokio::task::spawn_blocking(move || Database::open(&db_path))
            .await
            .expect("database open panicked")?;
        let root = roots::root_for(&db, &config.download_dir)?;
        roots::write_marker(&root);
        let stored = db.meta(SETTINGS_KEY)?;
        let settings = match stored.as_deref().map(serde_json::from_str::<Settings>) {
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                tracing::warn!(error = %e, "stored settings don't parse; using defaults");
                settings::defaults(&config.session)
            }
            None => settings::defaults(&config.session),
        };
        // The DHT starts from the nodes it knew last time, when there is saved state.
        let mut session_config = config.session.clone();
        if let Some(dht) = session_config.dht.as_mut()
            && dht.state.is_none()
            && let Some(saved) = db.meta(DHT_STATE_KEY)?
            && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(saved)
        {
            dht.state = apitorrent_dht::PersistedState::from_bytes(&bytes).ok();
        }
        let spaces: BTreeMap<SpaceId, Space> =
            db.spaces()?.into_iter().map(|s| (s.id, s)).collect();
        let automation = automation::Automation::open(&config.data_dir)?;
        let session = Session::start(session_config).await?;
        tracing::info!(peer_port = session.listen_addr().port(), "engine started");
        let core = Core {
            inner: Arc::new(Inner {
                config,
                db: Arc::new(Mutex::new(db)),
                session,
                torrents: RwLock::new(HashMap::new()),
                pending: RwLock::new(HashMap::new()),
                swarms: Mutex::new(HashMap::new()),
                spaces: RwLock::new(spaces),
                store: Store::new(),
                stop: Notify::new(),
                settings: RwLock::new(settings),
                supersede_pending: Mutex::new(HashMap::new()),
                fingerprints: RwLock::new(HashMap::new()),
                stopping: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                seeding: Mutex::new(HashMap::new()),
                seed_clock: Mutex::new(std::time::Instant::now()),
                roots: RwLock::new(vec![root]),
                automation,
                adapters: adapters::AdapterState::default(),
                library: library_index::LibraryState::default(),
                reuse: share::ReuseBook::default(),
                jobs: Arc::default(),
            }),
        };
        core.apply_session_settings();
        core.restore().await?;
        core.load_fingerprints().await;
        match core.db(|db| db.seed_states()).await {
            Ok(rows) => core
                .inner
                .seeding
                .lock()
                .expect("seeding lock poisoned")
                .extend(rows),
            Err(e) => tracing::warn!(error = %e, "could not load seeding state"),
        }
        match core.db(|db| db.reuse()).await {
            Ok(rows) => core.inner.reuse.load(rows),
            Err(e) => tracing::warn!(error = %e, "could not load file reuse records"),
        }
        core.apply_queue().await;
        tokio::spawn(core.clone().background());
        tokio::spawn(core.clone().fingerprinter());
        tokio::spawn(core.clone().sharer());
        tokio::spawn(core.clone().automation_loop());
        core.start_library().await;
        Ok(core)
    }

    pub fn settings(&self) -> Settings {
        self.inner
            .settings
            .read()
            .expect("settings lock poisoned")
            .clone()
    }

    /// Replace the instance settings. They are stored, and apply at once: rates,
    /// connection limits, groups (torrents are regrouped by label) and the queue.
    pub async fn set_settings(&self, new: Settings) -> Result<(), CoreError> {
        settings::validate(&new).map_err(CoreError::InvalidRequest)?;
        let json = serde_json::to_string(&new).expect("settings serialize");
        self.db(move |db| db.set_meta(SETTINGS_KEY, &json)).await?;
        *self.inner.settings.write().expect("settings lock poisoned") = new;
        self.apply_session_settings();
        self.regroup().await;
        self.apply_queue().await;
        self.refresh();
        Ok(())
    }

    /// Present as `identity` to trackers on `host`, unless a rule already covers that
    /// host (see [`Core::ensure_tracker_identities`]). Returns whether a rule was added.
    pub async fn ensure_tracker_identity(
        &self,
        host: &str,
        identity: apitorrent_types::ClientIdentity,
    ) -> Result<bool, CoreError> {
        let added = self
            .ensure_tracker_identities(&[host.to_owned()], identity)
            .await?;
        Ok(!added.is_empty())
    }

    /// Add a client identity rule (see [`apitorrent_types::IdentitySettings`]) for each of
    /// `hosts` that no rule covers yet: neither a rule for that host nor a `*.domain` rule
    /// that matches it. Rules the user made always win. Used by the Deluge import, which
    /// gives each tracker host its torrents use the Deluge identity they had there.
    ///
    /// The rules are stored and apply at once, like any settings change: torrents on those
    /// trackers announce with the new identity from their next announce. Returns the hosts
    /// that got a rule; bad host names are skipped.
    pub async fn ensure_tracker_identities(
        &self,
        hosts: &[String],
        identity: apitorrent_types::ClientIdentity,
    ) -> Result<Vec<String>, CoreError> {
        settings::client_profile(&identity)
            .map_err(|e| CoreError::InvalidRequest(e.to_string()))?;
        let mut s = self.settings();
        let added = settings::ensure_tracker_identities(&mut s.identity, hosts, &identity);
        if !added.is_empty() {
            tracing::info!(hosts = ?added, "added client identity rules");
            self.set_settings(s).await?;
        }
        Ok(added)
    }

    /// Push the settings' rates, connection limits and groups into the engine.
    fn apply_session_settings(&self) {
        let s = self.settings();
        let session = &self.inner.session;
        match settings::identity_rules(&s.identity) {
            Ok(rules) => session.set_identity_rules(rules),
            // Only stored settings from another version could get here; new ones are
            // validated. Torrents present as apitorrent until they are fixed.
            Err(e) => {
                tracing::warn!(error = %e, "client identity settings don't apply");
                session.set_identity_rules(apitorrent_engine::identity::IdentityRules::default());
            }
        }
        session.set_rate_limits(s.download_rate, s.upload_rate);
        session.set_encryption(settings::encryption_policy(s.encryption));
        session.set_connection_limits(
            s.max_connections as usize,
            s.max_half_open as usize,
            u64::from(s.connect_rate),
        );
        for (name, _) in session.groups() {
            if !s.groups.iter().any(|g| g.name == name) {
                session.remove_group(&name);
            }
        }
        for g in &s.groups {
            session.set_group(&g.name, settings::group_limits(g));
        }
        self.apply_space_limits();
    }

    /// Push every space's rates and connection limit into the engine.
    fn apply_space_limits(&self) {
        let session = &self.inner.session;
        for s in self
            .inner
            .spaces
            .read()
            .expect("spaces lock poisoned")
            .values()
        {
            session.set_space(&engine_space(s.id), spaces::engine_limits(&s.settings));
        }
    }

    /// Put every torrent in the group its label selects.
    async fn regroup(&self) {
        let s = self.settings();
        let changes: Vec<(TorrentHandle, EntryKey, Option<String>)> = {
            let mut torrents = self.inner.torrents.write().expect("torrents lock poisoned");
            torrents
                .iter_mut()
                .filter_map(|(key, e)| {
                    let group = settings::group_for(&s, e.label.as_deref()).map(str::to_owned);
                    (group != e.group).then(|| {
                        e.group.clone_from(&group);
                        (e.handle.clone(), key.engine(), group)
                    })
                })
                .collect()
        };
        for (handle, entry, group) in changes {
            handle.set_group(entry, group).await;
        }
    }

    /// Let the queue decide which torrents run: pause the ones over the limits, resume
    /// the ones that now fit. Torrents paused by the user or in error don't take part.
    async fn apply_queue(&self) {
        let q = self.settings().queue;
        let space_queues: HashMap<SpaceId, apitorrent_types::QueueSettings> = self
            .inner
            .spaces
            .read()
            .expect("spaces lock poisoned")
            .values()
            .map(|s| (s.id, spaces::queue_settings(&s.settings, &q)))
            .collect();
        let now = std::time::Instant::now();
        let slow_after = Duration::from_secs(u64::from(q.slow_after_secs));
        let mut actions = Vec::new();
        {
            let mut torrents = self.inner.torrents.write().expect("torrents lock poisoned");
            let mut by_space: BTreeMap<SpaceId, Vec<settings::QueueItem>> = BTreeMap::new();
            for (key, e) in torrents.iter_mut() {
                let stats = e.handle.stats();
                let mine = entry_stats(&stats, key);
                let running = !e.paused && !e.queued && stats.error.is_none();
                let below = if stats.finished {
                    mine.upload_rate < q.slow_upload_rate
                } else {
                    mine.download_rate < q.slow_download_rate
                };
                if running && below {
                    e.slow_since.get_or_insert(now);
                } else {
                    e.slow_since = None;
                }
                if !e.paused && stats.error.is_none() {
                    by_space
                        .entry(key.space)
                        .or_default()
                        .push(settings::QueueItem {
                            id: key.to_string(),
                            position: e.queue_position,
                            finished: stats.finished,
                            slow: q.ignore_slow
                                && e.slow_since.is_some_and(|t| now - t >= slow_after),
                        });
                }
            }
            // Each space's own slots first, then the instance's across the spaces.
            let mut chosen: Vec<settings::QueueItem> = Vec::new();
            for (space, items) in &by_space {
                let sq = space_queues.get(space).unwrap_or(&q);
                let active = settings::plan(items, sq);
                chosen.extend(items.iter().filter(|i| active.contains(&i.id)).cloned());
            }
            let active = settings::plan(&chosen, &q);
            for (key, e) in torrents.iter_mut() {
                if e.paused || e.handle.stats().error.is_some() {
                    continue;
                }
                let run = active.contains(&key.to_string());
                if run && e.queued {
                    e.queued = false;
                    actions.push((e.handle.clone(), key.engine(), true));
                } else if !run && !e.queued {
                    e.queued = true;
                    actions.push((e.handle.clone(), key.engine(), false));
                }
            }
        }
        for (handle, entry, run) in actions {
            if run {
                handle.resume_entry(entry).await;
            } else {
                handle.pause_entry(entry).await;
            }
        }
    }

    /// Change a torrent's own rate and connection limits.
    pub async fn set_torrent_limits(
        &self,
        key: &Key,
        limits: TorrentLimits,
    ) -> Result<(), CoreError> {
        let handle = self.handle(key)?;
        let k = key.clone();
        self.db(move |db| {
            db.set_limits(
                &k,
                limits.download_rate,
                limits.upload_rate,
                limits.max_connections,
            )
        })
        .await?;
        if let Some(e) = self
            .inner
            .torrents
            .write()
            .expect("torrents lock poisoned")
            .get_mut(key)
        {
            e.limits = limits;
        }
        handle.set_limits(key.engine(), engine_limits(limits)).await;
        Ok(())
    }

    /// Replace an entry's announce tiers. Each space's entry has its own list; a URL two
    /// running entries list is still announced once, by the older. Trackers that stay carry
    /// on where they were; dropped ones are told `stopped`. Tiers are trimmed and empty ones
    /// dropped. A magnet link still fetching its metadata can't be changed yet.
    pub async fn set_trackers(&self, key: &Key, tiers: Vec<Vec<String>>) -> Result<(), CoreError> {
        let tiers = clean_tiers(tiers)?;
        if self
            .inner
            .pending
            .read()
            .expect("pending lock poisoned")
            .contains_key(key)
        {
            return Err(CoreError::InvalidRequest(
                "its trackers can be changed once its metadata has arrived".into(),
            ));
        }
        let (handle, own) = {
            let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
            let e = torrents.get(key).ok_or(CoreError::NotFound)?;
            (e.handle.clone(), e.torrent.trackers.clone())
        };
        // The .torrent's own list again is stored as no list of its own.
        let stored =
            (tiers != own).then(|| serde_json::to_string(&tiers).expect("tiers serialize"));
        let k = key.clone();
        self.db(move |db| db.set_trackers(&k, stored.as_deref()))
            .await?;
        if let Some(e) = self
            .inner
            .torrents
            .write()
            .expect("torrents lock poisoned")
            .get_mut(key)
        {
            e.trackers.clone_from(&tiers);
        }
        handle.set_trackers(key.engine(), tiers).await;
        self.refresh();
        Ok(())
    }

    /// The tracker passkeys in the space's announce URLs (see [`passkeys`]).
    #[must_use]
    pub fn tracker_passkeys(&self, space: SpaceId) -> Vec<apitorrent_types::TrackerPasskey> {
        let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
        passkeys::list(
            torrents
                .iter()
                .filter(|(k, _)| k.space == space)
                .map(|(_, e)| e.trackers.as_slice()),
        )
    }

    /// Put a new passkey in place of the passkey `req.id` on every torrent in the space
    /// that uses it. Neither passkey is logged or put in an error.
    pub async fn replace_tracker_passkey(
        &self,
        space: SpaceId,
        req: apitorrent_types::ReplacePasskeyRequest,
    ) -> Result<apitorrent_types::ReplacePasskeyReport, CoreError> {
        let new = req.new_passkey.trim();
        let mut old = None;
        let using: Vec<(Key, String, Vec<Vec<String>>)> = {
            let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
            torrents
                .iter()
                .filter(|(k, _)| k.space == space)
                .filter_map(|(k, e)| {
                    let key = passkeys::key_of(&e.trackers, &req.id)?;
                    old.get_or_insert_with(|| key.to_owned());
                    Some((k.clone(), e.torrent.info.name.clone(), e.trackers.clone()))
                })
                .collect()
        };
        let Some(old) = old else {
            return Err(CoreError::NotFound);
        };
        for (_, _, tiers) in &using {
            if let Some(why) = passkeys::unacceptable(tiers, &old, new) {
                return Err(CoreError::InvalidRequest(why.into()));
            }
        }
        let mut report = apitorrent_types::ReplacePasskeyReport::default();
        // Magnets still fetching their metadata keep the magnet's trackers until then.
        report.skipped.extend(
            self.inner
                .pending
                .read()
                .expect("pending lock poisoned")
                .iter()
                .filter(|(k, p)| {
                    k.space == space
                        && passkeys::key_of(std::slice::from_ref(&p.magnet.trackers), &req.id)
                            .is_some()
                })
                .map(|(k, p)| apitorrent_types::ImportSkip {
                    name: p
                        .magnet
                        .display_name
                        .clone()
                        .unwrap_or_else(|| k.id.clone()),
                    reason: "still fetching its metadata; change it once that arrives".into(),
                }),
        );
        for (key, name, tiers) in using {
            match self
                .set_trackers(&key, passkeys::replaced(&tiers, &old, new))
                .await
            {
                Ok(()) => report.changed += 1,
                Err(e) => report.skipped.push(apitorrent_types::ImportSkip {
                    name,
                    reason: apitorrent_tracker::passkey::redact_text(&e.to_string()).into_owned(),
                }),
            }
        }
        tracing::info!(
            changed = report.changed,
            skipped = report.skipped.len(),
            "replaced a tracker passkey"
        );
        Ok(report)
    }

    /// Change a torrent's label, which also picks its group.
    pub async fn set_label(&self, key: &Key, label: Option<String>) -> Result<(), CoreError> {
        self.store_label(key, label).await?;
        self.regroup().await;
        self.refresh();
        Ok(())
    }

    /// Change a torrent's label without regrouping: [`Self::set_label`], or many of them
    /// followed by one regroup.
    async fn store_label(&self, key: &Key, label: Option<String>) -> Result<(), CoreError> {
        let label = label.map(|l| l.trim().to_owned()).filter(|l| !l.is_empty());
        if let Some(p) = self
            .inner
            .pending
            .write()
            .expect("pending lock poisoned")
            .get_mut(key)
        {
            p.label.clone_from(&label);
        } else {
            self.handle(key)?;
        }
        let k = key.clone();
        let stored = label.clone();
        self.db(move |db| db.set_label(&k, stored.as_deref()))
            .await?;
        if let Some(e) = self
            .inner
            .torrents
            .write()
            .expect("torrents lock poisoned")
            .get_mut(key)
        {
            e.label = label;
        }
        Ok(())
    }

    /// Change settings on several of a space's torrents (see [`BulkEdit`]). Each torrent
    /// is changed on its own, and one failing leaves the others changed.
    pub async fn bulk_edit(&self, space: SpaceId, edit: BulkEdit) -> BulkReport {
        let mut report = BulkReport::default();
        let mut relabelled = false;
        for id in &edit.ids {
            let key = Key::new(space, id.as_str());
            let outcome = self.bulk_edit_one(&key, &edit, &mut relabelled).await;
            match outcome {
                Ok(()) => report.updated.push(id.clone()),
                Err(e) => report.failed.push(BulkFailure {
                    id: id.clone(),
                    code: e.code().to_owned(),
                    message: match e {
                        CoreError::Database(_) | CoreError::Io(_) => {
                            tracing::error!(torrent = %id, error = %e, "bulk edit failed");
                            "internal error".to_owned()
                        }
                        e => e.to_string(),
                    },
                }),
            }
        }
        if relabelled {
            self.regroup().await;
        }
        self.refresh();
        report
    }

    async fn bulk_edit_one(
        &self,
        key: &Key,
        edit: &BulkEdit,
        relabelled: &mut bool,
    ) -> Result<(), CoreError> {
        if let Some(label) = &edit.label {
            self.store_label(key, Some(label.clone())).await?;
            *relabelled = true;
        }
        if let Some(limits) = &edit.limits {
            let current = self
                .inner
                .torrents
                .read()
                .expect("torrents lock poisoned")
                .get(key)
                .map(|e| e.limits)
                .ok_or(CoreError::NotFound)?;
            self.set_torrent_limits(key, limits.apply(current)).await?;
        }
        if let Some(seed) = &edit.seed_limits {
            self.set_seed_limits(key, seed.limits).await?;
        }
        if let Some(trackers) = edit.trackers.as_ref().filter(|t| !t.is_empty()) {
            let current = self.torrent_settings_of(key)?.trackers;
            let next = trackers.apply(&current);
            if next != current {
                self.set_trackers(key, next).await?;
            }
        }
        if let Some(policy) = edit.unwanted_policy {
            self.set_unwanted_policy(key, policy).await?;
        }
        Ok(())
    }

    /// The settings a [`BulkEdit`] changes, for each of `ids` in the space. Torrents not
    /// in the space are left out.
    #[must_use]
    pub fn torrent_settings(&self, space: SpaceId, ids: &[String]) -> Vec<TorrentSettings> {
        ids.iter()
            .filter_map(|id| self.torrent_settings_of(&Key::new(space, id.as_str())).ok())
            .collect()
    }

    fn torrent_settings_of(&self, key: &Key) -> Result<TorrentSettings, CoreError> {
        let summary = self.inner.store.get(key).ok_or(CoreError::NotFound)?;
        if let Some(p) = self
            .inner
            .pending
            .read()
            .expect("pending lock poisoned")
            .get(key)
        {
            return Ok(TorrentSettings {
                id: key.id.clone(),
                label: summary.label,
                limits: TorrentLimits::default(),
                seed_limits: None,
                trackers: p.magnet.trackers.iter().map(|t| vec![t.clone()]).collect(),
                unwanted_policy: p.unwanted,
            });
        }
        let (limits, trackers, unwanted_policy) = {
            let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
            let e = torrents.get(key).ok_or(CoreError::NotFound)?;
            (e.limits, e.trackers.clone(), e.unwanted)
        };
        Ok(TorrentSettings {
            id: key.id.clone(),
            label: summary.label,
            limits,
            seed_limits: self.seed_state(key).limits,
            trackers,
            unwanted_policy,
        })
    }

    /// Move a torrent in its space's queue. Positions are renumbered from 0.
    pub async fn queue_move(&self, key: &Key, to: QueueMove) -> Result<(), CoreError> {
        let positions = {
            let mut torrents = self.inner.torrents.write().expect("torrents lock poisoned");
            let mut pending = self.inner.pending.write().expect("pending lock poisoned");
            if !torrents.contains_key(key) && !pending.contains_key(key) {
                return Err(CoreError::NotFound);
            }
            let mut order: Vec<(u32, Key)> = torrents
                .iter()
                .filter(|(k, _)| k.space == key.space)
                .map(|(k, e)| (e.queue_position, k.clone()))
                .chain(
                    pending
                        .iter()
                        .filter(|(k, _)| k.space == key.space)
                        .map(|(k, p)| (p.queue_position, k.clone())),
                )
                .collect();
            order.sort();
            let mut ids: Vec<Key> = order.into_iter().map(|(_, k)| k).collect();
            let from = ids.iter().position(|k| k == key).expect("present");
            let item = ids.remove(from);
            let dest = match to {
                QueueMove::Top => 0,
                QueueMove::Up => from.saturating_sub(1),
                QueueMove::Down => (from + 1).min(ids.len()),
                QueueMove::Bottom => ids.len(),
            };
            ids.insert(dest, item);
            let positions: Vec<(Key, u32)> = ids
                .into_iter()
                .enumerate()
                .map(|(i, k)| (k, u32::try_from(i).unwrap_or(u32::MAX)))
                .collect();
            for (k, pos) in &positions {
                if let Some(e) = torrents.get_mut(k) {
                    e.queue_position = *pos;
                }
                if let Some(p) = pending.get_mut(k) {
                    p.queue_position = *pos;
                }
            }
            positions
        };
        self.db(move |db| db.set_queue_positions(&positions))
            .await?;
        self.apply_queue().await;
        self.refresh();
        Ok(())
    }

    /// Where each file of `torrent` lives on disk under `save_path` with `naming`.
    fn disk_paths(torrent: &Torrent, save_path: &std::path::Path, naming: u8) -> Vec<PathBuf> {
        let rel: Vec<Vec<String>> = match naming_from_byte(naming) {
            Naming::Portable => None,
            other => torrent.info.paths(other).ok(),
        }
        .unwrap_or_else(|| torrent.info.files.iter().map(|f| f.path.clone()).collect());
        rel.into_iter()
            .map(|c| c.iter().fold(save_path.to_path_buf(), |p, x| p.join(x)))
            .collect()
    }

    /// Link files the library already has into a new torrent's save path. Returns the
    /// indices of files shared with another torrent (hard links, which the engine treats
    /// as copy-on-write). Torrents the new one may take over wait for its check (see
    /// `settle_supersedes`).
    async fn link_from_library(
        &self,
        key: &Key,
        torrent: &Arc<Torrent>,
        save_path: &std::path::Path,
        naming: u8,
        priorities: &[u8],
    ) -> Vec<usize> {
        let lib = self.settings().library;
        if !lib.enabled {
            return Vec::new();
        }
        // Matching only ever pairs files of the same length, so only torrents with a file of
        // a wanted length can be sources. Asking all of them made adding the n-th torrent
        // cost n actor round trips.
        let lengths: std::collections::HashSet<u64> = torrent
            .info
            .files
            .iter()
            .enumerate()
            .filter(|(i, f)| !f.is_pad() && f.length > 0 && priorities.get(*i) != Some(&0))
            .map(|(_, f)| f.length)
            .collect();
        if lengths.is_empty() {
            return Vec::new();
        }
        let sources = self
            .library_sources(Some(&key.id), |t| {
                t.info.files.iter().any(|f| lengths.contains(&f.length))
            })
            .await;
        // Files in library folders are candidates too (see `folders`).
        let folder_files = self
            .folder_candidates(lengths.iter().copied().collect())
            .await;
        if sources.is_empty() && folder_files.is_empty() {
            return Vec::new();
        }
        let targets = Self::disk_paths(torrent, save_path, naming);
        let (new, prios) = (torrent.clone(), priorities.to_vec());
        let linked = tokio::task::spawn_blocking(move || {
            let mut matches =
                library::find_matches(&new, &targets, &prios, &sources, lib.partial_reuse);
            let taken: std::collections::HashSet<usize> = matches.iter().map(|m| m.file).collect();
            matches.extend(folders::find_matches(
                &new,
                &targets,
                &prios,
                &folder_files,
                lib.partial_reuse,
                &taken,
            ));
            let mut done = Vec::new();
            let mut shared = Vec::new();
            let mut reused = Vec::new();
            for m in &matches {
                match library::materialize(&m.from, &targets[m.file], m.full, lib.allow_copy) {
                    Some(how) => {
                        tracing::info!(
                            file = %targets[m.file].display(),
                            from = %m.source_id,
                            ?how,
                            full = m.full,
                            by = ?m.by,
                            "reused a file from the library"
                        );
                        if how.shared() {
                            shared.push(m.file);
                        }
                        reused.push(share::PendingReuse {
                            file: m.file,
                            // A library folder's file has no torrent name: show its file
                            // name (not its path, which is the server's business).
                            source_name: sources.iter().find(|s| s.id == m.source_id).map_or_else(
                                || {
                                    m.from.file_name().map_or_else(String::new, |n| {
                                        format!("library folder file {}", n.to_string_lossy())
                                    })
                                },
                                |s| s.torrent.info.name.clone(),
                            ),
                            source_id: m.source_id.clone(),
                            how,
                        });
                        done.push(m.clone());
                    }
                    None => {
                        tracing::debug!(file = %targets[m.file].display(), "could not link or copy");
                    }
                }
            }
            (shared, library::supersede_candidates(&done, &sources), reused)
        })
        .await
        .unwrap_or_default();
        let (linked, superseded, reused) = linked;
        // Recorded once the check says how much of each file held (see `settle_reuse`).
        self.inner.reuse.set_pending(&key.id, reused);
        if !superseded.is_empty() {
            self.inner
                .supersede_pending
                .lock()
                .expect("supersede lock poisoned")
                .insert(key.clone(), superseded);
        }
        linked
    }

    /// A snapshot of the files of every torrent `keep` accepts, except `except`'s: where
    /// they are, what's complete, and their fingerprints. Each one costs a round trip to the
    /// torrent's actor, so callers narrow it down first. A torrent in several spaces is
    /// listed once, at its primary's files (the engine's own); a source's `id` is the
    /// torrent id, whichever spaces hold it.
    async fn library_sources(
        &self,
        except: Option<&str>,
        keep: impl Fn(&Torrent) -> bool,
    ) -> Vec<library::Source> {
        let primaries: std::collections::HashSet<Key> = self
            .inner
            .swarms
            .lock()
            .expect("swarms lock poisoned")
            .iter()
            .map(|(id, s)| Key::new(s.primary, id.clone()))
            .collect();
        let entries: Vec<LibrarySnapshot> = self
            .inner
            .torrents
            .read()
            .expect("torrents lock poisoned")
            .iter()
            .filter(|(k, e)| {
                Some(k.id.as_str()) != except && primaries.contains(k) && keep(&e.torrent)
            })
            .map(|(k, e)| {
                (
                    k.id.clone(),
                    e.torrent.clone(),
                    e.save_path.clone(),
                    e.naming,
                    e.file_paths.clone(),
                    e.handle.clone(),
                )
            })
            .collect();
        let mut sources = Vec::with_capacity(entries.len());
        for (sid, t, save, naming, custom, handle) in entries {
            let Some(have) = handle.have().await else {
                continue;
            };
            let placements = handle.placements();
            let paths: Vec<Option<PathBuf>> = custom
                .map_or_else(
                    || Self::disk_paths(&t, &save, naming),
                    |c| {
                        c.into_iter()
                            .map(|p| p.iter().fold(save.clone(), |a, x| a.join(x)))
                            .collect::<Vec<_>>()
                    },
                )
                .into_iter()
                .zip(placements)
                .map(|(p, pl)| (pl == Placement::Save).then_some(p))
                .collect();
            // Whether each one is still current is checked only if a match would use it.
            let fingerprints = {
                let fps = self
                    .inner
                    .fingerprints
                    .read()
                    .expect("fingerprints lock poisoned");
                if fps.is_empty() {
                    Vec::new()
                } else {
                    paths
                        .iter()
                        .map(|p| p.as_ref().and_then(|p| fps.get(p).copied()))
                        .collect()
                }
            };
            sources.push(library::Source {
                id: sid,
                torrent: t,
                paths,
                have,
                fingerprints,
            });
        }
        sources
    }

    async fn load_fingerprints(&self) {
        match self.db(|db| db.fingerprints()).await {
            Ok(rows) => {
                let mut map = self
                    .inner
                    .fingerprints
                    .write()
                    .expect("fingerprints lock poisoned");
                for (path, fp) in rows {
                    map.insert(PathBuf::from(path), fp);
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not load file fingerprints"),
        }
    }

    /// The files worth fingerprinting: complete, in a save path, with no `pieces root` from
    /// their torrent, and no current fingerprint. Also drops fingerprints of files no
    /// torrent has any more.
    async fn fingerprint_work(&self) -> Vec<PathBuf> {
        let sources = self.library_sources(None, |_| true).await;
        let mut wanted = Vec::new();
        let mut known: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for s in &sources {
            let layout = apitorrent_engine::layout::Layout::from_info(&s.torrent.info);
            for (i, (f, p)) in s.torrent.info.files.iter().zip(&s.paths).enumerate() {
                let Some(p) = p else { continue };
                known.insert(p.clone());
                if f.is_pad() || f.length == 0 || f.pieces_root.is_some() {
                    continue;
                }
                if !layout.file_pieces(i).all(|piece| s.have.get(piece)) {
                    continue;
                }
                let current = self
                    .inner
                    .fingerprints
                    .read()
                    .expect("fingerprints lock poisoned")
                    .get(p)
                    .is_some_and(|fp| fp.current(p));
                if !current {
                    wanted.push(p.clone());
                }
            }
        }
        let gone: Vec<String> = {
            let mut map = self
                .inner
                .fingerprints
                .write()
                .expect("fingerprints lock poisoned");
            let gone: Vec<PathBuf> = map
                .keys()
                .filter(|p| !known.contains(*p))
                .cloned()
                .collect();
            for p in &gone {
                map.remove(p);
            }
            gone.iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect()
        };
        if !gone.is_empty() {
            let _ = self.db(move |db| db.delete_fingerprints(&gone)).await;
        }
        wanted.sort();
        wanted.dedup();
        wanted
    }

    /// Fingerprint every file that needs it now, at the configured rate (or flat out with
    /// `rate` 0 here meaning the setting). Returns how many were done. The background task
    /// calls this; tests call it directly.
    pub async fn fingerprint_now(&self) -> usize {
        let rate = self.settings().library.fingerprint_rate;
        let mut done = 0;
        for path in self.fingerprint_work().await {
            if self
                .inner
                .stopping
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                break;
            }
            let stop = self.inner.stopping.clone();
            let p = path.clone();
            let result =
                tokio::task::spawn_blocking(move || fingerprint::compute(&p, rate, &stop)).await;
            match result {
                Ok(Ok(fp)) => {
                    let key = path.to_string_lossy().into_owned();
                    if self
                        .db(move |db| db.set_fingerprint(&key, &fp))
                        .await
                        .is_ok()
                    {
                        self.inner
                            .fingerprints
                            .write()
                            .expect("fingerprints lock poisoned")
                            .insert(path, fp);
                        done += 1;
                    }
                }
                Ok(Err(fingerprint::Error::Stopped)) => break,
                Ok(Err(e)) => {
                    tracing::debug!(path = %path.display(), error = %e, "fingerprint skipped");
                }
                Err(e) => tracing::warn!(error = %e, "fingerprint task failed"),
            }
        }
        if done > 0 {
            tracing::info!(files = done, "fingerprinted files for future v2 matches");
        }
        done
    }

    /// Fingerprints files in idle time: a pass every few minutes, skipped while the library
    /// or fingerprinting is off, or while a torrent is being checked (both read the disk).
    async fn fingerprinter(self) {
        let mut tick = tokio::time::interval(FINGERPRINT_EVERY);
        tick.tick().await;
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                () = self.inner.stop.notified() => return,
            }
            // A shutdown while a pass was running missed the notification above.
            if self
                .inner
                .stopping
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                return;
            }
            let lib = self.settings().library;
            if !lib.enabled || lib.fingerprint_rate == 0 {
                continue;
            }
            let checking = self
                .inner
                .torrents
                .read()
                .expect("torrents lock poisoned")
                .values()
                .any(|e| e.handle.stats().state == EngineState::Checking);
            if checking {
                continue;
            }
            self.fingerprint_now().await;
        }
    }

    /// Record the supersessions that the new torrents' checks proved: a candidate counts
    /// only if every file that took it over verified. Decided once, when the check ends;
    /// pieces downloaded later don't count, since they would be a different version.
    async fn settle_supersedes(&self) {
        let ready: Vec<(Key, TorrentHandle, Arc<Torrent>)> = {
            let pending = self
                .inner
                .supersede_pending
                .lock()
                .expect("supersede lock poisoned");
            if pending.is_empty() {
                return;
            }
            let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
            pending
                .keys()
                .filter_map(|id| {
                    let e = torrents.get(id)?;
                    (e.handle.stats().state != EngineState::Checking)
                        .then(|| (id.clone(), e.handle.clone(), e.torrent.clone()))
                })
                .collect()
        };
        for (id, handle, torrent) in ready {
            // What the check found, not what is there now: pieces downloaded since would be
            // a different version's, and must not count as matching.
            let Some(have) = handle.checked() else {
                continue;
            };
            let Some(cands) = self
                .inner
                .supersede_pending
                .lock()
                .expect("supersede lock poisoned")
                .remove(&id)
            else {
                continue;
            };
            let layout = apitorrent_engine::layout::Layout::from_info(&torrent.info);
            for c in cands {
                if !library::files_verified(&layout, &have, &c.files) {
                    tracing::info!(torrent = %c.source_id, by = %id, "not superseded: the matched files differ");
                    continue;
                }
                // A pack supersedes only its own space's copy of a torrent.
                let source = Key::new(id.space, c.source_id.clone());
                if !self
                    .inner
                    .torrents
                    .read()
                    .expect("torrents lock poisoned")
                    .contains_key(&source)
                {
                    continue;
                }
                let (k, by) = (source.clone(), id.id.clone());
                if self.db(move |db| db.set_superseded(&k, &by)).await.is_ok()
                    && let Some(e) = self
                        .inner
                        .torrents
                        .write()
                        .expect("torrents lock poisoned")
                        .get_mut(&source)
                {
                    tracing::info!(torrent = %source, by = %id, "torrent superseded by a pack");
                    e.superseded_by = Some(id.id.clone());
                }
            }
        }
        // Torrents removed before their check ended. (Locks in the same order as above:
        // the pending map first.)
        let mut pending = self
            .inner
            .supersede_pending
            .lock()
            .expect("supersede lock poisoned");
        let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
        pending.retain(|id, _| torrents.contains_key(id));
    }

    /// Remember that a torrent came from another client (whose config directory is
    /// `source`), so dedupe leaves its files to an admin. Logged, not fatal, on failure.
    async fn record_imported(&self, row: db::ImportedRow) {
        let (key, waiting) = (
            row.key.clone(),
            row.taken_over_at.is_none().then(|| row.source.clone()),
        );
        if let Err(e) = self.db(move |db| db.set_imported(&row)).await {
            tracing::warn!(error = %e, "could not record an imported torrent");
            return;
        }
        self.set_in_deluge(&key, waiting);
    }

    /// Mark an imported torrent as taken over (Deluge has stopped it) at `at`.
    async fn mark_taken_over(&self, key: &Key, at: i64) -> Result<(), CoreError> {
        let k = key.clone();
        self.db(move |db| db.set_taken_over(&k, at)).await?;
        self.set_in_deluge(key, None);
        Ok(())
    }

    /// Show, or stop showing, a torrent as still Deluge's (`TorrentSummary::in_deluge`).
    fn set_in_deluge(&self, key: &Key, config_dir: Option<String>) {
        if let Some(e) = self
            .inner
            .torrents
            .write()
            .expect("torrents lock poisoned")
            .get_mut(key)
        {
            e.in_deluge = config_dir;
        }
        self.refresh_one(key);
    }

    /// Show a torrent as imported from a `.torrent` file Deluge left but didn't list
    /// (`TorrentSummary::leftover_from`).
    fn set_leftover_from(&self, key: &Key, config_dir: String) {
        if let Some(e) = self
            .inner
            .torrents
            .write()
            .expect("torrents lock poisoned")
            .get_mut(key)
        {
            e.leftover_from = Some(config_dir);
        }
        self.refresh_one(key);
    }

    /// What a Deluge installation holds, for choosing what to import into `space`: each
    /// torrent with its label, trackers, path and progress, whether the space has it
    /// already, and why it couldn't be imported. Reads only.
    pub async fn preview_deluge(
        &self,
        space: SpaceId,
        req: apitorrent_types::DelugePreviewRequest,
    ) -> Result<apitorrent_types::DelugePreview, CoreError> {
        use apitorrent_types::{DelugePreview, DelugePreviewTorrent};
        let target_space = self.space(space)?;
        let dir = PathBuf::from(&req.config_dir);
        let state = tokio::task::spawn_blocking(move || apitorrent_deluge::load(&dir))
            .await
            .expect("Deluge reader panicked")
            .map_err(|e| CoreError::InvalidRequest(e.to_string()))?;
        let map: Vec<(String, String)> = req
            .path_map
            .iter()
            .map(|m| (m.from.clone(), m.to.clone()))
            .collect();
        let download_dir = self.download_dir(space);
        let present: HashSet<String> = if req.new_space {
            HashSet::new()
        } else {
            let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
            let pending = self.inner.pending.read().expect("pending lock poisoned");
            torrents
                .keys()
                .chain(pending.keys())
                .filter(|k| k.space == space)
                .map(|k| k.id.clone())
                .collect()
        };
        let roots: &[String] = if req.new_space {
            &[]
        } else {
            &target_space.settings.storage_roots
        };
        let summaries = deluge_import::summarize(&state, &map, &download_dir);
        // Hosts of private torrents an identity rule already covers: the import adds none.
        let private: Vec<String> = {
            let mut h: Vec<String> = summaries
                .iter()
                .filter(|s| s.details.private)
                .flat_map(|s| s.hosts.clone())
                .collect();
            h.sort();
            h.dedup();
            h
        };
        let covered_hosts = {
            let mut probe = self.settings().identity;
            let added = settings::ensure_tracker_identities(
                &mut probe,
                &private,
                &apitorrent_types::ClientIdentity::Apitorrent,
            );
            private.into_iter().filter(|h| !added.contains(h)).collect()
        };
        // Only for showing the choice: a slow daemon mustn't hold up the preview (the
        // import asks again).
        let deluge_identity = tokio::time::timeout(
            Duration::from_secs(3),
            takeover::deluge_identity(Path::new(&req.config_dir)),
        )
        .await
        .unwrap_or_else(|_| apitorrent_types::ClientIdentity::deluge_default());
        let mut labels: Vec<String> = summaries.iter().filter_map(|s| s.label.clone()).collect();
        labels.sort();
        labels.dedup();
        let mut trackers: Vec<String> = summaries.iter().flat_map(|s| s.hosts.clone()).collect();
        trackers.sort();
        trackers.dedup();
        let torrents = summaries
            .into_iter()
            .map(|s| {
                let outside =
                    !roots.is_empty() && !roots.iter().any(|r| s.save_path.starts_with(r.as_str()));
                let problem = s.error.clone().or_else(|| {
                    outside.then(|| {
                        format!(
                            "{} is outside space {}'s storage roots",
                            s.save_path.display(),
                            target_space.slug
                        )
                    })
                });
                let d = s.details;
                DelugePreviewTorrent {
                    present: present.contains(&s.id),
                    id: s.id,
                    name: s.name,
                    label: s.label,
                    trackers: s.hosts,
                    save_path: s.save_path.to_string_lossy().into_owned(),
                    size: s.size,
                    progress: s.progress,
                    paused: s.paused,
                    leftover: s.leftover,
                    problem,
                    private: d.private,
                    limits: d.limits,
                    stop_at_ratio: d
                        .stop_at_ratio
                        .map(|(ratio, remove)| apitorrent_types::StopAtRatio { ratio, remove }),
                    skipped_files: d.skipped_files,
                    renamed_files: d.renamed_files,
                    downloaded: d.downloaded,
                    uploaded: d.uploaded,
                    seeding_secs: d.seeding_secs,
                    queue: d.queue,
                    added_at: d.added_at,
                    edited_trackers: d.edited_trackers,
                    not_carried: d.not_carried,
                }
            })
            .collect();
        Ok(DelugePreview {
            torrents,
            labels,
            trackers,
            download_location: state
                .settings
                .download_location
                .as_deref()
                .map(|p| import::remap(p, &map)),
            suggested_slug: deluge_import::suggested_slug(Path::new(&req.config_dir)),
            settings: deluge_import::settings_view(&state.settings),
            label_options: state.label_options,
            warnings: state.warnings,
            deluge_identity,
            covered_hosts,
        })
    }

    /// Bring in a Deluge installation's torrents into a space: files where Deluge left them,
    /// progress from its resume data (validated against the files on disk, so nothing is
    /// rehashed), labels, queue order, per-torrent limits and all-time totals. Torrents the
    /// space already has are left alone, so importing twice is safe. See `deluge_import`.
    /// Waits until it's done; [`Core::start_import_deluge`] runs it in the background.
    pub async fn import_deluge(
        &self,
        space: SpaceId,
        req: apitorrent_types::ImportDelugeRequest,
    ) -> Result<apitorrent_types::ImportReport, CoreError> {
        self.import_deluge_with(space, req, &Progress::none()).await
    }

    /// Start [`Core::import_deluge`] as a background job, and return it at once. Refused
    /// while another import into the space runs.
    pub fn start_import_deluge(
        &self,
        space: SpaceId,
        req: apitorrent_types::ImportDelugeRequest,
    ) -> Result<apitorrent_types::Job, CoreError> {
        self.space(space)?;
        let progress = self
            .inner
            .jobs
            .start(apitorrent_types::JobKind::DelugeImport, space)
            .ok_or_else(|| {
                CoreError::Busy("an import into this space is already running".into())
            })?;
        let job = self.inner.jobs.get(progress.id()).expect("just started");
        let core = self.clone();
        tokio::spawn(async move {
            let outcome = match core.import_deluge_with(space, req, &progress).await {
                Ok(report) => apitorrent_types::JobOutcome::DelugeImport { report },
                Err(e) => apitorrent_types::JobOutcome::Failed {
                    message: e.to_string(),
                },
            };
            progress.finish(outcome);
        });
        Ok(job)
    }

    /// Start [`Core::take_over`] as a background job, and return it at once. Refused while
    /// another take-over in the space runs.
    pub fn start_take_over(
        &self,
        space: SpaceId,
        ids: Vec<String>,
    ) -> Result<apitorrent_types::Job, CoreError> {
        self.space(space)?;
        let progress = self
            .inner
            .jobs
            .start(apitorrent_types::JobKind::TakeOver, space)
            .ok_or_else(|| {
                CoreError::Busy("a take-over in this space is already running".into())
            })?;
        let job = self.inner.jobs.get(progress.id()).expect("just started");
        let core = self.clone();
        tokio::spawn(async move {
            let outcome = match core.take_over_with(space, &ids, &progress).await {
                Ok(report) => apitorrent_types::JobOutcome::TakeOver { report },
                Err(e) => apitorrent_types::JobOutcome::Failed {
                    message: e.to_string(),
                },
            };
            progress.finish(outcome);
        });
        Ok(job)
    }

    /// One of `space`'s background jobs.
    pub fn job(&self, space: SpaceId, id: u64) -> Result<apitorrent_types::Job, CoreError> {
        self.inner
            .jobs
            .get(id)
            .filter(|j| j.space == space)
            .ok_or(CoreError::NotFound)
    }

    /// `space`'s background jobs, running and recently finished, oldest first.
    pub fn jobs(&self, space: SpaceId) -> Vec<apitorrent_types::Job> {
        self.inner.jobs.in_space(space)
    }

    async fn import_deluge_with(
        &self,
        space: SpaceId,
        req: apitorrent_types::ImportDelugeRequest,
        progress: &Progress,
    ) -> Result<apitorrent_types::ImportReport, CoreError> {
        use apitorrent_types::{ImportReport, ImportSkip};
        /// Torrents written to the database per transaction: few commits, and the database
        /// isn't held long at a time.
        const BATCH: usize = 64;
        let target_space = self.space(space)?;
        deluge_import::check_options(&req.options).map_err(CoreError::InvalidRequest)?;
        progress.phase("Reading Deluge's state", 0);
        let dir = PathBuf::from(&req.config_dir);
        let mut state = tokio::task::spawn_blocking(move || apitorrent_deluge::load(&dir))
            .await
            .expect("Deluge reader panicked")
            .map_err(|e| CoreError::InvalidRequest(e.to_string()))?;
        let pick = req.options.settings;
        if pick.any() {
            if space == DEFAULT_SPACE {
                let mut s = self.settings();
                deluge_import::apply_instance_settings(&mut s, &state.settings, pick);
                self.set_settings(s).await?;
            } else {
                let mut s = target_space.settings.clone();
                deluge_import::apply_space_settings(&mut s, &state.settings, pick);
                self.update_space(
                    space,
                    apitorrent_types::spaces::UpdateSpaceRequest {
                        name: target_space.name.clone(),
                        description: target_space.description.clone(),
                        settings: s,
                    },
                )
                .await?;
            }
        }
        let map: Vec<(String, String)> = req
            .path_map
            .iter()
            .map(|m| (m.from.clone(), m.to.clone()))
            .collect();
        let present: HashSet<String> = {
            let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
            let pending = self.inner.pending.read().expect("pending lock poisoned");
            torrents
                .keys()
                .chain(pending.keys())
                .filter(|k| k.space == space)
                .map(|k| k.id.clone())
                .collect()
        };
        let download_dir = self.download_dir(space);
        let first_position = self.next_queue_position(space);
        progress.phase("Reading the torrents", 0);
        // Parsing every .torrent is CPU work: off the async runtime.
        let (state, not_selected, mut plan) = {
            let slug = target_space.slug.clone();
            let roots = target_space.settings.storage_roots.clone();
            let (filter, paused, options) = (req.filter.clone(), req.paused, req.options.clone());
            let ratio_per_torrent = space != DEFAULT_SPACE && pick.stop_at_ratio;
            tokio::task::spawn_blocking(move || {
                let not_selected = deluge_import::select(&mut state, &filter, &map, &download_dir);
                let plan = deluge_import::plan(
                    &state,
                    &deluge_import::Target {
                        space,
                        slug: &slug,
                        roots: &roots,
                        download_dir: &download_dir,
                        present: &present,
                        map: &map,
                        paused,
                        ratio_per_torrent,
                        options: &options,
                        first_position,
                    },
                );
                (state, not_selected, plan)
            })
            .await
            .expect("import planner panicked")
        };
        let mut warnings = state.warnings.clone();
        // Nothing starts here while Deluge still runs it.
        if !deluge_import::running_ids(&plan).is_empty() {
            progress.phase("Pausing them in Deluge", 0);
        }
        let taken_over_at =
            deluge_import::take_over_running(&mut plan, Path::new(&req.config_dir), &mut warnings)
                .await;
        // Private trackers knew these torrents as Deluge's: keep presenting as Deluge there.
        let private_hosts = deluge_import::identity_hosts(&plan, &req.options);
        let identity_hosts = if private_hosts.is_empty() {
            Vec::new()
        } else {
            let identity =
                deluge_import::import_identity(&req.options, Path::new(&req.config_dir)).await;
            match self
                .ensure_tracker_identities(&private_hosts, identity)
                .await
            {
                Ok(added) => added,
                Err(e) => {
                    warnings.push(format!("couldn't add client identity rules: {e}"));
                    Vec::new()
                }
            }
        };
        let mut report = ImportReport {
            identity_hosts,
            warnings,
            skipped: plan.skipped.clone(),
            not_selected,
            ..ImportReport::default()
        };
        report
            .skipped
            .extend(plan.already.iter().map(|name| ImportSkip {
                name: name.clone(),
                reason: "already in this space".into(),
            }));
        let total = u32::try_from(plan.torrents.len() + plan.magnets.len()).unwrap_or(u32::MAX);
        progress.phase("Adding torrents", total);
        for m in plan.magnets {
            progress.working_on(&m.name);
            let added = self
                .add(
                    space,
                    AddTorrentRequest {
                        source: AddSource::Magnet { uri: m.uri.clone() },
                        save_path: Some(m.row.save_path.clone()),
                        paused: m.row.paused,
                        label: m.row.label.clone(),
                        file_priorities: None,
                        unwanted_policy: UnwantedPolicy::Hold,
                    },
                )
                .await;
            match added {
                Ok(id) => {
                    let mut row = m.row.clone();
                    row.id = id;
                    self.record_imported(deluge_import::imported_row(
                        &row,
                        &m.deluge_id,
                        &req.config_dir,
                        taken_over_at,
                        false,
                    ))
                    .await;
                    report.imported.push(m.name);
                }
                Err(e) => report.skipped.push(ImportSkip {
                    name: m.name,
                    reason: e.to_string(),
                }),
            }
            progress.step(None);
        }
        // Where a torrent keeps held files depends only on its save path's volume: probe
        // each save path once, not once per torrent.
        let held_dirs: HashMap<PathBuf, PathBuf> = {
            let data = self.inner.config.data_dir.clone();
            let saves: HashSet<PathBuf> = plan
                .torrents
                .iter()
                .map(|t| PathBuf::from(&t.row.save_path))
                .collect();
            tokio::task::spawn_blocking(move || {
                saves
                    .into_iter()
                    .map(|save| {
                        let dir = held_dir_for(&data, &save);
                        (save, dir)
                    })
                    .collect()
            })
            .await
            .expect("held path probe panicked")
        };
        let mut torrents = plan.torrents.into_iter().peekable();
        while torrents.peek().is_some() {
            let mut batch: Vec<deluge_import::PlannedTorrent> =
                torrents.by_ref().take(BATCH).collect();
            for t in &mut batch {
                let held = held_dirs[Path::new(&t.row.save_path)].join(&t.row.id);
                t.row.held_path = Some(held.to_string_lossy().into_owned());
            }
            progress.working_on(&batch[0].name);
            // One transaction for the batch's rows, seeding times and import records.
            let rows: Vec<_> = batch
                .iter()
                .map(|t| {
                    (
                        t.row.clone(),
                        t.seed,
                        deluge_import::imported_row(
                            &t.row,
                            &t.deluge_id,
                            &req.config_dir,
                            taken_over_at,
                            t.leftover,
                        ),
                    )
                })
                .collect();
            let written = self.db(move |db| db.insert_imported(&rows)).await?;
            for (t, stored) in batch.into_iter().zip(written) {
                if let Err(e) = stored {
                    report.skipped.push(ImportSkip {
                        name: t.name,
                        reason: e.to_string(),
                    });
                    progress.step(None);
                    continue;
                }
                let key = Key::new(space, t.row.id.clone());
                let held_path = PathBuf::from(t.row.held_path.as_deref().expect("set above"));
                let (added_at, queue_position, paused) =
                    (t.row.added_at, t.row.queue_position, t.row.paused);
                let spawned = self
                    .spawn_entry(Spawn {
                        key: key.clone(),
                        peers: Vec::new(),
                        trackers: t.torrent.trackers.clone(),
                        torrent: t.torrent.clone(),
                        save_path: PathBuf::from(&t.row.save_path),
                        held_path,
                        have: t.have.clone(),
                        placements: None,
                        paused,
                        file_priorities: t.row.file_priorities.clone().unwrap_or_default(),
                        unwanted: UnwantedPolicy::Hold,
                        label: t.label.clone(),
                        added_at,
                        totals: (t.row.downloaded_total, t.row.uploaded_total),
                        queue_position,
                        limits: t.limits,
                        queued: !paused && self.queue_limited(),
                        naming: 0,
                        linked: Vec::new(),
                        file_paths: t.file_paths.clone(),
                    })
                    .await;
                match spawned {
                    Ok(_) => {
                        self.inner
                            .seeding
                            .lock()
                            .expect("seeding lock poisoned")
                            .insert(key.clone(), t.seed);
                        // Still Deluge's until taken over (as `record_imported` shows it);
                        // a leftover never was.
                        let waiting = (!t.leftover && (paused || taken_over_at.is_none()))
                            .then(|| req.config_dir.clone());
                        self.set_in_deluge(&key, waiting);
                        if t.leftover {
                            self.set_leftover_from(&key, req.config_dir.clone());
                        }
                        report.imported.push(t.name);
                    }
                    Err(e) => {
                        let _ = self.db(move |db| db.remove_torrent(&key)).await;
                        report.skipped.push(ImportSkip {
                            name: t.name,
                            reason: e.to_string(),
                        });
                    }
                }
                progress.step(None);
            }
            // Show what has arrived so far, rather than everything at the end.
            self.refresh();
        }
        tracing::info!(
            space = %target_space.slug,
            imported = report.imported.len(),
            skipped = report.skipped.len(),
            "Deluge import finished"
        );
        progress.phase("Starting them", total);
        self.apply_queue().await;
        self.refresh();
        Ok(report)
    }

    /// Remove superseded torrents whose tracker now says they're unregistered (their files
    /// stay: the torrent that superseded them holds the same data), and ask tracker
    /// adapters for the packs of unregistered torrents. See `adapters`.
    pub async fn retire_superseded(&self) {
        adapters::retire_superseded(self).await;
    }

    /// The naming for a new torrent saved under `save_path`: the setting, unless the
    /// filesystem there can't take native names.
    async fn choose_naming(&self, save_path: &std::path::Path) -> u8 {
        if self.settings().naming == apitorrent_types::NamingPolicy::Portable {
            return 0;
        }
        let path = save_path.to_path_buf();
        tokio::task::spawn_blocking(move || match probe_filesystem(&path) {
            Ok(fs) if fs.native_names => {
                if fs.case_insensitive {
                    2
                } else {
                    1
                }
            }
            Ok(_) => {
                tracing::info!(path = %path.display(), "filesystem needs portable names");
                0
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "could not probe the filesystem; using portable names");
                0
            }
        })
        .await
        .unwrap_or(0)
    }

    /// The end of a space's queue.
    fn next_queue_position(&self, space: SpaceId) -> u32 {
        let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
        let pending = self.inner.pending.read().expect("pending lock poisoned");
        torrents
            .iter()
            .filter(|(k, _)| k.space == space)
            .map(|(_, e)| e.queue_position + 1)
            .chain(
                pending
                    .iter()
                    .filter(|(k, _)| k.space == space)
                    .map(|(_, p)| p.queue_position + 1),
            )
            .max()
            .unwrap_or(0)
    }

    /// Whether any queue limit is set, the instance's or a space's, so new and restored
    /// torrents should wait for the queue's decision rather than start at once.
    fn queue_limited(&self) -> bool {
        self.settings().queue.limited()
            || self
                .inner
                .spaces
                .read()
                .expect("spaces lock poisoned")
                .values()
                .any(|s| spaces::queue_limited(&s.settings))
    }

    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    /// The directories this instance downloads into, each with a `.apitorrent-root` marker.
    pub fn storage_roots(&self) -> Vec<apitorrent_types::StorageRoot> {
        self.inner.roots.read().expect("roots lock").clone()
    }

    /// Replace a storage root's share locators (where other machines reach it), after
    /// checking and normalising them. Returns the updated root.
    pub async fn set_root_locators(
        &self,
        id: &str,
        locators: &[String],
    ) -> Result<apitorrent_types::StorageRoot, CoreError> {
        if !self.storage_roots().iter().any(|r| r.id == id) {
            return Err(CoreError::NotFound);
        }
        let normal = apitorrent_types::share::normalise_locators(locators)
            .map_err(CoreError::InvalidRequest)?;
        let (key, value) = (id.to_owned(), normal.clone());
        self.db(move |db| roots::save_locators(db, &key, &value))
            .await?;
        let mut roots = self.inner.roots.write().expect("roots lock");
        let root = roots
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or(CoreError::NotFound)?;
        root.locators = normal;
        Ok(root.clone())
    }

    /// Fetch a web locator's copy of a storage root's marker file, to tell an admin whether
    /// the URL serves that root. The locator needn't be saved yet.
    pub async fn check_root_locator(
        &self,
        id: &str,
        locator: &str,
    ) -> Result<apitorrent_types::LocatorCheck, CoreError> {
        if !self.storage_roots().iter().any(|r| r.id == id) {
            return Err(CoreError::NotFound);
        }
        let parsed = apitorrent_types::share::ShareLocator::parse(locator)
            .map_err(CoreError::InvalidRequest)?;
        if !parsed.protocol.is_web() {
            return Err(CoreError::InvalidRequest(
                "only http:// and https:// locators can be checked from the server".into(),
            ));
        }
        Ok(roots::check_web_locator(id, &parsed).await)
    }

    pub fn peer_port(&self) -> u16 {
        self.inner.session.listen_addr().port()
    }

    /// Addresses banned right now.
    pub fn bans(&self) -> Vec<apitorrent_types::Ban> {
        use apitorrent_engine::ban::BanReason as R;
        self.inner
            .session
            .bans()
            .into_iter()
            .map(|b| apitorrent_types::Ban {
                ip: b.ip.to_string(),
                reason: match b.reason {
                    R::BadData => apitorrent_types::BanReason::BadData,
                    R::Untrusted => apitorrent_types::BanReason::Untrusted,
                    R::Manual => apitorrent_types::BanReason::Manual,
                },
                count: b.count,
                until: b
                    .until
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)),
            })
            .collect()
    }

    pub fn ban(&self, ip: &str) -> Result<(), CoreError> {
        let ip = ip
            .parse()
            .map_err(|_| CoreError::InvalidRequest(format!("not an IP address: {ip}")))?;
        self.inner.session.ban(ip);
        Ok(())
    }

    pub fn unban(&self, ip: &str) -> Result<(), CoreError> {
        let ip = ip
            .parse()
            .map_err(|_| CoreError::InvalidRequest(format!("not an IP address: {ip}")))?;
        if self.inner.session.unban(ip) {
            Ok(())
        } else {
            Err(CoreError::NotFound)
        }
    }

    /// Replace the IP blocklist.
    pub fn set_blocklist(&self, list: apitorrent_engine::ban::Blocklist) {
        tracing::info!(ranges = list.len(), "blocklist loaded");
        self.inner.session.set_blocklist(list);
    }

    /// A snapshot of one space's torrents (or every space's, for `None`) and the patches
    /// after it. Patches cover every space: see [`Patch::for_space`].
    pub fn subscribe(&self, space: Option<SpaceId>) -> (Snapshot, broadcast::Receiver<Arc<Patch>>) {
        self.inner.store.subscribe(space)
    }

    async fn db<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Database) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T, CoreError> {
        let db = self.inner.db.clone();
        tokio::task::spawn_blocking(move || f(&mut db.lock().expect("database lock poisoned")))
            .await
            .expect("database task panicked")
            .map_err(CoreError::from)
    }

    async fn restore(&self) -> Result<(), CoreError> {
        let mut rows = self.db(|db| db.torrents()).await?;
        let count = rows.len();
        // A torrent in several spaces has one swarm, whose files are its primary's: the
        // entry that saved resume data last time (only primaries do), else the oldest.
        rows.sort_by_key(|r| (r.have.is_none(), r.added_at, r.space));
        for row in rows {
            let key = Key::new(row.space, row.id.clone());
            if row.torrent.is_empty() {
                self.restore_pending(row);
                continue;
            }
            let torrent = match Torrent::from_bytes(&row.torrent) {
                Ok(t) => Arc::new(t),
                Err(e) => {
                    tracing::error!(torrent = %key, error = %e, "stored torrent no longer parses");
                    continue;
                }
            };
            let pieces = u32::try_from(torrent.info.piece_count()).expect("fits");
            let have = row
                .have
                .as_deref()
                .and_then(|b| Bitfield::from_bytes(b, pieces));
            let files = torrent.info.files.len();
            let prios = row
                .file_priorities
                .clone()
                .filter(|p| p.len() == files)
                .unwrap_or_else(|| vec![4; files]);
            let placements = row
                .placements
                .as_deref()
                .filter(|p| p.len() == files)
                .map(|p| p.iter().map(|&b| placement_from_byte(b)).collect());
            let save_path = PathBuf::from(&row.save_path);
            let held_path = row.held_path.as_ref().map_or_else(
                || held_path_for(&self.inner.config.data_dir, &save_path, &row.id),
                PathBuf::from,
            );
            let trackers = row
                .trackers
                .as_deref()
                .and_then(|t| serde_json::from_str::<Vec<Vec<String>>>(t).ok())
                .unwrap_or_else(|| torrent.trackers.clone());
            let spawned = self
                .spawn_entry(Spawn {
                    key: key.clone(),
                    peers: Vec::new(),
                    trackers,
                    torrent,
                    save_path,
                    held_path,
                    have,
                    placements,
                    paused: row.paused,
                    file_priorities: prios,
                    unwanted: policy_from_byte(row.unwanted_policy),
                    label: row.label,
                    added_at: row.added_at,
                    totals: (row.downloaded_total, row.uploaded_total),
                    queue_position: row.queue_position,
                    limits: TorrentLimits {
                        download_rate: row.download_rate,
                        upload_rate: row.upload_rate,
                        max_connections: row.max_connections,
                    },
                    queued: !row.paused && self.queue_limited(),
                    naming: row.naming,
                    linked: row
                        .linked
                        .as_deref()
                        .map(unpack_indices)
                        .unwrap_or_default(),
                    file_paths: row
                        .file_paths
                        .as_deref()
                        .and_then(|j| serde_json::from_str(j).ok()),
                })
                .await;
            if let Err(e) = spawned {
                tracing::error!(torrent = %key, error = %e, "could not restore the torrent");
            }
        }
        let superseded = self.db(|db| db.superseded()).await?;
        let imported = self.db(|db| db.imported()).await?;
        {
            let mut torrents = self.inner.torrents.write().expect("torrents lock poisoned");
            for (s, by) in superseded {
                if let Some(e) = torrents.get_mut(&s) {
                    e.superseded_by = Some(by);
                }
            }
            for r in imported {
                if let Some(e) = torrents.get_mut(&r.key) {
                    if r.leftover {
                        e.leftover_from = Some(r.source.clone());
                    }
                    if r.taken_over_at.is_none() {
                        e.in_deluge = Some(r.source);
                    }
                }
            }
        }
        tracing::info!(count, "torrents restored");
        self.refresh();
        Ok(())
    }

    /// Bring back a magnet link that was still waiting for metadata.
    fn restore_pending(&self, row: TorrentRow) {
        let key = Key::new(row.space, row.id.clone());
        let Some(uri) = row.magnet else {
            tracing::error!(torrent = %key, "stored torrent has neither metadata nor a magnet link");
            return;
        };
        let magnet = match Magnet::parse(&uri) {
            Ok(m) => m,
            Err(e) => {
                tracing::error!(torrent = %key, error = %e, "stored magnet link no longer parses");
                return;
            }
        };
        let save_path = PathBuf::from(&row.save_path);
        let held_path = row.held_path.as_ref().map_or_else(
            || held_path_for(&self.inner.config.data_dir, &save_path, &row.id),
            PathBuf::from,
        );
        self.inner
            .pending
            .write()
            .expect("pending lock poisoned")
            .insert(
                key.clone(),
                Pending {
                    magnet,
                    uri,
                    save_path,
                    held_path,
                    label: row.label,
                    added_at: row.added_at,
                    paused: row.paused,
                    unwanted: policy_from_byte(row.unwanted_policy),
                    queue_position: row.queue_position,
                    naming: row.naming,
                    fetch: None,
                },
            );
        if !row.paused {
            self.start_fetch(&key);
        }
    }

    /// Start (or restart) the metadata fetch for a pending magnet link. When it finishes,
    /// the torrent starts for real.
    fn start_fetch(&self, key: &Key) {
        let mut pending = self.inner.pending.write().expect("pending lock poisoned");
        let Some(p) = pending.get_mut(key) else {
            return;
        };
        let mut fetch = self.inner.session.fetch_metadata(&p.magnet);
        let (cancel_tx, cancel_rx) = oneshot::channel();
        p.fetch = Some((fetch.watch(), cancel_tx));
        let core = self.clone();
        let key = key.clone();
        tokio::spawn(async move {
            tokio::select! {
                fetched = fetch.wait() => {
                    if let Some(fetched) = fetched
                        && let Err(e) = core.metadata_arrived(&key, fetched).await
                    {
                        tracing::error!(torrent = %key, error = %e, "could not start the torrent from its metadata");
                    }
                }
                _ = cancel_rx => {}
            }
        });
    }

    /// A magnet link's metadata is here: store it as a `.torrent` and start the torrent.
    async fn metadata_arrived(
        &self,
        key: &Key,
        fetched: apitorrent_engine::metadata::Fetched,
    ) -> Result<(), CoreError> {
        // The pending entry stays visible (to the list, details, pause and remove) until
        // the torrent replaces it below; removing it first left a gap in which the torrent
        // was nowhere, and the API answered 404.
        let Some(magnet) = self
            .inner
            .pending
            .read()
            .expect("pending lock poisoned")
            .get(key)
            .map(|p| (p.magnet.clone(), p.save_path.clone(), p.naming))
        else {
            return Ok(()); // removed meanwhile
        };
        let (magnet, save_path, naming) = magnet;
        let bytes = apitorrent_engine::metadata::torrent_file(
            &fetched.info,
            &magnet.trackers,
            &magnet.web_seeds,
        );
        let torrent = Arc::new(
            Torrent::from_bytes(&bytes).map_err(|e| CoreError::InvalidTorrent(e.to_string()))?,
        );
        let prios = select_only_priorities(&torrent, magnet.select_only.as_deref());
        let hashes = torrent.info.info_hashes();
        let (k, name, stored) = (key.clone(), torrent.info.name.clone(), prios.clone());
        let (v1, v2) = (
            hashes.get_v1().map(|h| h.to_hex()),
            hashes.get_v2().map(|h| h.to_hex()),
        );
        self.db(move |db| {
            db.set_metadata(&k, &bytes, &name, v1.as_deref(), v2.as_deref(), &stored)
        })
        .await?;
        tracing::info!(torrent = %key, name = %torrent.info.name, "metadata fetched");
        let linked = if self.swarm_exists(&key.id) {
            Vec::new()
        } else {
            self.link_from_library(key, &torrent, &save_path, naming, &prios)
                .await
        };

        // The latest pending state (the user may have paused or relabelled it meanwhile),
        // then the torrent takes its place.
        let Some(snapshot) = self
            .inner
            .pending
            .read()
            .expect("pending lock poisoned")
            .get(key)
            .map(|p| PendingSnapshot {
                held_path: p.held_path.clone(),
                paused: p.paused,
                unwanted: p.unwanted,
                label: p.label.clone(),
                added_at: p.added_at,
                queue_position: p.queue_position,
            })
        else {
            return Ok(()); // removed meanwhile
        };
        self.spawn_entry(Spawn {
            key: key.clone(),
            peers: fetched.peers,
            trackers: torrent.trackers.clone(),
            torrent,
            save_path,
            held_path: snapshot.held_path,
            have: None,
            placements: None,
            paused: snapshot.paused,
            file_priorities: prios,
            unwanted: snapshot.unwanted,
            label: snapshot.label.clone(),
            added_at: snapshot.added_at,
            totals: (0, 0),
            queue_position: snapshot.queue_position,
            limits: TorrentLimits::default(),
            queued: !snapshot.paused && self.queue_limited(),
            naming,
            linked,
            file_paths: None,
        })
        .await?;
        let last = self
            .inner
            .pending
            .write()
            .expect("pending lock poisoned")
            .remove(key);
        match last {
            // Removed in the moment between the snapshot and now: undo the torrent too.
            None => {
                tracing::info!(torrent = %key, "removed while its metadata arrived");
                self.remove(key, false).await?;
                return Ok(());
            }
            // Changed in that moment: carry the change over.
            Some(p) => {
                if p.paused != snapshot.paused {
                    self.set_paused(key, p.paused).await?;
                }
                if p.label != snapshot.label {
                    self.set_label(key, p.label).await?;
                }
            }
        }
        self.apply_queue().await;
        self.refresh();
        Ok(())
    }

    /// Whether some space already has this torrent running in the engine.
    fn swarm_exists(&self, id: &str) -> bool {
        self.inner
            .swarms
            .lock()
            .expect("swarms lock poisoned")
            .contains_key(id)
    }

    /// Whether this space already has the torrent (running or waiting for metadata).
    fn taken(&self, key: &Key) -> bool {
        self.inner
            .torrents
            .read()
            .expect("torrents lock poisoned")
            .contains_key(key)
            || self
                .inner
                .pending
                .read()
                .expect("pending lock poisoned")
                .contains_key(key)
    }

    /// Add a magnet link: store it, and fetch its metadata in the background. When another
    /// space already has the torrent, its metadata is used at once instead.
    async fn add_magnet(
        &self,
        space: SpaceId,
        uri: &str,
        req: AddTorrentRequest,
    ) -> Result<TorrentId, CoreError> {
        let magnet = Magnet::parse(uri).map_err(|e| CoreError::InvalidRequest(e.to_string()))?;
        let id = magnet.info_hashes.id_string();
        let key = Key::new(space, id.clone());
        if self.taken(&key) {
            return Err(CoreError::Duplicate);
        }
        let known = {
            let id = id.clone();
            self.db(move |db| db.torrent_file(&id)).await?
        };
        if let Some(bytes) = known {
            // The swarm is already here: join it, announcing to the magnet's trackers.
            let bytes = if magnet.trackers.is_empty() {
                bytes
            } else {
                let tiers: Vec<Vec<String>> =
                    magnet.trackers.iter().map(|t| vec![t.clone()]).collect();
                import::with_trackers(&bytes, &tiers).unwrap_or(bytes)
            };
            let prios = Torrent::from_bytes(&bytes)
                .ok()
                .map(|t| select_only_priorities(&t, magnet.select_only.as_deref()));
            return self
                .add_bytes(
                    space,
                    bytes,
                    AddTorrentRequest {
                        file_priorities: req.file_priorities.clone().or(prios),
                        ..req
                    },
                )
                .await;
        }
        let save_path =
            self.resolve_save_path(space, req.save_path.as_deref(), req.label.as_deref())?;
        let held_path = {
            let (data, save, id) = (
                self.inner.config.data_dir.clone(),
                save_path.clone(),
                id.clone(),
            );
            tokio::task::spawn_blocking(move || held_path_for(&data, &save, &id))
                .await
                .expect("held path probe panicked")
        };
        let name = magnet.display_name.clone().unwrap_or_else(|| id.clone());
        let naming = self.choose_naming(&save_path).await;
        let row = TorrentRow {
            space,
            id: id.clone(),
            info_hash_v1: magnet.info_hashes.get_v1().map(|h| h.to_hex()),
            info_hash_v2: magnet.info_hashes.get_v2().map(|h| h.to_hex()),
            name,
            torrent: Vec::new(),
            save_path: save_path.to_string_lossy().into_owned(),
            label: req.label.clone(),
            added_at: now_ms(),
            paused: req.paused,
            have: None,
            file_priorities: None,
            downloaded_total: 0,
            uploaded_total: 0,
            placements: None,
            held_path: Some(held_path.to_string_lossy().into_owned()),
            unwanted_policy: policy_to_byte(req.unwanted_policy),
            queue_position: self.next_queue_position(space),
            download_rate: 0,
            upload_rate: 0,
            max_connections: None,
            magnet: Some(uri.to_owned()),
            naming,
            linked: None,
            file_paths: None,
            trackers: None,
        };
        let (added_at, queue_position) = (row.added_at, row.queue_position);
        self.db(move |db| db.insert_torrent(&row)).await?;
        self.inner
            .pending
            .write()
            .expect("pending lock poisoned")
            .insert(
                key.clone(),
                Pending {
                    magnet,
                    uri: uri.to_owned(),
                    save_path,
                    held_path,
                    label: req.label,
                    added_at,
                    paused: req.paused,
                    unwanted: req.unwanted_policy,
                    queue_position,
                    naming,
                    fetch: None,
                },
            );
        if !req.paused {
            self.start_fetch(&key);
        }
        tracing::info!(torrent = %key, "magnet link added");
        self.refresh();
        Ok(id)
    }

    /// Start tracking an entry. The first entry of a torrent starts its swarm in the engine,
    /// at its own save path; a later one (another space) joins that swarm, and gets its
    /// files in its own save path as they complete (see `place_secondaries`).
    async fn spawn_entry(&self, s: Spawn) -> Result<TorrentId, CoreError> {
        let Spawn {
            key,
            peers,
            torrent,
            trackers,
            save_path,
            held_path,
            have,
            placements,
            paused,
            file_priorities,
            unwanted,
            label,
            added_at,
            totals: (downloaded_base, uploaded_base),
            queue_position,
            limits,
            queued,
            naming,
            linked,
            file_paths: custom_paths,
        } = s;
        let file_paths = match (&custom_paths, naming_from_byte(naming)) {
            (Some(p), _) => Some(p.clone()),
            (None, Naming::Portable) => None,
            (None, other) => match torrent.info.paths(other) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(torrent = %key, error = %e, "native paths failed; using portable names");
                    None
                }
            },
        };
        let group = settings::group_for(&self.settings(), label.as_deref()).map(str::to_owned);
        let entry = EntryParams {
            key: key.engine(),
            trackers: Some(trackers.clone()),
            limits: engine_limits(limits),
            group: group.clone(),
            space: Some(engine_space(key.space)),
            paused: paused || queued,
        };
        let (handle, joined) = {
            let mut swarms = self.inner.swarms.lock().expect("swarms lock poisoned");
            if let Some(swarm) = swarms.get(&key.id) {
                (swarm.handle.clone(), true)
            } else {
                let handle = self
                    .inner
                    .session
                    .add_torrent(AddTorrent {
                        held_path: Some(held_path),
                        parts_path: Some(self.parts_path(&key.id)),
                        have,
                        placements,
                        peers,
                        unwanted: engine_policy(unwanted),
                        paused: entry.paused,
                        file_priorities: Some(file_priorities.clone()),
                        limits: entry.limits,
                        group: group.clone(),
                        entry: entry.key,
                        trackers: entry.trackers.clone(),
                        space: entry.space.clone(),
                        file_paths,
                        linked,
                        ..AddTorrent::new(torrent.clone(), save_path.clone())
                    })
                    .map_err(|_| CoreError::Duplicate)?;
                swarms.insert(
                    key.id.clone(),
                    Swarm {
                        handle: handle.clone(),
                        primary: key.space,
                    },
                );
                (handle, false)
            }
        };
        if joined {
            handle.add_entry(entry).await;
            tracing::info!(torrent = %key, "joined the swarm another space runs");
        }
        let files = torrent.info.files.len();
        self.inner
            .torrents
            .write()
            .expect("torrents lock poisoned")
            .insert(
                key.clone(),
                Entry {
                    torrent,
                    handle,
                    save_path,
                    trackers,
                    placed: vec![false; files],
                    place_retry: None,
                    label,
                    added_at,
                    paused,
                    file_priorities,
                    unwanted,
                    downloaded_base,
                    uploaded_base,
                    queue_position,
                    limits,
                    queued,
                    slow_since: None,
                    group,
                    naming,
                    superseded_by: None,
                    in_deluge: None,
                    leftover_from: None,
                    file_paths: custom_paths,
                },
            );
        if joined {
            self.sync_priorities(&key.id).await;
        }
        Ok(key.id)
    }

    /// The part store of a torrent's swarm.
    fn parts_path(&self, id: &str) -> PathBuf {
        self.inner
            .config
            .data_dir
            .join("parts")
            .join(format!("{id}.parts"))
    }

    /// Give a swarm the file priorities of all its entries: a file any of them wants is
    /// downloaded, at the highest priority any of them gives it.
    async fn sync_priorities(&self, id: &str) {
        let (handle, merged) = {
            let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
            let mut handle = None;
            let mut merged: Vec<u8> = Vec::new();
            for (k, e) in torrents.iter().filter(|(k, _)| k.id == id) {
                let _ = k;
                handle.get_or_insert_with(|| e.handle.clone());
                if merged.len() < e.file_priorities.len() {
                    merged.resize(e.file_priorities.len(), 0);
                }
                for (m, &p) in merged.iter_mut().zip(&e.file_priorities) {
                    *m = (*m).max(p);
                }
            }
            (handle, merged)
        };
        if let Some(handle) = handle {
            handle.set_file_priorities(merged).await;
        }
    }

    /// Add a torrent and start it (unless the request asks for it paused).
    pub async fn add(
        &self,
        space: SpaceId,
        req: AddTorrentRequest,
    ) -> Result<TorrentId, CoreError> {
        self.space(space)?;
        let bytes = match &req.source {
            AddSource::TorrentFile { data } => base64::engine::general_purpose::STANDARD
                .decode(data.trim())
                .map_err(|e| CoreError::InvalidRequest(format!("bad base64: {e}")))?,
            AddSource::Magnet { uri } => {
                let uri = uri.clone();
                return self.add_magnet(space, &uri, req).await;
            }
        };
        self.add_bytes(space, bytes, req).await
    }

    /// Add a `.torrent` file's bytes to a space.
    async fn add_bytes(
        &self,
        space: SpaceId,
        bytes: Vec<u8>,
        req: AddTorrentRequest,
    ) -> Result<TorrentId, CoreError> {
        let torrent = Arc::new(
            Torrent::from_bytes(&bytes).map_err(|e| CoreError::InvalidTorrent(e.to_string()))?,
        );
        let id = torrent.info.info_hashes().id_string();
        let key = Key::new(space, id.clone());
        if self.taken(&key) {
            return Err(CoreError::Duplicate);
        }
        let files = torrent.info.files.len();
        let prios = match req.file_priorities {
            Some(p) if p.len() == files => p,
            Some(p) => {
                return Err(CoreError::InvalidRequest(format!(
                    "{} file priorities for {files} files",
                    p.len()
                )));
            }
            None => vec![4; files],
        };
        let save_path =
            self.resolve_save_path(space, req.save_path.as_deref(), req.label.as_deref())?;
        let hashes = torrent.info.info_hashes();
        let held_path = {
            let (data, save, id) = (
                self.inner.config.data_dir.clone(),
                save_path.clone(),
                id.clone(),
            );
            tokio::task::spawn_blocking(move || held_path_for(&data, &save, &id))
                .await
                .expect("held path probe panicked")
        };
        let row = TorrentRow {
            space,
            id: id.clone(),
            info_hash_v1: hashes.get_v1().map(|h| h.to_hex()),
            info_hash_v2: hashes.get_v2().map(|h| h.to_hex()),
            name: torrent.info.name.clone(),
            torrent: bytes,
            save_path: save_path.to_string_lossy().into_owned(),
            label: req.label.clone(),
            added_at: now_ms(),
            paused: req.paused,
            have: None,
            file_priorities: Some(prios.clone()),
            downloaded_total: 0,
            uploaded_total: 0,
            placements: None,
            held_path: Some(held_path.to_string_lossy().into_owned()),
            unwanted_policy: policy_to_byte(req.unwanted_policy),
            queue_position: self.next_queue_position(space),
            download_rate: 0,
            upload_rate: 0,
            max_connections: None,
            magnet: None,
            naming: self.choose_naming(&save_path).await,
            linked: None,
            file_paths: None,
            trackers: None,
        };
        let queue_position = row.queue_position;
        let naming = row.naming;
        let added_at = row.added_at;
        self.db(move |db| db.insert_torrent(&row)).await?;
        // A torrent another space has joins its swarm, and takes its files from there.
        let linked = if self.swarm_exists(&id) {
            Vec::new()
        } else {
            self.link_from_library(&key, &torrent, &save_path, naming, &prios)
                .await
        };
        let spawned = self
            .spawn_entry(Spawn {
                key: key.clone(),
                peers: Vec::new(),
                trackers: torrent.trackers.clone(),
                torrent,
                save_path,
                held_path,
                have: None,
                placements: None,
                paused: req.paused,
                file_priorities: prios,
                unwanted: req.unwanted_policy,
                label: req.label,
                added_at,
                totals: (0, 0),
                queue_position,
                limits: TorrentLimits::default(),
                queued: !req.paused && self.queue_limited(),
                naming,
                linked,
                file_paths: None,
            })
            .await;
        if let Err(e) = spawned {
            let k = key.clone();
            self.db(move |db| db.remove_torrent(&k)).await?;
            return Err(e);
        }
        // Before the check ends, so bookend pieces are held back from peers from the start.
        let reused = self.inner.reuse.pending_files(&key.id);
        if !reused.is_empty()
            && let Ok(handle) = self.handle(&key)
        {
            handle.set_reused(reused).await;
        }
        tracing::info!(torrent = %key, "torrent added");
        // Only the new torrent changed. A full queue pass and refresh walk every torrent,
        // which made adding the n-th torrent cost O(n); both run every second anyway.
        if self.queue_limited() {
            self.apply_queue().await;
        }
        self.refresh_one(&key);
        Ok(id)
    }

    fn handle(&self, key: &Key) -> Result<TorrentHandle, CoreError> {
        self.inner
            .torrents
            .read()
            .expect("torrents lock poisoned")
            .get(key)
            .map(|e| e.handle.clone())
            .ok_or(CoreError::NotFound)
    }

    /// Whether an entry's files are its swarm's own (the engine torrent's storage).
    fn is_primary(&self, key: &Key) -> bool {
        self.inner
            .swarms
            .lock()
            .expect("swarms lock poisoned")
            .get(&key.id)
            .is_some_and(|s| s.primary == key.space)
    }

    /// Before an imported torrent runs here, make sure the client it came from has stopped
    /// it (see `takeover`). Torrents that weren't imported, or were taken over already,
    /// pass straight through.
    async fn ensure_taken_over(&self, key: &Key) -> Result<(), CoreError> {
        let k = key.clone();
        let Some(row) = self.db(move |db| db.imported_one(&k)).await? else {
            return Ok(());
        };
        if row.taken_over_at.is_some() {
            return Ok(());
        }
        takeover::pause_in_deluge(Path::new(&row.source), &[row.source_id().to_owned()])
            .await
            .map_err(CoreError::OtherOwner)?;
        self.mark_taken_over(key, now_ms()).await
    }

    /// The Deluge installations on this machine (see `apitorrent_deluge::discover`), with
    /// how many of each one's torrents are already imported here and elsewhere.
    pub async fn find_deluge(
        &self,
        space: SpaceId,
    ) -> Result<Vec<apitorrent_types::DelugeInstall>, CoreError> {
        use apitorrent_deluge::discover;
        let found = tokio::task::spawn_blocking(|| discover::discover(&discover::Places::system()))
            .await
            .expect("Deluge discovery panicked");
        let rows = self.db(|db| db.imported()).await?;
        // Imports record the directory as it was typed: compare canonical forms.
        let canonical = |p: &str| std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p));
        let mut sources: std::collections::HashMap<String, PathBuf> =
            std::collections::HashMap::new();
        for r in &rows {
            sources
                .entry(r.source.clone())
                .or_insert_with(|| canonical(&r.source));
        }
        Ok(found
            .into_iter()
            .map(|f| {
                let (mut here, mut elsewhere) = (0u32, 0u32);
                for r in rows.iter().filter(|r| sources[&r.source] == f.config_dir) {
                    if r.key.space == space {
                        here += 1;
                    } else {
                        elsewhere += 1;
                    }
                }
                apitorrent_types::DelugeInstall {
                    config_dir: f.config_dir.display().to_string(),
                    suggested_slug: deluge_import::suggested_slug(Path::new(&f.name)),
                    name: f.name,
                    running: f.running,
                    units: f.units,
                    daemon_port: f.daemon_port,
                    torrents: f.torrents,
                    problem: f.problem,
                    imported_here: here,
                    imported_elsewhere: elsewhere,
                }
            })
            .collect())
    }

    /// The space's imported torrents that the client they came from may still run, by
    /// that client's config directory.
    pub async fn imports_not_taken_over(
        &self,
        space: SpaceId,
    ) -> Result<Vec<apitorrent_types::ImportSource>, CoreError> {
        let rows = self.db(|db| db.imported()).await?;
        let mut by_source: std::collections::BTreeMap<String, u32> =
            std::collections::BTreeMap::new();
        for r in rows
            .iter()
            .filter(|r| r.key.space == space && r.taken_over_at.is_none())
        {
            *by_source.entry(r.source.clone()).or_default() += 1;
        }
        Ok(by_source
            .into_iter()
            .map(|(config_dir, torrents)| apitorrent_types::ImportSource {
                config_dir,
                torrents,
            })
            .collect())
    }

    /// Take over the space's imported torrents still waiting (all of them, or those in
    /// `ids`): pause them in the client they came from, one call per config directory,
    /// then resume them here.
    pub async fn take_over(
        &self,
        space: SpaceId,
        ids: &[String],
    ) -> Result<apitorrent_types::TakeOverReport, CoreError> {
        self.take_over_with(space, ids, &Progress::none()).await
    }

    async fn take_over_with(
        &self,
        space: SpaceId,
        ids: &[String],
        progress: &Progress,
    ) -> Result<apitorrent_types::TakeOverReport, CoreError> {
        let rows = self.db(|db| db.imported()).await?;
        let mut groups: std::collections::BTreeMap<String, Vec<db::ImportedRow>> =
            std::collections::BTreeMap::new();
        for r in rows.into_iter().filter(|r| {
            r.key.space == space
                && r.taken_over_at.is_none()
                && (ids.is_empty() || ids.contains(&r.key.id))
        }) {
            groups.entry(r.source.clone()).or_default().push(r);
        }
        let name = |key: &Key| {
            self.inner
                .torrents
                .read()
                .expect("torrents lock poisoned")
                .get(key)
                .map_or_else(|| key.id.clone(), |e| e.torrent.info.name.clone())
        };
        let mut report = apitorrent_types::TakeOverReport::default();
        let total = u32::try_from(groups.values().map(Vec::len).sum::<usize>()).unwrap_or(u32::MAX);
        for (source, rows) in groups {
            progress.phase("Pausing them in Deluge", total);
            let deluge_ids: Vec<String> = rows.iter().map(|r| r.source_id().to_owned()).collect();
            if let Err(e) = takeover::pause_in_deluge(Path::new(&source), &deluge_ids).await {
                report
                    .skipped
                    .extend(rows.iter().map(|r| apitorrent_types::ImportSkip {
                        name: name(&r.key),
                        reason: e.clone(),
                    }));
                progress.steps(u32::try_from(rows.len()).unwrap_or(u32::MAX), None);
                continue;
            }
            progress.phase("Starting them here", total);
            let at = now_ms();
            for r in rows {
                progress.working_on(&name(&r.key));
                self.mark_taken_over(&r.key, at).await?;
                match self.resume(&r.key).await {
                    Ok(()) => report.taken_over += 1,
                    Err(e) => report.skipped.push(apitorrent_types::ImportSkip {
                        name: name(&r.key),
                        reason: e.to_string(),
                    }),
                }
                progress.step(None);
            }
        }
        Ok(report)
    }

    pub async fn pause(&self, key: &Key) -> Result<(), CoreError> {
        self.set_paused(key, true).await
    }

    pub async fn resume(&self, key: &Key) -> Result<(), CoreError> {
        self.set_paused(key, false).await
    }

    async fn set_paused(&self, key: &Key, paused: bool) -> Result<(), CoreError> {
        if !paused {
            self.ensure_taken_over(key).await?;
        }
        let is_pending = {
            let mut pending = self.inner.pending.write().expect("pending lock poisoned");
            match pending.get_mut(key) {
                Some(p) => {
                    p.paused = paused;
                    if paused {
                        // Dropping the sender cancels the fetch.
                        p.fetch = None;
                    }
                    true
                }
                None => false,
            }
        };
        if is_pending {
            let k = key.clone();
            self.db(move |db| db.set_paused(&k, paused)).await?;
            if !paused {
                self.start_fetch(key);
            }
            self.refresh();
            return Ok(());
        }
        let handle = self.handle(key)?;
        let k = key.clone();
        self.db(move |db| db.set_paused(&k, paused)).await?;
        if let Some(e) = self
            .inner
            .torrents
            .write()
            .expect("torrents lock poisoned")
            .get_mut(key)
        {
            e.paused = paused;
            e.queued = false;
        }
        if paused {
            handle.pause_entry(key.engine()).await;
        } else {
            handle.resume_entry(key.engine()).await;
        }
        self.apply_queue().await;
        self.refresh();
        Ok(())
    }

    /// Change the priority of some files; the others keep theirs. Pad entries can't be
    /// changed (they are never downloaded).
    pub async fn set_file_priorities(
        &self,
        key: &Key,
        updates: &[FilePriority],
    ) -> Result<(), CoreError> {
        let prios = {
            let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
            let entry = torrents.get(key).ok_or(CoreError::NotFound)?;
            let files = &entry.torrent.info.files;
            let mut prios = entry.file_priorities.clone();
            for u in updates {
                let i = u.index as usize;
                if files
                    .get(i)
                    .is_none_or(apitorrent_metainfo::FileEntry::is_pad)
                {
                    return Err(CoreError::InvalidRequest(format!("no file {}", u.index)));
                }
                if u.priority > 7 {
                    return Err(CoreError::InvalidRequest(format!(
                        "priority {} is above the maximum of 7",
                        u.priority
                    )));
                }
                prios[i] = u.priority;
            }
            prios
        };
        let k = key.clone();
        let stored = prios.clone();
        self.db(move |db| db.set_file_priorities(&k, &stored))
            .await?;
        if let Some(e) = self
            .inner
            .torrents
            .write()
            .expect("torrents lock poisoned")
            .get_mut(key)
        {
            e.file_priorities.clone_from(&prios);
        }
        self.sync_priorities(&key.id).await;
        Ok(())
    }

    /// Choose what happens to files of this torrent that are deselected from now on. Only
    /// the swarm's primary has deselected files of its own; another space's setting takes
    /// effect if its entry becomes the primary.
    pub async fn set_unwanted_policy(
        &self,
        key: &Key,
        policy: UnwantedPolicy,
    ) -> Result<(), CoreError> {
        let handle = self.handle(key)?;
        let k = key.clone();
        self.db(move |db| db.set_unwanted_policy(&k, policy_to_byte(policy)))
            .await?;
        if let Some(e) = self
            .inner
            .torrents
            .write()
            .expect("torrents lock poisoned")
            .get_mut(key)
        {
            e.unwanted = policy;
        }
        if self.is_primary(key) {
            handle.set_unwanted_policy(engine_policy(policy)).await;
        }
        Ok(())
    }

    /// Delete held copies of deselected files (all of them when `files` is `None`).
    pub async fn release_held(&self, key: &Key, files: Option<Vec<u32>>) -> Result<(), CoreError> {
        let handle = self.handle(key)?;
        if self.is_primary(key) {
            let files = files.map(|f| f.into_iter().map(|i| i as usize).collect());
            handle.release_held(files).await;
        }
        Ok(())
    }

    /// Where each of an entry's files is on disk.
    fn entry_paths(e: &Entry) -> Vec<PathBuf> {
        e.file_paths.as_ref().map_or_else(
            || Self::disk_paths(&e.torrent, &e.save_path, e.naming),
            |c| {
                c.iter()
                    .map(|p| p.iter().fold(e.save_path.clone(), |a, x| a.join(x)))
                    .collect()
            },
        )
    }

    /// Remove a torrent from its space, and optionally its downloaded files. Only this
    /// space's files are touched: another space's copy of the same torrent keeps its files
    /// and keeps seeding. Held copies and the part store go with the swarm's last entry.
    pub async fn remove(&self, key: &Key, delete_files: bool) -> Result<(), CoreError> {
        let pending = self
            .inner
            .pending
            .write()
            .expect("pending lock poisoned")
            .remove(key);
        if pending.is_some() {
            let k = key.clone();
            self.db(move |db| db.remove_torrent(&k)).await?;
            self.inner.store.apply(Vec::new(), vec![key.clone()]);
            tracing::info!(torrent = %key, "magnet link removed");
            return Ok(());
        }
        let (entry, others) = {
            let mut torrents = self.inner.torrents.write().expect("torrents lock poisoned");
            let entry = torrents.remove(key).ok_or(CoreError::NotFound)?;
            let mut others: Vec<(Key, i64)> = torrents
                .iter()
                .filter(|(k, _)| k.id == key.id)
                .map(|(k, e)| (k.clone(), e.added_at))
                .collect();
            others.sort_by_key(|(k, at)| (*at, k.space));
            (entry, others)
        };
        let primary = self.is_primary(key);
        let mine = Self::entry_paths(&entry);
        if !delete_files {
            // Files left behind under a library folder stay reusable (see `folders`).
            let complete = self
                .complete_files(key, &entry.handle, &entry.torrent, &entry.placed)
                .await;
            self.keep_as_folder_files(&entry.torrent, &mine, &complete)
                .await;
        }
        // Paths another entry of the torrent also uses (two spaces saving to one folder)
        // are never deleted.
        let shared: std::collections::HashSet<PathBuf> = {
            let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
            others
                .iter()
                .filter_map(|(k, _)| torrents.get(k))
                .flat_map(Self::entry_paths)
                .collect()
        };
        if others.is_empty() {
            entry.handle.shutdown().await;
            self.inner
                .swarms
                .lock()
                .expect("swarms lock poisoned")
                .remove(&key.id);
            self.inner.reuse.forget(&key.id);
            let id = key.id.clone();
            if let Err(e) = self.db(move |db| db.delete_reuse(&id)).await {
                tracing::warn!(torrent = %key, error = %e, "could not delete file reuse records");
            }
            if let Err(e) = entry.handle.delete_data(delete_files).await {
                tracing::warn!(torrent = %key, error = %e, "could not delete all of the torrent's data");
            }
        } else if primary {
            // The swarm moves to the next entry's files.
            entry.handle.shutdown().await;
            if let Err(e) = entry.handle.delete_data(false).await {
                tracing::warn!(torrent = %key, error = %e, "could not delete the held copies and part store");
            }
            if delete_files {
                Self::delete_paths(&mine, &shared, &entry.save_path).await;
            }
            let next: Vec<Key> = others.iter().map(|(k, _)| k.clone()).collect();
            if let Err(e) = self.move_swarm(&key.id, &next) {
                tracing::error!(torrent = %key, error = %e, "could not move the swarm to another space's files");
            }
        } else {
            entry.handle.remove_entry(key.engine()).await;
            if delete_files {
                Self::delete_paths(&mine, &shared, &entry.save_path).await;
            }
            self.sync_priorities(&key.id).await;
        }
        let k = key.clone();
        self.db(move |db| db.remove_torrent(&k)).await?;
        self.inner.store.apply(Vec::new(), vec![key.clone()]);
        self.inner
            .seeding
            .lock()
            .expect("seeding lock poisoned")
            .remove(key);
        tracing::info!(torrent = %key, delete_files, "torrent removed");
        Ok(())
    }

    /// Delete an entry's own files, keeping any another entry uses, and the folders that
    /// end up empty below the save path.
    async fn delete_paths(
        paths: &[PathBuf],
        keep: &std::collections::HashSet<PathBuf>,
        root: &std::path::Path,
    ) {
        let paths: Vec<PathBuf> = paths
            .iter()
            .filter(|p| !keep.contains(*p))
            .cloned()
            .collect();
        let root = root.to_path_buf();
        let _ = tokio::task::spawn_blocking(move || {
            for p in &paths {
                match std::fs::remove_file(p) {
                    Ok(()) => {
                        let mut dir = p.parent();
                        while let Some(d) = dir {
                            if d == root || !d.starts_with(&root) || std::fs::remove_dir(d).is_err()
                            {
                                break;
                            }
                            dir = d.parent();
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => tracing::warn!(path = %p.display(), error = %e, "could not delete"),
                }
            }
        })
        .await;
    }

    /// Restart a swarm on the files of the first of `entries` (oldest first), the others
    /// joining it: its primary was removed. The new primary's files are checked, since
    /// it may have only some of them.
    fn move_swarm(&self, id: &str, entries: &[Key]) -> Result<(), CoreError> {
        let Some((first, rest)) = entries.split_first() else {
            return Ok(());
        };
        let settings = self.settings();
        let (params, mut extra) = {
            let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
            let e = torrents.get(first).ok_or(CoreError::NotFound)?;
            let mut merged: Vec<u8> = e.file_priorities.clone();
            let extra: Vec<EntryParams> = rest
                .iter()
                .filter_map(|k| torrents.get(k).map(|x| (k, x)))
                .map(|(k, x)| {
                    for (m, &p) in merged.iter_mut().zip(&x.file_priorities) {
                        *m = (*m).max(p);
                    }
                    EntryParams {
                        key: k.engine(),
                        trackers: Some(x.trackers.clone()),
                        limits: engine_limits(x.limits),
                        group: x.group.clone(),
                        space: Some(engine_space(k.space)),
                        paused: x.paused || x.queued,
                    }
                })
                .collect();
            let file_paths = match (&e.file_paths, naming_from_byte(e.naming)) {
                (Some(p), _) => Some(p.clone()),
                (None, Naming::Portable) => None,
                (None, other) => e.torrent.info.paths(other).ok(),
            };
            let params = AddTorrent {
                held_path: Some(held_path_for(&self.inner.config.data_dir, &e.save_path, id)),
                parts_path: Some(self.parts_path(id)),
                unwanted: engine_policy(e.unwanted),
                paused: e.paused || e.queued,
                file_priorities: Some(merged),
                limits: engine_limits(e.limits),
                group: settings::group_for(&settings, e.label.as_deref()).map(str::to_owned),
                entry: first.engine(),
                trackers: Some(e.trackers.clone()),
                space: Some(engine_space(first.space)),
                file_paths,
                ..AddTorrent::new(e.torrent.clone(), e.save_path.clone())
            };
            (params, extra)
        };
        let handle = {
            let mut swarms = self.inner.swarms.lock().expect("swarms lock poisoned");
            let handle = self
                .inner
                .session
                .add_torrent(AddTorrent {
                    more_entries: std::mem::take(&mut extra),
                    ..params
                })
                .map_err(|_| CoreError::Duplicate)?;
            swarms.insert(
                id.to_owned(),
                Swarm {
                    handle: handle.clone(),
                    primary: first.space,
                },
            );
            handle
        };
        let mut torrents = self.inner.torrents.write().expect("torrents lock poisoned");
        for k in entries {
            if let Some(e) = torrents.get_mut(k) {
                e.handle = handle.clone();
                e.placed.iter_mut().for_each(|p| *p = false);
            }
        }
        tracing::info!(torrent = %first, "the swarm moved to this space's files");
        Ok(())
    }

    /// Give every entry that isn't its swarm's primary the files that are complete and
    /// that it wants, in its own save path: a hard link where the volume allows, else a
    /// copy-on-write clone, else a copy (when the library setting allows copies). A file
    /// already at the path is left alone.
    async fn place_secondaries(&self) {
        let allow_copy = self.settings().library.allow_copy;
        let now = std::time::Instant::now();
        // (secondary, its primary, the swarm, the files still to place)
        let work: Vec<(Key, Key, TorrentHandle, Vec<usize>)> = {
            let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
            let swarms = self.inner.swarms.lock().expect("swarms lock poisoned");
            torrents
                .iter()
                .filter_map(|(k, e)| {
                    let swarm = swarms.get(&k.id)?;
                    if swarm.primary == k.space || e.place_retry.is_some_and(|t| now < t) {
                        return None;
                    }
                    let todo: Vec<usize> = (0..e.placed.len())
                        .filter(|&i| {
                            !e.placed[i]
                                && e.file_priorities.get(i).is_some_and(|&p| p > 0)
                                && !e.torrent.info.files[i].is_pad()
                        })
                        .collect();
                    (!todo.is_empty()).then(|| {
                        (
                            k.clone(),
                            Key::new(swarm.primary, k.id.clone()),
                            swarm.handle.clone(),
                            todo,
                        )
                    })
                })
                .collect()
        };
        for (key, primary, handle, todo) in work {
            let Some(have) = handle.have().await else {
                continue;
            };
            let placements = handle.placements();
            let pairs: Vec<(usize, PathBuf, PathBuf)> = {
                let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
                let (Some(e), Some(p)) = (torrents.get(&key), torrents.get(&primary)) else {
                    continue;
                };
                let layout = Layout::from_info(&e.torrent.info);
                let (to, from) = (Self::entry_paths(e), Self::entry_paths(p));
                todo.into_iter()
                    .filter(|&i| {
                        placements.get(i) == Some(&Placement::Save)
                            && layout.file_pieces(i).all(|piece| have.get(piece))
                    })
                    .map(|i| (i, from[i].clone(), to[i].clone()))
                    .collect()
            };
            if pairs.is_empty() {
                continue;
            }
            let k = key.clone();
            let results: Vec<(usize, bool)> = tokio::task::spawn_blocking(move || {
                pairs
                    .into_iter()
                    .map(|(i, from, to)| {
                        if from == to || to.exists() {
                            return (i, true);
                        }
                        let how = library::materialize(&from, &to, true, allow_copy);
                        if let Some(how) = how {
                            tracing::info!(torrent = %k, file = %to.display(), ?how, "placed a file from another space's copy");
                        } else {
                            tracing::warn!(torrent = %k, file = %to.display(), "could not link or copy a file from another space's copy");
                        }
                        (i, how.is_some())
                    })
                    .collect()
            })
            .await
            .unwrap_or_default();
            let mut torrents = self.inner.torrents.write().expect("torrents lock poisoned");
            if let Some(e) = torrents.get_mut(&key) {
                for (i, ok) in results {
                    if ok {
                        e.placed[i] = true;
                    } else {
                        e.place_retry = Some(now + Duration::from_secs(60));
                    }
                }
            }
        }
    }

    /// Torrents of one space, or of every space (`None`).
    pub fn list(&self, space: Option<SpaceId>) -> Vec<TorrentSummary> {
        self.inner.store.list(space)
    }

    pub async fn detail(&self, key: &Key) -> Result<TorrentDetail, CoreError> {
        let pending = self
            .inner
            .pending
            .read()
            .expect("pending lock poisoned")
            .get(key)
            .map(|p| {
                (
                    p.uri.clone(),
                    p.magnet.trackers.clone(),
                    p.unwanted,
                    p.naming,
                )
            });
        if let Some((uri, trackers, unwanted_policy, naming)) = pending {
            let summary = self.inner.store.get(key).ok_or(CoreError::NotFound)?;
            return Ok(TorrentDetail {
                summary,
                files: Vec::new(),
                trackers: trackers.into_iter().map(|t| vec![t]).collect(),
                comment: None,
                created_by: None,
                creation_date: None,
                piece_length: 0,
                magnet: uri,
                unwanted_policy,
                limits: TorrentLimits::default(),
                naming: policy_of(naming),
                peers: Vec::new(),
                tracker_status: Vec::new(),
                seed_limits: None,
                effective_seed_limits: self.settings().seed_limits,
                seeding_secs: 0,
                seed_limit_reached: false,
                library_bytes: 0,
            });
        }
        let (torrent, handle, prios, unwanted_policy, limits, naming, trackers) = {
            let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
            let e = torrents.get(key).ok_or(CoreError::NotFound)?;
            (
                e.torrent.clone(),
                e.handle.clone(),
                e.file_priorities.clone(),
                e.unwanted,
                e.limits,
                e.naming,
                e.trackers.clone(),
            )
        };
        let primary = self.is_primary(key);
        let placements = {
            let h = handle.clone();
            tokio::task::spawn_blocking(move || h.placements())
                .await
                .expect("placements task panicked")
        };
        let summary = self.inner.store.get(key).ok_or(CoreError::NotFound)?;
        let have = handle
            .have()
            .await
            .unwrap_or_else(|| Bitfield::new(summary.pieces_total));
        let layout = Layout::from_info(&torrent.info);
        let reuse = self.inner.reuse.of(&key.id);
        let library_bytes = reuse.values().map(|r| r.bytes).sum();
        let files = torrent
            .info
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| !f.is_pad())
            .map(|(i, f)| {
                let pieces = layout.file_pieces(i);
                let total = pieces.len();
                let done = pieces.filter(|&p| have.get(p)).count();
                let priority = prios.get(i).copied().unwrap_or(4);
                TorrentFile {
                    index: u32::try_from(i).expect("fits"),
                    path: f.display_path(),
                    length: f.length,
                    priority,
                    progress: if total == 0 { 1.0 } else { ratio(done, total) },
                    // Another space's entry has its wanted files in its save path only.
                    placement: match placements.get(i) {
                        _ if !primary && priority == 0 => FilePlacement::Parts,
                        _ if !primary => FilePlacement::Save,
                        Some(Placement::Held) => FilePlacement::Held,
                        Some(Placement::Parts) => FilePlacement::Parts,
                        _ => FilePlacement::Save,
                    },
                    reused: reuse.get(&i).cloned(),
                }
            })
            .collect();
        let mut magnet = Magnet::new(torrent.info.info_hashes());
        magnet.display_name = Some(torrent.info.name.clone());
        magnet.trackers = trackers.iter().flatten().cloned().collect();
        let seed = self.seed_state(key);
        let group = self
            .inner
            .torrents
            .read()
            .expect("torrents lock poisoned")
            .get(key)
            .and_then(|e| e.group.clone());
        let effective_seed_limits =
            seeding::effective(&self.settings(), group.as_deref(), seed.limits);
        let entry = key.engine();
        Ok(TorrentDetail {
            seed_limits: seed.limits,
            effective_seed_limits,
            seeding_secs: seed.seed_ms / 1000,
            seed_limit_reached: seed.reached,
            library_bytes,
            summary,
            files,
            trackers,
            comment: torrent.comment.clone(),
            created_by: torrent.created_by.clone(),
            creation_date: torrent.creation_date,
            piece_length: torrent.info.piece_length,
            magnet: magnet.to_uri(),
            unwanted_policy,
            limits,
            naming: policy_of(naming),
            peers: handle
                .peers()
                .await
                .into_iter()
                .filter(|p| p.entry == entry)
                .map(peer_info)
                .collect(),
            tracker_status: handle
                .trackers_of(Some(entry))
                .await
                .into_iter()
                .map(|t| apitorrent_types::TrackerInfo {
                    url: t.url,
                    tier: u32::try_from(t.tier).unwrap_or(u32::MAX),
                    peers: t.peers,
                    seeders: t.seeders,
                    leechers: t.leechers,
                    error: t.error,
                    warning: t.warning,
                })
                .collect(),
        })
    }

    fn seed_state(&self, key: &Key) -> seeding::SeedState {
        self.inner
            .seeding
            .lock()
            .expect("seeding lock poisoned")
            .get(key)
            .copied()
            .unwrap_or_default()
    }

    /// Set a torrent's own seed limits (`None` follows its group's or the instance's). A
    /// change lets the new limits act, even if earlier ones already did.
    pub async fn set_seed_limits(
        &self,
        key: &Key,
        limits: Option<apitorrent_types::SeedLimits>,
    ) -> Result<(), CoreError> {
        self.handle(key)?;
        if let Some(l) = limits
            && (!l.ratio.is_finite() || l.ratio < 0.0)
        {
            return Err(CoreError::InvalidRequest(
                "the ratio must be a number, 0 or more".into(),
            ));
        }
        let state = {
            let mut map = self.inner.seeding.lock().expect("seeding lock poisoned");
            let s = map.entry(key.clone()).or_default();
            s.limits = limits;
            s.reached = false;
            *s
        };
        let k = key.clone();
        self.db(move |db| db.save_seed_states(&[(k, state)]))
            .await?;
        Ok(())
    }

    /// Count seeding time, and act on torrents that reached their seed limits. Called every
    /// refresh. Only torrents seeding right now count time: not paused, not queued. Each
    /// space's entry counts its own ratio: its own peers' traffic only.
    async fn apply_seed_limits(&self) {
        let elapsed = {
            let mut clock = self.inner.seed_clock.lock().expect("seed clock poisoned");
            let now = std::time::Instant::now();
            let e = now.duration_since(*clock);
            *clock = now;
            e
        };
        let settings = self.settings();
        let mut due: Vec<(Key, apitorrent_types::SeedLimitAction)> = Vec::new();
        {
            let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
            let mut seeding = self.inner.seeding.lock().expect("seeding lock poisoned");
            for (key, e) in torrents.iter() {
                let stats = e.handle.stats();
                if e.paused || e.queued || stats.state != EngineState::Seeding {
                    continue;
                }
                let mine = entry_stats(&stats, key);
                let s = seeding.entry(key.clone()).or_default();
                s.seed_ms += u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
                if s.reached {
                    continue;
                }
                let limits = seeding::effective(&settings, e.group.as_deref(), s.limits);
                if !limits.active() {
                    continue;
                }
                let size: u64 = e
                    .torrent
                    .info
                    .files
                    .iter()
                    .filter(|f| !f.is_pad())
                    .map(|f| f.length)
                    .sum();
                let ratio = seeding::ratio(
                    e.uploaded_base + mine.uploaded,
                    e.downloaded_base + mine.downloaded,
                    size,
                );
                if seeding::reached(&limits, ratio, s.seed_ms / 1000) {
                    s.reached = true;
                    due.push((key.clone(), limits.action));
                }
            }
        }
        for (key, action) in due {
            use apitorrent_types::SeedLimitAction as A;
            tracing::info!(torrent = %key, ?action, "seed limits reached");
            let result = match action {
                A::Pause => self.pause(&key).await,
                A::Remove => self.remove(&key, false).await,
                A::RemoveWithData => self.remove(&key, true).await,
            };
            if let Err(e) = result {
                tracing::warn!(torrent = %key, error = %e, "could not act on the seed limits");
            }
            if action == A::Pause {
                let state = self.seed_state(&key);
                let k = key.clone();
                let _ = self.db(move |db| db.save_seed_states(&[(k, state)])).await;
            }
        }
    }

    /// Session-wide numbers for a status bar.
    pub fn session_stats(&self) -> apitorrent_types::SessionStats {
        let s = &self.inner.session;
        apitorrent_types::SessionStats {
            connections: u32::try_from(s.connections()).unwrap_or(u32::MAX),
            memory_used: s.memory_used(),
            memory_limit: s.memory_limit(),
            banned: u32::try_from(s.bans().len()).unwrap_or(u32::MAX),
            peer_port: s.listen_addr().port(),
            dht_nodes: s.dht_stats().map(|d| {
                let n = d.ipv4.map_or(0, |t| t.nodes) + d.ipv6.map_or(0, |t| t.nodes);
                u32::try_from(n).unwrap_or(u32::MAX)
            }),
            library_bytes: self.inner.reuse.total(),
        }
    }

    /// Publish one torrent's summary, when only it changed.
    fn refresh_one(&self, key: &Key) {
        let summary = self
            .inner
            .torrents
            .read()
            .expect("torrents lock poisoned")
            .get(key)
            .map(|e| summarize(key, e));
        if let Some(s) = summary {
            self.inner.store.apply(vec![s], Vec::new());
        }
    }

    /// Rebuild every summary from the engine and publish the changes.
    fn refresh(&self) {
        let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
        let mut summaries: Vec<TorrentSummary> =
            torrents.iter().map(|(k, e)| summarize(k, e)).collect();
        drop(torrents);
        let pending = self.inner.pending.read().expect("pending lock poisoned");
        summaries.extend(pending.iter().map(|(k, p)| summarize_pending(k, p)));
        drop(pending);
        self.inner.store.apply(summaries, Vec::new());
    }

    /// Save resume state. Only a swarm's primary has pieces and placements of its own; the
    /// others save their totals.
    async fn save_resume(&self) -> Result<(), CoreError> {
        let primaries: std::collections::HashSet<Key> = self
            .inner
            .swarms
            .lock()
            .expect("swarms lock poisoned")
            .iter()
            .map(|(id, s)| Key::new(s.primary, id.clone()))
            .collect();
        let entries: Vec<(Key, TorrentHandle, u64, u64)> = self
            .inner
            .torrents
            .read()
            .expect("torrents lock poisoned")
            .iter()
            .map(|(k, e)| {
                (
                    k.clone(),
                    e.handle.clone(),
                    e.downloaded_base,
                    e.uploaded_base,
                )
            })
            .collect();
        let mut rows = Vec::with_capacity(entries.len());
        for (key, handle, down, up) in entries {
            let stats = handle.stats();
            let mine = entry_stats(&stats, &key);
            // A torrent still checking has no pieces yet: saving none makes the next start
            // check again, where an empty bitfield would trust that nothing is there.
            if !primaries.contains(&key) || stats.state == EngineState::Checking {
                rows.push(ResumeRow {
                    key,
                    have: Vec::new(),
                    placements: Vec::new(),
                    linked: Vec::new(),
                    downloaded_total: down + mine.downloaded,
                    uploaded_total: up + mine.uploaded,
                });
                continue;
            }
            if let Some(have) = handle.have().await {
                let h = handle.clone();
                let placements = tokio::task::spawn_blocking(move || h.placements())
                    .await
                    .expect("placements task panicked");
                let linked = pack_indices(&handle.linked());
                rows.push(ResumeRow {
                    key,
                    have: have.as_bytes().to_vec(),
                    placements: placements.into_iter().map(placement_to_byte).collect(),
                    linked,
                    downloaded_total: down + mine.downloaded,
                    uploaded_total: up + mine.uploaded,
                });
            }
        }
        self.db(move |db| db.save_resume(&rows)).await?;
        let seeding: Vec<(Key, seeding::SeedState)> = self
            .inner
            .seeding
            .lock()
            .expect("seeding lock poisoned")
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        self.db(move |db| db.save_seed_states(&seeding)).await?;
        if let Some(state) = self.inner.session.dht_state().await {
            let encoded = base64::engine::general_purpose::STANDARD.encode(state);
            self.db(move |db| db.set_meta(DHT_STATE_KEY, &encoded))
                .await?;
        }
        Ok(())
    }

    async fn background(self) {
        let mut refresh = tokio::time::interval(REFRESH);
        let mut save = tokio::time::interval(SAVE_EVERY);
        save.tick().await;
        let mut retire = tokio::time::interval(RETIRE_EVERY);
        retire.tick().await;
        loop {
            tokio::select! {
                _ = refresh.tick() => {
                    // Torrents that finish move from the download limit to the seed limit.
                    self.apply_queue().await;
                    self.settle_supersedes().await;
                    self.settle_reuse().await;
                    self.apply_seed_limits().await;
                    self.place_secondaries().await;
                    self.notice_finished();
                    self.refresh();
                }
                _ = save.tick() => {
                    if let Err(e) = self.save_resume().await {
                        tracing::error!(error = %e, "saving resume state failed");
                    }
                }
                _ = retire.tick() => self.retire_superseded().await,
                () = self.inner.stop.notified() => break,
            }
        }
    }

    /// Save resume state and stop every torrent.
    pub async fn shutdown(&self) -> Result<(), CoreError> {
        self.inner
            .stopping
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.inner.stop.notify_waiters();
        for p in self
            .inner
            .pending
            .write()
            .expect("pending lock poisoned")
            .values_mut()
        {
            p.fetch = None;
        }
        self.save_resume().await?;
        self.flush_reuse().await;
        let handles: Vec<TorrentHandle> = self
            .inner
            .swarms
            .lock()
            .expect("swarms lock poisoned")
            .values()
            .map(|s| s.handle.clone())
            .collect();
        for h in handles {
            h.shutdown().await;
        }
        tracing::info!("core stopped");
        Ok(())
    }
}

/// An entry's own share of its swarm's statistics.
fn entry_stats(stats: &apitorrent_engine::TorrentStats, key: &Key) -> EntryStats {
    let k = key.engine();
    stats
        .entries
        .iter()
        .find(|e| e.key == k)
        .cloned()
        .unwrap_or(EntryStats {
            key: k,
            paused: true,
            downloaded: 0,
            uploaded: 0,
            download_rate: 0,
            upload_rate: 0,
            peers: 0,
            seeds: 0,
        })
}

fn summarize(key: &Key, e: &Entry) -> TorrentSummary {
    let stats = e.handle.stats();
    let mine = entry_stats(&stats, key);
    let info = &e.torrent.info;
    let hashes = info.info_hashes();
    let state = match stats.state {
        EngineState::Checking => TorrentState::Checking,
        EngineState::Downloading => TorrentState::Downloading,
        EngineState::Seeding => TorrentState::Seeding,
        EngineState::Paused => TorrentState::Paused,
        EngineState::Error => TorrentState::Error,
    };
    TorrentSummary {
        id: key.id.clone(),
        space: key.space,
        name: info.name.clone(),
        info_hash_v1: hashes.get_v1().map(|h| h.to_hex()),
        info_hash_v2: hashes.get_v2().map(|h| h.to_hex()),
        state: if e.paused {
            TorrentState::Paused
        } else if e.queued && stats.error.is_none() {
            TorrentState::Queued
        } else {
            state
        },
        progress: if stats.wanted_total == 0 {
            1.0
        } else {
            ratio(stats.wanted_have as usize, stats.wanted_total as usize)
        },
        size: info.content_length(),
        pieces_have: stats.pieces_have,
        pieces_total: stats.pieces_total,
        downloaded: e.downloaded_base + mine.downloaded,
        uploaded: e.uploaded_base + mine.uploaded,
        download_rate: mine.download_rate,
        upload_rate: mine.upload_rate,
        peers: mine.peers,
        seeds: mine.seeds,
        added_at: e.added_at,
        label: e.label.clone(),
        save_path: e.save_path.to_string_lossy().into_owned(),
        private: info.private,
        error: stats.error,
        queue_position: e.queue_position,
        group: e.group.clone(),
        superseded_by: e.superseded_by.clone(),
        in_deluge: e.in_deluge.clone(),
        leftover_from: e.leftover_from.clone(),
    }
}

fn summarize_pending(key: &Key, p: &Pending) -> TorrentSummary {
    let id = key.id.as_str();
    let status = p
        .fetch
        .as_ref()
        .map(|(w, _)| w.borrow().clone())
        .unwrap_or_default();
    let hashes = p.magnet.info_hashes;
    TorrentSummary {
        id: id.to_owned(),
        space: key.space,
        name: p
            .magnet
            .display_name
            .clone()
            .unwrap_or_else(|| id.to_owned()),
        info_hash_v1: hashes.get_v1().map(|h| h.to_hex()),
        info_hash_v2: hashes.get_v2().map(|h| h.to_hex()),
        state: if p.paused {
            TorrentState::Paused
        } else {
            TorrentState::Metadata
        },
        progress: 0.0,
        size: 0,
        pieces_have: 0,
        pieces_total: 0,
        downloaded: 0,
        uploaded: 0,
        download_rate: 0,
        upload_rate: 0,
        peers: status.peers,
        seeds: 0,
        added_at: p.added_at,
        label: p.label.clone(),
        save_path: p.save_path.to_string_lossy().into_owned(),
        private: false,
        error: status.last_error,
        queue_position: p.queue_position,
        group: None,
        superseded_by: None,
        in_deluge: None,
        leftover_from: None,
    }
}

/// File priorities from a magnet link's `so` selection (BEP 53). Its indices count the
/// torrent's real files, skipping pad entries.
fn select_only_priorities(
    torrent: &Torrent,
    select: Option<&[std::ops::RangeInclusive<usize>]>,
) -> Vec<u8> {
    let files = &torrent.info.files;
    let Some(select) = select else {
        return vec![4; files.len()];
    };
    let mut real = 0usize;
    files
        .iter()
        .map(|f| {
            if f.is_pad() {
                return 0;
            }
            let chosen = select.iter().any(|r| r.contains(&real));
            real += 1;
            if chosen { 4 } else { 0 }
        })
        .collect()
}

fn peer_info(p: apitorrent_engine::PeerInfo) -> apitorrent_types::PeerInfo {
    let mut flags = String::new();
    for (on, c) in [
        (p.outbound, 'O'),
        (p.am_choking, 'C'),
        (p.am_interested, 'I'),
        (p.peer_choking, 'c'),
        (p.peer_interested, 'i'),
        (p.on_parole, 'P'),
        (p.utp, 'U'),
        (p.encrypted, 'E'),
    ] {
        if on {
            flags.push(c);
        }
    }
    apitorrent_types::PeerInfo {
        address: p.addr.to_string(),
        client: p.client,
        progress: p.progress,
        download_rate: p.download_rate,
        upload_rate: p.upload_rate,
        downloaded: p.downloaded,
        uploaded: p.uploaded,
        flags,
    }
}

fn engine_limits(l: TorrentLimits) -> apitorrent_engine::limits::TorrentLimits {
    apitorrent_engine::limits::TorrentLimits {
        download_rate: l.download_rate,
        upload_rate: l.upload_rate,
        max_connections: l.max_connections.map(|n| n as usize),
        memory: None,
    }
}

#[allow(clippy::cast_precision_loss)]
fn ratio(a: usize, b: usize) -> f64 {
    a as f64 / b as f64
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn pack_indices(indices: &[usize]) -> Vec<u8> {
    indices
        .iter()
        .flat_map(|&i| u32::try_from(i).unwrap_or(u32::MAX).to_le_bytes())
        .collect()
}

fn unpack_indices(bytes: &[u8]) -> Vec<usize> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c) as usize)
        .collect()
}

fn naming_from_byte(b: u8) -> Naming {
    match b {
        1 => Naming::Native {
            case_insensitive: false,
        },
        2 => Naming::Native {
            case_insensitive: true,
        },
        _ => Naming::Portable,
    }
}

fn policy_of(naming: u8) -> apitorrent_types::NamingPolicy {
    if naming == 0 {
        apitorrent_types::NamingPolicy::Portable
    } else {
        apitorrent_types::NamingPolicy::Native
    }
}

/// What a filesystem allows, found by trying it.
struct FsTraits {
    /// Names Windows forbids (`:` and a trailing `.`) can be created and read back.
    native_names: bool,
    case_insensitive: bool,
}

/// Try the filesystem under `dir`: create a scratch directory, a file with a name only
/// POSIX filesystems accept, and check whether names differing in case collide. A real
/// test is the only check that works for every OS, mount, junction and network share.
fn probe_filesystem(dir: &std::path::Path) -> std::io::Result<FsTraits> {
    std::fs::create_dir_all(dir)?;
    let scratch = dir.join(format!(".apitorrent-probe-{:08x}", rand_u32()));
    std::fs::create_dir(&scratch)?;
    let result = (|| {
        let odd = "probe:name.";
        let native_names = std::fs::write(scratch.join(odd), b"").is_ok()
            && std::fs::read_dir(&scratch)?
                .filter_map(Result::ok)
                .any(|e| e.file_name() == odd);
        std::fs::write(scratch.join("CaseProbe"), b"")?;
        let case_insensitive = scratch.join("CASEPROBE").exists();
        Ok(FsTraits {
            native_names,
            case_insensitive,
        })
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

fn rand_u32() -> u32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    nanos ^ std::process::id()
}

fn placement_to_byte(p: Placement) -> u8 {
    match p {
        Placement::Save => 0,
        Placement::Held => 1,
        Placement::Parts => 2,
    }
}

fn placement_from_byte(b: u8) -> Placement {
    match b {
        1 => Placement::Held,
        2 => Placement::Parts,
        _ => Placement::Save,
    }
}

fn policy_to_byte(p: UnwantedPolicy) -> u8 {
    match p {
        UnwantedPolicy::Hold => 0,
        UnwantedPolicy::Delete => 1,
    }
}

fn policy_from_byte(b: u8) -> UnwantedPolicy {
    if b == 1 {
        UnwantedPolicy::Delete
    } else {
        UnwantedPolicy::Hold
    }
}

fn engine_policy(p: UnwantedPolicy) -> EnginePolicy {
    match p {
        UnwantedPolicy::Hold => EnginePolicy::Hold,
        UnwantedPolicy::Delete => EnginePolicy::Delete,
    }
}

/// Choose a torrent's held area: inside the data directory when that is on the same volume
/// as the save path (holding a file is then a rename), otherwise a hidden directory in the
/// save path. The volume test is a real rename, which is the only check that works the same
/// on every platform and filesystem (mount points, junctions, network shares).
fn held_path_for(data_dir: &std::path::Path, save_path: &std::path::Path, id: &str) -> PathBuf {
    held_dir_for(data_dir, save_path).join(id)
}

/// The folder a torrent saved under `save_path` keeps held files in: under the data
/// directory when that is on the same volume (so a rename moves them), else beside the data.
fn held_dir_for(data_dir: &std::path::Path, save_path: &std::path::Path) -> PathBuf {
    let preferred = data_dir.join("held");
    let fallback = save_path.join(".apitorrent-held");
    let probe = || -> std::io::Result<bool> {
        std::fs::create_dir_all(data_dir.join("held"))?;
        std::fs::create_dir_all(save_path)?;
        let name = format!(
            ".apitorrent-probe-{}-{}",
            std::process::id(),
            PROBE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let from = save_path.join(&name);
        let to = data_dir.join("held").join(&name);
        std::fs::write(&from, b"")?;
        let moved = std::fs::rename(&from, &to).is_ok();
        let _ = std::fs::remove_file(if moved { &to } else { &from });
        Ok(moved)
    };
    match probe() {
        Ok(true) => preferred,
        Ok(false) => fallback,
        Err(e) => {
            tracing::warn!(error = %e, "could not probe the save path's volume");
            fallback
        }
    }
}

/// Makes concurrent probes' file names unique.
static PROBE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Most trackers one entry may list.
const MAX_TRACKERS: usize = 200;

/// Announce tiers as given by a user: trimmed, empty tiers dropped, each URL `http`,
/// `https` or `udp` and listed once.
fn clean_tiers(tiers: Vec<Vec<String>>) -> Result<Vec<Vec<String>>, CoreError> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for tier in tiers {
        let mut urls = Vec::new();
        for raw in tier {
            let url = raw.trim().to_owned();
            if url.is_empty() {
                continue;
            }
            let ok = url.len() <= 2048
                && url::Url::parse(&url).is_ok_and(|u| {
                    matches!(u.scheme(), "http" | "https" | "udp") && u.host().is_some()
                });
            if !ok {
                return Err(CoreError::InvalidRequest(format!(
                    "{url:?} is not an http, https or udp tracker URL"
                )));
            }
            if !seen.insert(url.clone()) {
                return Err(CoreError::InvalidRequest(format!("{url} is listed twice")));
            }
            urls.push(url);
        }
        if !urls.is_empty() {
            out.push(urls);
        }
    }
    if seen.len() > MAX_TRACKERS {
        return Err(CoreError::InvalidRequest(format!(
            "at most {MAX_TRACKERS} trackers"
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tracker_list_tests {
    use super::*;

    fn tiers(v: &[&[&str]]) -> Vec<Vec<String>> {
        v.iter()
            .map(|t| t.iter().map(|&u| u.to_owned()).collect())
            .collect()
    }

    #[test]
    fn tracker_lists_are_cleaned() {
        assert_eq!(
            clean_tiers(tiers(&[
                &[" http://a/announce ", ""],
                &[],
                &["udp://b:80", "https://c/x"]
            ]))
            .unwrap(),
            tiers(&[&["http://a/announce"], &["udp://b:80", "https://c/x"]])
        );
        assert!(clean_tiers(Vec::new()).unwrap().is_empty());
        for bad in [
            tiers(&[&["http://a"], &["http://a"]]),
            tiers(&[&["ftp://a"]]),
            tiers(&[&["not a url"]]),
            tiers(&[&["http://"]]),
        ] {
            assert!(clean_tiers(bad.clone()).is_err(), "{bad:?}");
        }
    }
}

//! apitorrent's core: the torrent registry above the engine.
//!
//! The core owns the instance database, adds and restores torrents in the engine, keeps the
//! revisioned store that clients sync from, and saves resume state. The API server is a thin
//! layer over [`Core`].

pub mod db;
pub mod import;
pub mod library;
pub mod settings;
pub mod store;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use tokio::sync::{Notify, broadcast, oneshot, watch};

use apitorrent_engine::bitfield::Bitfield;
use apitorrent_engine::layout::Layout;
use apitorrent_engine::metadata::FetchStatus;
use apitorrent_engine::storage::Placement;
use apitorrent_engine::{
    AddTorrent, Session, SessionConfig, TorrentHandle, TorrentState as EngineState,
    UnwantedPolicy as EnginePolicy,
};
use apitorrent_metainfo::sanitize::Naming;
use apitorrent_metainfo::{Magnet, Torrent};
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
/// How often superseded torrents are checked for retirement.
const RETIRE_EVERY: Duration = Duration::from_secs(60);

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
            CoreError::Database(_) | CoreError::Io(_) => "internal",
        }
    }
}

struct Entry {
    torrent: Arc<Torrent>,
    handle: TorrentHandle,
    save_path: PathBuf,
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
    group: Option<String>,
    naming: u8,
    superseded_by: Option<TorrentId>,
    /// Per-file paths overriding the naming policy.
    file_paths: Option<Vec<Vec<String>>>,
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

/// Everything needed to start a torrent in the engine and track it.
struct Spawn {
    id: TorrentId,
    /// Peers to try straight away (found while fetching a magnet's metadata).
    peers: Vec<SocketAddr>,
    torrent: Arc<Torrent>,
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
    torrents: RwLock<HashMap<TorrentId, Entry>>,
    pending: RwLock<HashMap<TorrentId, Pending>>,
    store: Store,
    stop: Notify,
    settings: RwLock<Settings>,
}

const SETTINGS_KEY: &str = "settings";

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
        let stored = db.meta(SETTINGS_KEY)?;
        let settings = match stored.as_deref().map(serde_json::from_str::<Settings>) {
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                tracing::warn!(error = %e, "stored settings don't parse; using defaults");
                settings::defaults(&config.session)
            }
            None => settings::defaults(&config.session),
        };
        let session = Session::start(config.session.clone()).await?;
        tracing::info!(peer_port = session.listen_addr().port(), "engine started");
        let core = Core {
            inner: Arc::new(Inner {
                config,
                db: Arc::new(Mutex::new(db)),
                session,
                torrents: RwLock::new(HashMap::new()),
                pending: RwLock::new(HashMap::new()),
                store: Store::new(),
                stop: Notify::new(),
                settings: RwLock::new(settings),
            }),
        };
        core.apply_session_settings();
        core.restore().await?;
        core.apply_queue().await;
        tokio::spawn(core.clone().background());
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

    /// Push the settings' rates, connection limits and groups into the engine.
    fn apply_session_settings(&self) {
        let s = self.settings();
        let session = &self.inner.session;
        session.set_rate_limits(s.download_rate, s.upload_rate);
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
    }

    /// Put every torrent in the group its label selects.
    async fn regroup(&self) {
        let s = self.settings();
        let changes: Vec<(TorrentHandle, Option<String>)> = {
            let mut torrents = self.inner.torrents.write().expect("torrents lock poisoned");
            torrents
                .values_mut()
                .filter_map(|e| {
                    let group = settings::group_for(&s, e.label.as_deref()).map(str::to_owned);
                    (group != e.group).then(|| {
                        e.group.clone_from(&group);
                        (e.handle.clone(), group)
                    })
                })
                .collect()
        };
        for (handle, group) in changes {
            handle.set_group(group).await;
        }
    }

    /// Let the queue decide which torrents run: pause the ones over the limits, resume
    /// the ones that now fit. Torrents paused by the user or in error don't take part.
    async fn apply_queue(&self) {
        let q = self.settings().queue;
        let mut actions = Vec::new();
        {
            let mut torrents = self.inner.torrents.write().expect("torrents lock poisoned");
            let items: Vec<settings::QueueItem> = torrents
                .iter()
                .filter(|(_, e)| !e.paused && e.handle.stats().error.is_none())
                .map(|(id, e)| settings::QueueItem {
                    id: id.clone(),
                    position: e.queue_position,
                    finished: e.handle.stats().finished,
                })
                .collect();
            let active = settings::plan(&items, &q);
            for item in &items {
                let e = torrents.get_mut(&item.id).expect("listed above");
                let run = active.contains(&item.id);
                if run && e.queued {
                    e.queued = false;
                    actions.push((e.handle.clone(), true));
                } else if !run && !e.queued {
                    e.queued = true;
                    actions.push((e.handle.clone(), false));
                }
            }
        }
        for (handle, run) in actions {
            if run {
                handle.resume().await;
            } else {
                handle.pause().await;
            }
        }
    }

    /// Change a torrent's own rate and connection limits.
    pub async fn set_torrent_limits(
        &self,
        id: &str,
        limits: TorrentLimits,
    ) -> Result<(), CoreError> {
        let handle = self.handle(id)?;
        let key = id.to_owned();
        self.db(move |db| {
            db.set_limits(
                &key,
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
            .get_mut(id)
        {
            e.limits = limits;
        }
        handle.set_limits(engine_limits(limits)).await;
        Ok(())
    }

    /// Change a torrent's label, which also picks its group.
    pub async fn set_label(&self, id: &str, label: Option<String>) -> Result<(), CoreError> {
        let label = label.map(|l| l.trim().to_owned()).filter(|l| !l.is_empty());
        if let Some(p) = self
            .inner
            .pending
            .write()
            .expect("pending lock poisoned")
            .get_mut(id)
        {
            p.label.clone_from(&label);
        } else {
            self.handle(id)?;
        }
        let key = id.to_owned();
        let stored = label.clone();
        self.db(move |db| db.set_label(&key, stored.as_deref()))
            .await?;
        if let Some(e) = self
            .inner
            .torrents
            .write()
            .expect("torrents lock poisoned")
            .get_mut(id)
        {
            e.label = label;
        }
        self.regroup().await;
        self.refresh();
        Ok(())
    }

    /// Move a torrent in the queue. Positions are renumbered from 0.
    pub async fn queue_move(&self, id: &str, to: QueueMove) -> Result<(), CoreError> {
        let positions = {
            let mut torrents = self.inner.torrents.write().expect("torrents lock poisoned");
            let mut pending = self.inner.pending.write().expect("pending lock poisoned");
            if !torrents.contains_key(id) && !pending.contains_key(id) {
                return Err(CoreError::NotFound);
            }
            let mut order: Vec<(u32, String)> = torrents
                .iter()
                .map(|(k, e)| (e.queue_position, k.clone()))
                .chain(pending.iter().map(|(k, p)| (p.queue_position, k.clone())))
                .collect();
            order.sort();
            let mut ids: Vec<String> = order.into_iter().map(|(_, k)| k).collect();
            let from = ids.iter().position(|k| k == id).expect("present");
            let item = ids.remove(from);
            let dest = match to {
                QueueMove::Top => 0,
                QueueMove::Up => from.saturating_sub(1),
                QueueMove::Down => (from + 1).min(ids.len()),
                QueueMove::Bottom => ids.len(),
            };
            ids.insert(dest, item);
            let positions: Vec<(String, u32)> = ids
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
    /// linked file indices, and records torrents the new one fully takes over.
    async fn link_from_library(
        &self,
        id: &str,
        torrent: &Arc<Torrent>,
        save_path: &std::path::Path,
        naming: u8,
        priorities: &[u8],
    ) -> Vec<usize> {
        let lib = self.settings().library;
        if !lib.enabled {
            return Vec::new();
        }
        // A snapshot of every torrent's files: where they are and what's complete.
        let entries: Vec<LibrarySnapshot> = self
            .inner
            .torrents
            .read()
            .expect("torrents lock poisoned")
            .iter()
            .filter(|(k, _)| k.as_str() != id)
            .map(|(k, e)| {
                (
                    k.clone(),
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
            let paths = custom
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
            sources.push(library::Source {
                id: sid,
                torrent: t,
                paths,
                have,
            });
        }
        if sources.is_empty() {
            return Vec::new();
        }
        let targets = Self::disk_paths(torrent, save_path, naming);
        let (new, prios) = (torrent.clone(), priorities.to_vec());
        let (linked, superseded) = tokio::task::spawn_blocking(move || {
            let matches = library::find_matches(&new, &targets, &prios, &sources);
            let mut done = Vec::new();
            for m in &matches {
                match library::materialize(&m.from, &targets[m.file], lib.allow_copy) {
                    Some(how) => {
                        tracing::info!(
                            file = %targets[m.file].display(),
                            from = %m.source_id,
                            ?how,
                            "reused a file from the library"
                        );
                        done.push(m.clone());
                    }
                    None => {
                        tracing::debug!(file = %targets[m.file].display(), "could not link or copy");
                    }
                }
            }
            let superseded = library::superseded(&done, &sources);
            (
                done.into_iter().map(|m| m.file).collect::<Vec<_>>(),
                superseded,
            )
        })
        .await
        .unwrap_or_default();
        for s in superseded {
            let (key, by) = (s.clone(), id.to_owned());
            if self
                .db(move |db| db.set_superseded(&key, &by))
                .await
                .is_ok()
                && let Some(e) = self
                    .inner
                    .torrents
                    .write()
                    .expect("torrents lock poisoned")
                    .get_mut(&s)
            {
                tracing::info!(torrent = %s, by = %id, "torrent superseded by a pack");
                e.superseded_by = Some(id.to_owned());
            }
        }
        linked
    }

    /// Bring in a Deluge installation's torrents: files where Deluge left them, progress from
    /// its resume data (validated against the files on disk, so nothing is rehashed),
    /// labels, queue order, per-torrent limits and all-time totals.
    pub async fn import_deluge(
        &self,
        req: apitorrent_types::ImportDelugeRequest,
    ) -> Result<apitorrent_types::ImportReport, CoreError> {
        use apitorrent_types::{ImportReport, ImportSkip};
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
        let mut report = ImportReport {
            warnings: state.warnings.clone(),
            ..ImportReport::default()
        };
        if req.import_settings {
            let d = &state.settings;
            let mut s = self.settings();
            s.download_rate = d.max_download_rate.unwrap_or(0);
            s.upload_rate = d.max_upload_rate.unwrap_or(0);
            if let Some(n) = d.max_connections {
                s.max_connections = n;
            }
            s.queue.max_active_downloads = d.max_active_downloading.unwrap_or(0);
            s.queue.max_active_seeds = d.max_active_seeding.unwrap_or(0);
            s.queue.max_active = d.max_active_limit.unwrap_or(0);
            self.set_settings(s).await?;
        }
        let ours = self.inner.config.download_dir.clone();
        for t in state.torrents {
            let label = || t.id.clone();
            let skip = |report: &mut ImportReport, name: String, reason: String| {
                report.skipped.push(ImportSkip { name, reason });
            };
            let bytes = match &t.trackers {
                Some(tiers) => {
                    import::with_trackers(&t.torrent, tiers).unwrap_or_else(|| t.torrent.clone())
                }
                None => t.torrent.clone(),
            };
            let torrent = match Torrent::from_bytes(&bytes) {
                Ok(parsed) => Arc::new(parsed),
                Err(e) => {
                    // A magnet that never got its metadata in Deluge: add it as a magnet.
                    if let Some(uri) = &t.magnet {
                        match self
                            .add(AddTorrentRequest {
                                source: AddSource::Magnet { uri: uri.clone() },
                                save_path: t.save_path.as_deref().map(|p| import::remap(p, &map)),
                                paused: req.paused || t.paused,
                                label: t.label.clone(),
                                file_priorities: None,
                                unwanted_policy: UnwantedPolicy::Hold,
                            })
                            .await
                        {
                            Ok(_) => report.imported.push(label()),
                            Err(e) => skip(&mut report, label(), e.to_string()),
                        }
                    } else {
                        skip(&mut report, label(), format!("unreadable .torrent: {e}"));
                    }
                    continue;
                }
            };
            let name = torrent.info.name.clone();
            let id = torrent.info.info_hashes().id_string();
            let taken = self
                .inner
                .torrents
                .read()
                .expect("torrents lock poisoned")
                .contains_key(&id);
            if taken {
                skip(&mut report, name, "already in apitorrent".into());
                continue;
            }
            let save_path = import::save_path(
                t.save_path.as_deref(),
                state.settings.download_location.as_deref(),
                &ours,
                &map,
            );
            let files = torrent.info.files.len();
            let prios = t
                .file_priorities
                .clone()
                .filter(|p| p.len() == files)
                .unwrap_or_else(|| vec![4; files]);
            let pieces = u32::try_from(torrent.info.piece_count()).unwrap_or(0);
            let have = t
                .pieces
                .as_deref()
                .and_then(|p| import::pieces_bitfield(p, pieces));
            let file_paths = {
                let (tor, save, renamed) = (torrent.clone(), save_path.clone(), t.renamed.clone());
                tokio::task::spawn_blocking(move || import::file_layout(&tor, &save, &renamed))
                    .await
                    .unwrap_or(None)
            };
            let held_path = held_path_for(&self.inner.config.data_dir, &save_path, &id);
            let limits = TorrentLimits {
                download_rate: t.max_download_rate.unwrap_or(0),
                upload_rate: t.max_upload_rate.unwrap_or(0),
                max_connections: t.max_connections,
            };
            let paused = req.paused || t.paused;
            let hashes = torrent.info.info_hashes();
            let row = TorrentRow {
                id: id.clone(),
                info_hash_v1: hashes.get_v1().map(|h| h.to_hex()),
                info_hash_v2: hashes.get_v2().map(|h| h.to_hex()),
                name: name.clone(),
                torrent: bytes,
                save_path: save_path.to_string_lossy().into_owned(),
                label: t.label.clone(),
                added_at: t.added_at.map_or_else(now_ms, |s| s.saturating_mul(1000)),
                paused,
                have: have.as_ref().map(|h| h.as_bytes().to_vec()),
                file_priorities: Some(prios.clone()),
                downloaded_total: t.total_downloaded,
                uploaded_total: t.total_uploaded,
                placements: None,
                held_path: Some(held_path.to_string_lossy().into_owned()),
                unwanted_policy: 0,
                queue_position: self.next_queue_position(),
                download_rate: limits.download_rate,
                upload_rate: limits.upload_rate,
                max_connections: limits.max_connections,
                magnet: None,
                naming: 0,
                linked: None,
                file_paths: file_paths
                    .as_ref()
                    .map(|p| serde_json::to_string(p).expect("serialize")),
            };
            let (added_at, queue_position) = (row.added_at, row.queue_position);
            if let Err(e) = self.db(move |db| db.insert_torrent(&row)).await {
                skip(&mut report, name, e.to_string());
                continue;
            }
            let spawned = self.spawn_entry(Spawn {
                id: id.clone(),
                peers: Vec::new(),
                torrent,
                save_path,
                held_path,
                have,
                placements: None,
                paused,
                file_priorities: prios,
                unwanted: UnwantedPolicy::Hold,
                label: t.label.clone(),
                added_at,
                totals: (t.total_downloaded, t.total_uploaded),
                queue_position,
                limits,
                queued: !paused && self.queue_limited(),
                naming: 0,
                linked: Vec::new(),
                file_paths,
            });
            match spawned {
                Ok(_) => report.imported.push(name),
                Err(e) => {
                    let key = id.clone();
                    let _ = self.db(move |db| db.remove_torrent(&key)).await;
                    skip(&mut report, name, e.to_string());
                }
            }
        }
        tracing::info!(
            imported = report.imported.len(),
            skipped = report.skipped.len(),
            "Deluge import finished"
        );
        self.apply_queue().await;
        self.refresh();
        Ok(report)
    }

    /// Remove superseded torrents whose tracker now says they're unregistered. Their files
    /// stay: the torrent that superseded them holds the same data.
    pub async fn retire_superseded(&self) {
        if !self.settings().library.retire_superseded {
            return;
        }
        let candidates: Vec<(String, TorrentHandle)> = self
            .inner
            .torrents
            .read()
            .expect("torrents lock poisoned")
            .iter()
            .filter(|(_, e)| e.superseded_by.is_some())
            .map(|(k, e)| (k.clone(), e.handle.clone()))
            .collect();
        for (id, handle) in candidates {
            let gone = handle
                .trackers()
                .await
                .iter()
                .filter_map(|t| t.error.as_deref())
                .any(library::is_unregistered);
            if gone {
                tracing::info!(%id, "retiring a superseded torrent its tracker no longer knows");
                if let Err(e) = self.remove(&id, false).await {
                    tracing::warn!(%id, error = %e, "could not retire");
                }
            }
        }
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

    fn next_queue_position(&self) -> u32 {
        let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
        let pending = self.inner.pending.read().expect("pending lock poisoned");
        torrents
            .values()
            .map(|e| e.queue_position + 1)
            .chain(pending.values().map(|p| p.queue_position + 1))
            .max()
            .unwrap_or(0)
    }

    /// Whether any queue limit is set, so new and restored torrents should wait for the
    /// queue's decision rather than start at once.
    fn queue_limited(&self) -> bool {
        self.settings().queue != apitorrent_types::QueueSettings::default()
    }

    pub fn store(&self) -> &Store {
        &self.inner.store
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

    pub fn subscribe(&self) -> (Snapshot, broadcast::Receiver<Arc<Patch>>) {
        self.inner.store.subscribe()
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
        let rows = self.db(|db| db.torrents()).await?;
        let count = rows.len();
        for row in rows {
            if row.torrent.is_empty() {
                self.restore_pending(row);
                continue;
            }
            let torrent = match Torrent::from_bytes(&row.torrent) {
                Ok(t) => Arc::new(t),
                Err(e) => {
                    tracing::error!(id = %row.id, error = %e, "stored torrent no longer parses");
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
            self.spawn_entry(Spawn {
                id: row.id.clone(),
                peers: Vec::new(),
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
            })?;
        }
        let superseded = self.db(|db| db.superseded()).await?;
        {
            let mut torrents = self.inner.torrents.write().expect("torrents lock poisoned");
            for (s, by) in superseded {
                if let Some(e) = torrents.get_mut(&s) {
                    e.superseded_by = Some(by);
                }
            }
        }
        tracing::info!(count, "torrents restored");
        self.refresh();
        Ok(())
    }

    /// Bring back a magnet link that was still waiting for metadata.
    fn restore_pending(&self, row: TorrentRow) {
        let Some(uri) = row.magnet else {
            tracing::error!(id = %row.id, "stored torrent has neither metadata nor a magnet link");
            return;
        };
        let magnet = match Magnet::parse(&uri) {
            Ok(m) => m,
            Err(e) => {
                tracing::error!(id = %row.id, error = %e, "stored magnet link no longer parses");
                return;
            }
        };
        let save_path = PathBuf::from(&row.save_path);
        let held_path = row.held_path.as_ref().map_or_else(
            || held_path_for(&self.inner.config.data_dir, &save_path, &row.id),
            PathBuf::from,
        );
        let id = row.id.clone();
        self.inner
            .pending
            .write()
            .expect("pending lock poisoned")
            .insert(
                row.id,
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
            self.start_fetch(&id);
        }
    }

    /// Start (or restart) the metadata fetch for a pending magnet link. When it finishes,
    /// the torrent starts for real.
    fn start_fetch(&self, id: &str) {
        let mut pending = self.inner.pending.write().expect("pending lock poisoned");
        let Some(p) = pending.get_mut(id) else {
            return;
        };
        let mut fetch = self.inner.session.fetch_metadata(&p.magnet);
        let (cancel_tx, cancel_rx) = oneshot::channel();
        p.fetch = Some((fetch.watch(), cancel_tx));
        let core = self.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            tokio::select! {
                fetched = fetch.wait() => {
                    if let Some(fetched) = fetched
                        && let Err(e) = core.metadata_arrived(&id, fetched).await
                    {
                        tracing::error!(%id, error = %e, "could not start the torrent from its metadata");
                    }
                }
                _ = cancel_rx => {}
            }
        });
    }

    /// A magnet link's metadata is here: store it as a `.torrent` and start the torrent.
    async fn metadata_arrived(
        &self,
        id: &str,
        fetched: apitorrent_engine::metadata::Fetched,
    ) -> Result<(), CoreError> {
        let Some(p) = self
            .inner
            .pending
            .write()
            .expect("pending lock poisoned")
            .remove(id)
        else {
            return Ok(()); // removed meanwhile
        };
        let bytes = apitorrent_engine::metadata::torrent_file(
            &fetched.info,
            &p.magnet.trackers,
            &p.magnet.web_seeds,
        );
        let torrent = Arc::new(
            Torrent::from_bytes(&bytes).map_err(|e| CoreError::InvalidTorrent(e.to_string()))?,
        );
        let prios = select_only_priorities(&torrent, p.magnet.select_only.as_deref());
        let hashes = torrent.info.info_hashes();
        let (key, name, stored) = (id.to_owned(), torrent.info.name.clone(), prios.clone());
        let (v1, v2) = (
            hashes.get_v1().map(|h| h.to_hex()),
            hashes.get_v2().map(|h| h.to_hex()),
        );
        self.db(move |db| {
            db.set_metadata(&key, &bytes, &name, v1.as_deref(), v2.as_deref(), &stored)
        })
        .await?;
        tracing::info!(%id, name = %torrent.info.name, "metadata fetched");
        let linked = self
            .link_from_library(id, &torrent, &p.save_path, p.naming, &prios)
            .await;
        self.spawn_entry(Spawn {
            id: id.to_owned(),
            peers: fetched.peers,
            torrent,
            save_path: p.save_path,
            held_path: p.held_path,
            have: None,
            placements: None,
            paused: p.paused,
            file_priorities: prios,
            unwanted: p.unwanted,
            label: p.label,
            added_at: p.added_at,
            totals: (0, 0),
            queue_position: p.queue_position,
            limits: TorrentLimits::default(),
            queued: !p.paused && self.queue_limited(),
            naming: p.naming,
            linked,
            file_paths: None,
        })?;
        self.apply_queue().await;
        self.refresh();
        Ok(())
    }

    /// Add a magnet link: store it, and fetch its metadata in the background.
    async fn add_magnet(&self, uri: &str, req: AddTorrentRequest) -> Result<TorrentId, CoreError> {
        let magnet = Magnet::parse(uri).map_err(|e| CoreError::InvalidRequest(e.to_string()))?;
        let id = magnet.info_hashes.id_string();
        let taken = self
            .inner
            .torrents
            .read()
            .expect("torrents lock poisoned")
            .contains_key(&id)
            || self
                .inner
                .pending
                .read()
                .expect("pending lock poisoned")
                .contains_key(&id);
        if taken {
            return Err(CoreError::Duplicate);
        }
        let save_path = req
            .save_path
            .map_or_else(|| self.inner.config.download_dir.clone(), PathBuf::from);
        if !save_path.is_absolute() {
            return Err(CoreError::InvalidRequest(
                "save path must be absolute".into(),
            ));
        }
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
            queue_position: self.next_queue_position(),
            download_rate: 0,
            upload_rate: 0,
            max_connections: None,
            magnet: Some(uri.to_owned()),
            naming,
            linked: None,
            file_paths: None,
        };
        let (added_at, queue_position) = (row.added_at, row.queue_position);
        self.db(move |db| db.insert_torrent(&row)).await?;
        self.inner
            .pending
            .write()
            .expect("pending lock poisoned")
            .insert(
                id.clone(),
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
            self.start_fetch(&id);
        }
        tracing::info!(%id, "magnet link added");
        self.refresh();
        Ok(id)
    }

    fn spawn_entry(&self, s: Spawn) -> Result<TorrentId, CoreError> {
        let Spawn {
            id,
            peers,
            torrent,
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
                    tracing::warn!(%id, error = %e, "native paths failed; using portable names");
                    None
                }
            },
        };
        let group = settings::group_for(&self.settings(), label.as_deref()).map(str::to_owned);
        let parts_path = self
            .inner
            .config
            .data_dir
            .join("parts")
            .join(format!("{id}.parts"));
        let handle = self
            .inner
            .session
            .add_torrent(AddTorrent {
                held_path: Some(held_path),
                parts_path: Some(parts_path),
                have,
                placements,
                peers,
                unwanted: engine_policy(unwanted),
                paused: paused || queued,
                file_priorities: Some(file_priorities.clone()),
                limits: engine_limits(limits),
                group: group.clone(),
                file_paths,
                linked,
                ..AddTorrent::new(torrent.clone(), save_path.clone())
            })
            .map_err(|_| CoreError::Duplicate)?;
        self.inner
            .torrents
            .write()
            .expect("torrents lock poisoned")
            .insert(
                id.clone(),
                Entry {
                    torrent,
                    handle,
                    save_path,
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
                    group,
                    naming,
                    superseded_by: None,
                    file_paths: custom_paths,
                },
            );
        Ok(id)
    }

    /// Add a torrent and start it (unless the request asks for it paused).
    pub async fn add(&self, req: AddTorrentRequest) -> Result<TorrentId, CoreError> {
        let bytes = match &req.source {
            AddSource::TorrentFile { data } => base64::engine::general_purpose::STANDARD
                .decode(data.trim())
                .map_err(|e| CoreError::InvalidRequest(format!("bad base64: {e}")))?,
            AddSource::Magnet { uri } => {
                let uri = uri.clone();
                return self.add_magnet(&uri, req).await;
            }
        };
        let torrent = Arc::new(
            Torrent::from_bytes(&bytes).map_err(|e| CoreError::InvalidTorrent(e.to_string()))?,
        );
        let id = torrent.info.info_hashes().id_string();
        let taken = self
            .inner
            .torrents
            .read()
            .expect("torrents lock poisoned")
            .contains_key(&id)
            || self
                .inner
                .pending
                .read()
                .expect("pending lock poisoned")
                .contains_key(&id);
        if taken {
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
        let save_path = req
            .save_path
            .map_or_else(|| self.inner.config.download_dir.clone(), PathBuf::from);
        if !save_path.is_absolute() {
            return Err(CoreError::InvalidRequest(
                "save path must be absolute".into(),
            ));
        }
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
            queue_position: self.next_queue_position(),
            download_rate: 0,
            upload_rate: 0,
            max_connections: None,
            magnet: None,
            naming: self.choose_naming(&save_path).await,
            linked: None,
            file_paths: None,
        };
        let queue_position = row.queue_position;
        let naming = row.naming;
        let added_at = row.added_at;
        self.db(move |db| db.insert_torrent(&row)).await?;
        let linked = self
            .link_from_library(&id, &torrent, &save_path, naming, &prios)
            .await;
        if let Err(e) = self.spawn_entry(Spawn {
            id: id.clone(),
            peers: Vec::new(),
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
        }) {
            let id2 = id.clone();
            self.db(move |db| db.remove_torrent(&id2)).await?;
            return Err(e);
        }
        tracing::info!(%id, "torrent added");
        self.apply_queue().await;
        self.refresh();
        Ok(id)
    }

    fn handle(&self, id: &str) -> Result<TorrentHandle, CoreError> {
        self.inner
            .torrents
            .read()
            .expect("torrents lock poisoned")
            .get(id)
            .map(|e| e.handle.clone())
            .ok_or(CoreError::NotFound)
    }

    pub async fn pause(&self, id: &str) -> Result<(), CoreError> {
        self.set_paused(id, true).await
    }

    pub async fn resume(&self, id: &str) -> Result<(), CoreError> {
        self.set_paused(id, false).await
    }

    async fn set_paused(&self, id: &str, paused: bool) -> Result<(), CoreError> {
        let is_pending = {
            let mut pending = self.inner.pending.write().expect("pending lock poisoned");
            match pending.get_mut(id) {
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
            let key = id.to_owned();
            self.db(move |db| db.set_paused(&key, paused)).await?;
            if !paused {
                self.start_fetch(id);
            }
            self.refresh();
            return Ok(());
        }
        let handle = self.handle(id)?;
        let key = id.to_owned();
        self.db(move |db| db.set_paused(&key, paused)).await?;
        if let Some(e) = self
            .inner
            .torrents
            .write()
            .expect("torrents lock poisoned")
            .get_mut(id)
        {
            e.paused = paused;
            e.queued = false;
        }
        if paused {
            handle.pause().await;
        } else {
            handle.resume().await;
        }
        self.apply_queue().await;
        self.refresh();
        Ok(())
    }

    /// Change the priority of some files; the others keep theirs. Pad entries can't be
    /// changed (they are never downloaded).
    pub async fn set_file_priorities(
        &self,
        id: &str,
        updates: &[FilePriority],
    ) -> Result<(), CoreError> {
        let handle = self.handle(id)?;
        let prios = {
            let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
            let entry = torrents.get(id).ok_or(CoreError::NotFound)?;
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
        let key = id.to_owned();
        let stored = prios.clone();
        self.db(move |db| db.set_file_priorities(&key, &stored))
            .await?;
        if let Some(e) = self
            .inner
            .torrents
            .write()
            .expect("torrents lock poisoned")
            .get_mut(id)
        {
            e.file_priorities.clone_from(&prios);
        }
        handle.set_file_priorities(prios).await;
        Ok(())
    }

    /// Choose what happens to files of this torrent that are deselected from now on.
    pub async fn set_unwanted_policy(
        &self,
        id: &str,
        policy: UnwantedPolicy,
    ) -> Result<(), CoreError> {
        let handle = self.handle(id)?;
        let key = id.to_owned();
        self.db(move |db| db.set_unwanted_policy(&key, policy_to_byte(policy)))
            .await?;
        if let Some(e) = self
            .inner
            .torrents
            .write()
            .expect("torrents lock poisoned")
            .get_mut(id)
        {
            e.unwanted = policy;
        }
        handle.set_unwanted_policy(engine_policy(policy)).await;
        Ok(())
    }

    /// Delete held copies of deselected files (all of them when `files` is `None`).
    pub async fn release_held(&self, id: &str, files: Option<Vec<u32>>) -> Result<(), CoreError> {
        let handle = self.handle(id)?;
        let files = files.map(|f| f.into_iter().map(|i| i as usize).collect());
        handle.release_held(files).await;
        Ok(())
    }

    /// Remove a torrent, and optionally its downloaded files. Its held copies and part
    /// store are always deleted: they only exist for the torrent's sake.
    pub async fn remove(&self, id: &str, delete_files: bool) -> Result<(), CoreError> {
        let pending = self
            .inner
            .pending
            .write()
            .expect("pending lock poisoned")
            .remove(id);
        if pending.is_some() {
            let key = id.to_owned();
            self.db(move |db| db.remove_torrent(&key)).await?;
            self.inner.store.apply(Vec::new(), vec![id.to_owned()]);
            tracing::info!(%id, "magnet link removed");
            return Ok(());
        }
        let entry = self
            .inner
            .torrents
            .write()
            .expect("torrents lock poisoned")
            .remove(id)
            .ok_or(CoreError::NotFound)?;
        entry.handle.shutdown().await;
        let key = id.to_owned();
        self.db(move |db| db.remove_torrent(&key)).await?;
        if let Err(e) = entry.handle.delete_data(delete_files).await {
            tracing::warn!(%id, error = %e, "could not delete all of the torrent's data");
        }
        self.inner.store.apply(Vec::new(), vec![id.to_owned()]);
        tracing::info!(%id, delete_files, "torrent removed");
        Ok(())
    }

    pub fn list(&self) -> Vec<TorrentSummary> {
        self.inner.store.list()
    }

    pub async fn detail(&self, id: &str) -> Result<TorrentDetail, CoreError> {
        let pending = self
            .inner
            .pending
            .read()
            .expect("pending lock poisoned")
            .get(id)
            .map(|p| {
                (
                    p.uri.clone(),
                    p.magnet.trackers.clone(),
                    p.unwanted,
                    p.naming,
                )
            });
        if let Some((uri, trackers, unwanted_policy, naming)) = pending {
            let summary = self.inner.store.get(id).ok_or(CoreError::NotFound)?;
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
            });
        }
        let (torrent, handle, prios, unwanted_policy, limits, naming) = {
            let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
            let e = torrents.get(id).ok_or(CoreError::NotFound)?;
            (
                e.torrent.clone(),
                e.handle.clone(),
                e.file_priorities.clone(),
                e.unwanted,
                e.limits,
                e.naming,
            )
        };
        let placements = {
            let h = handle.clone();
            tokio::task::spawn_blocking(move || h.placements())
                .await
                .expect("placements task panicked")
        };
        let summary = self.inner.store.get(id).ok_or(CoreError::NotFound)?;
        let have = handle
            .have()
            .await
            .unwrap_or_else(|| Bitfield::new(summary.pieces_total));
        let layout = Layout::from_info(&torrent.info);
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
                TorrentFile {
                    index: u32::try_from(i).expect("fits"),
                    path: f.display_path(),
                    length: f.length,
                    priority: prios.get(i).copied().unwrap_or(4),
                    progress: if total == 0 { 1.0 } else { ratio(done, total) },
                    placement: match placements.get(i) {
                        Some(Placement::Held) => FilePlacement::Held,
                        Some(Placement::Parts) => FilePlacement::Parts,
                        _ => FilePlacement::Save,
                    },
                }
            })
            .collect();
        let mut magnet = Magnet::new(torrent.info.info_hashes());
        magnet.display_name = Some(torrent.info.name.clone());
        magnet.trackers = torrent.trackers.iter().flatten().cloned().collect();
        Ok(TorrentDetail {
            summary,
            files,
            trackers: torrent.trackers.clone(),
            comment: torrent.comment.clone(),
            created_by: torrent.created_by.clone(),
            creation_date: torrent.creation_date,
            piece_length: torrent.info.piece_length,
            magnet: magnet.to_uri(),
            unwanted_policy,
            limits,
            naming: policy_of(naming),
            peers: handle.peers().await.into_iter().map(peer_info).collect(),
            tracker_status: handle
                .trackers()
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

    /// Session-wide numbers for a status bar.
    pub fn session_stats(&self) -> apitorrent_types::SessionStats {
        let s = &self.inner.session;
        apitorrent_types::SessionStats {
            connections: u32::try_from(s.connections()).unwrap_or(u32::MAX),
            memory_used: s.memory_used(),
            memory_limit: s.memory_limit(),
            banned: u32::try_from(s.bans().len()).unwrap_or(u32::MAX),
            peer_port: s.listen_addr().port(),
        }
    }

    /// Rebuild every summary from the engine and publish the changes.
    fn refresh(&self) {
        let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
        let mut summaries: Vec<TorrentSummary> =
            torrents.iter().map(|(id, e)| summarize(id, e)).collect();
        drop(torrents);
        let pending = self.inner.pending.read().expect("pending lock poisoned");
        summaries.extend(pending.iter().map(|(id, p)| summarize_pending(id, p)));
        drop(pending);
        self.inner.store.apply(summaries, Vec::new());
    }

    async fn save_resume(&self) -> Result<(), CoreError> {
        let entries: Vec<(TorrentId, TorrentHandle, u64, u64)> = self
            .inner
            .torrents
            .read()
            .expect("torrents lock poisoned")
            .iter()
            .map(|(id, e)| {
                (
                    id.clone(),
                    e.handle.clone(),
                    e.downloaded_base,
                    e.uploaded_base,
                )
            })
            .collect();
        let mut rows = Vec::with_capacity(entries.len());
        for (id, handle, down, up) in entries {
            if let Some(have) = handle.have().await {
                let stats = handle.stats();
                let h = handle.clone();
                let placements = tokio::task::spawn_blocking(move || h.placements())
                    .await
                    .expect("placements task panicked");
                let linked = pack_indices(&handle.linked());
                rows.push(ResumeRow {
                    id,
                    have: have.as_bytes().to_vec(),
                    placements: placements.into_iter().map(placement_to_byte).collect(),
                    linked,
                    downloaded_total: down + stats.downloaded,
                    uploaded_total: up + stats.uploaded,
                });
            }
        }
        self.db(move |db| db.save_resume(&rows)).await
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
        let handles: Vec<TorrentHandle> = self
            .inner
            .torrents
            .read()
            .expect("torrents lock poisoned")
            .values()
            .map(|e| e.handle.clone())
            .collect();
        for h in handles {
            h.shutdown().await;
        }
        tracing::info!("core stopped");
        Ok(())
    }
}

fn summarize(id: &str, e: &Entry) -> TorrentSummary {
    let stats = e.handle.stats();
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
        id: id.to_owned(),
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
        downloaded: e.downloaded_base + stats.downloaded,
        uploaded: e.uploaded_base + stats.uploaded,
        download_rate: stats.download_rate,
        upload_rate: stats.upload_rate,
        peers: stats.peers,
        seeds: stats.seeds,
        added_at: e.added_at,
        label: e.label.clone(),
        save_path: e.save_path.to_string_lossy().into_owned(),
        private: info.private,
        error: stats.error,
        queue_position: e.queue_position,
        group: e.group.clone(),
        superseded_by: e.superseded_by.clone(),
    }
}

fn summarize_pending(id: &str, p: &Pending) -> TorrentSummary {
    let status = p
        .fetch
        .as_ref()
        .map(|(w, _)| w.borrow().clone())
        .unwrap_or_default();
    let hashes = p.magnet.info_hashes;
    TorrentSummary {
        id: id.to_owned(),
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
    let preferred = data_dir.join("held").join(id);
    let fallback = save_path.join(".apitorrent-held").join(id);
    let probe = || -> std::io::Result<bool> {
        std::fs::create_dir_all(data_dir.join("held"))?;
        std::fs::create_dir_all(save_path)?;
        let name = format!(".apitorrent-probe-{id}");
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

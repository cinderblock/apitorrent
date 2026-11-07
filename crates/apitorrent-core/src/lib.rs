//! apitorrent's core: the torrent registry above the engine.
//!
//! The core owns the instance database, adds and restores torrents in the engine, keeps the
//! revisioned store that clients sync from, and saves resume state. The API server is a thin
//! layer over [`Core`].

pub mod db;
pub mod store;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use tokio::sync::{Notify, broadcast};

use apitorrent_engine::bitfield::Bitfield;
use apitorrent_engine::layout::Layout;
use apitorrent_engine::storage::Placement;
use apitorrent_engine::{
    AddTorrent, Session, SessionConfig, TorrentHandle, TorrentState as EngineState,
    UnwantedPolicy as EnginePolicy,
};
use apitorrent_metainfo::{Magnet, Torrent};
use apitorrent_types::{
    AddSource, AddTorrentRequest, FilePlacement, FilePriority, TorrentDetail, TorrentFile,
    TorrentId, TorrentState, TorrentSummary, UnwantedPolicy,
};

use crate::db::{Database, ResumeRow, TorrentRow};
pub use crate::store::{Patch, Snapshot, Store};

/// How often summaries are refreshed from the engine.
const REFRESH: Duration = Duration::from_secs(1);
/// How often resume state is saved.
const SAVE_EVERY: Duration = Duration::from_secs(30);

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
}

/// Everything needed to start a torrent in the engine and track it.
struct Spawn {
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
}

struct Inner {
    config: CoreConfig,
    db: Arc<Mutex<Database>>,
    session: Session,
    torrents: RwLock<HashMap<TorrentId, Entry>>,
    store: Store,
    stop: Notify,
}

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
        let session = Session::start(config.session.clone()).await?;
        tracing::info!(peer_port = session.listen_addr().port(), "engine started");
        let core = Core {
            inner: Arc::new(Inner {
                config,
                db: Arc::new(Mutex::new(db)),
                session,
                torrents: RwLock::new(HashMap::new()),
                store: Store::new(),
                stop: Notify::new(),
            }),
        };
        core.restore().await?;
        tokio::spawn(core.clone().background());
        Ok(core)
    }

    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    pub fn peer_port(&self) -> u16 {
        self.inner.session.listen_addr().port()
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
            })?;
        }
        tracing::info!(count, "torrents restored");
        self.refresh();
        Ok(())
    }

    fn spawn_entry(&self, s: Spawn) -> Result<TorrentId, CoreError> {
        let Spawn {
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
        } = s;
        let id = torrent.info.info_hashes().id_string();
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
                unwanted: engine_policy(unwanted),
                paused,
                file_priorities: Some(file_priorities.clone()),
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
                // Parse it so a bad link fails as such, then say what's missing.
                Magnet::parse(uri).map_err(|e| CoreError::InvalidRequest(e.to_string()))?;
                return Err(CoreError::Unsupported(
                    "magnet links need metadata exchange, which isn't implemented yet".into(),
                ));
            }
        };
        let torrent = Arc::new(
            Torrent::from_bytes(&bytes).map_err(|e| CoreError::InvalidTorrent(e.to_string()))?,
        );
        let id = torrent.info.info_hashes().id_string();
        if self
            .inner
            .torrents
            .read()
            .expect("torrents lock poisoned")
            .contains_key(&id)
        {
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
        };
        let added_at = row.added_at;
        self.db(move |db| db.insert_torrent(&row)).await?;
        if let Err(e) = self.spawn_entry(Spawn {
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
        }) {
            let id2 = id.clone();
            self.db(move |db| db.remove_torrent(&id2)).await?;
            return Err(e);
        }
        tracing::info!(%id, "torrent added");
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
        }
        if paused {
            handle.pause().await;
        } else {
            handle.resume().await;
        }
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
        let (torrent, handle, prios, unwanted_policy) = {
            let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
            let e = torrents.get(id).ok_or(CoreError::NotFound)?;
            (
                e.torrent.clone(),
                e.handle.clone(),
                e.file_priorities.clone(),
                e.unwanted,
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
        })
    }

    /// Rebuild every summary from the engine and publish the changes.
    fn refresh(&self) {
        let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
        let summaries = torrents.iter().map(|(id, e)| summarize(id, e)).collect();
        drop(torrents);
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
                rows.push(ResumeRow {
                    id,
                    have: have.as_bytes().to_vec(),
                    placements: placements.into_iter().map(placement_to_byte).collect(),
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
        loop {
            tokio::select! {
                _ = refresh.tick() => self.refresh(),
                _ = save.tick() => {
                    if let Err(e) = self.save_resume().await {
                        tracing::error!(error = %e, "saving resume state failed");
                    }
                }
                () = self.inner.stop.notified() => break,
            }
        }
    }

    /// Save resume state and stop every torrent.
    pub async fn shutdown(&self) -> Result<(), CoreError> {
        self.inner.stop.notify_waiters();
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
        } else {
            state
        },
        progress: if stats.pieces_total == 0 {
            1.0
        } else {
            ratio(stats.pieces_have as usize, stats.pieces_total as usize)
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

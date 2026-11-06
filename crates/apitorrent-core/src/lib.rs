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
use apitorrent_engine::{
    AddTorrent, Session, SessionConfig, TorrentHandle, TorrentState as EngineState,
};
use apitorrent_metainfo::{Magnet, Torrent};
use apitorrent_types::{
    AddSource, AddTorrentRequest, TorrentDetail, TorrentFile, TorrentId, TorrentState,
    TorrentSummary,
};

use crate::db::{Database, TorrentRow};
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
    /// Totals from previous sessions; the engine's counters start at zero each run.
    downloaded_base: u64,
    uploaded_base: u64,
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
            self.spawn_entry(
                torrent,
                PathBuf::from(&row.save_path),
                have,
                row.paused,
                prios,
                row.label,
                row.added_at,
                (row.downloaded_total, row.uploaded_total),
            )?;
        }
        tracing::info!(count, "torrents restored");
        self.refresh();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_entry(
        &self,
        torrent: Arc<Torrent>,
        save_path: PathBuf,
        have: Option<Bitfield>,
        paused: bool,
        file_priorities: Vec<u8>,
        label: Option<String>,
        added_at: i64,
        (downloaded_base, uploaded_base): (u64, u64),
    ) -> Result<TorrentId, CoreError> {
        let id = torrent.info.info_hashes().id_string();
        let handle = self
            .inner
            .session
            .add_torrent(AddTorrent {
                torrent: torrent.clone(),
                save_path: save_path.clone(),
                have,
                paused,
                peers: Vec::new(),
                file_priorities: Some(file_priorities.clone()),
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
        };
        let added_at = row.added_at;
        self.db(move |db| db.insert_torrent(&row)).await?;
        if let Err(e) = self.spawn_entry(
            torrent,
            save_path,
            None,
            req.paused,
            prios,
            req.label,
            added_at,
            (0, 0),
        ) {
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

    pub async fn set_file_priorities(&self, id: &str, prios: Vec<u8>) -> Result<(), CoreError> {
        let handle = self.handle(id)?;
        {
            let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
            let files = torrents
                .get(id)
                .ok_or(CoreError::NotFound)?
                .torrent
                .info
                .files
                .len();
            if prios.len() != files {
                return Err(CoreError::InvalidRequest(format!(
                    "{} file priorities for {files} files",
                    prios.len()
                )));
            }
        }
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

    /// Remove a torrent, and optionally its downloaded files.
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
        if delete_files {
            delete_torrent_files(&entry.torrent, &entry.save_path);
        }
        self.inner.store.apply(Vec::new(), vec![id.to_owned()]);
        tracing::info!(%id, delete_files, "torrent removed");
        Ok(())
    }

    pub fn list(&self) -> Vec<TorrentSummary> {
        self.inner.store.list()
    }

    pub async fn detail(&self, id: &str) -> Result<TorrentDetail, CoreError> {
        let (torrent, handle, prios) = {
            let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
            let e = torrents.get(id).ok_or(CoreError::NotFound)?;
            (
                e.torrent.clone(),
                e.handle.clone(),
                e.file_priorities.clone(),
            )
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
                rows.push((
                    id,
                    have.as_bytes().to_vec(),
                    down + stats.downloaded,
                    up + stats.uploaded,
                ));
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

/// Delete a torrent's files, then any directories that are left empty. Only paths that
/// belong to the torrent are touched.
fn delete_torrent_files(torrent: &Torrent, save_path: &std::path::Path) {
    let mut dirs = std::collections::BTreeSet::new();
    for f in torrent.info.files.iter().filter(|f| !f.is_pad()) {
        let mut p = save_path.to_path_buf();
        for c in &f.path {
            p.push(c);
        }
        if let Err(e) = std::fs::remove_file(&p)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %p.display(), error = %e, "could not delete file");
        }
        let mut dir = p.parent().map(std::path::Path::to_path_buf);
        while let Some(d) = dir {
            if d == save_path {
                break;
            }
            dirs.insert(d.clone());
            dir = d.parent().map(std::path::Path::to_path_buf);
        }
    }
    // Deepest first; remove_dir fails harmlessly on directories that aren't empty.
    for d in dirs.iter().rev() {
        let _ = std::fs::remove_dir(d);
    }
}

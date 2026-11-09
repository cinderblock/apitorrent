//! What plugins may ask of apitorrent.

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use ts_rs::TS;

use apitorrent_types::automation::{ReleaseCandidate, SubmitCandidatesOutcome};
use apitorrent_types::{
    AddTorrentRequest, QueueMove, QueueSettings, SeedLimits, TorrentDetail, TorrentId,
    TorrentState, TorrentSummary,
};

use crate::BoxFuture;

/// A failed call, as the plugin sees it: a stable code and a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, thiserror::Error)]
#[serde(rename_all = "camelCase")]
#[error("{message}")]
pub struct AppError {
    pub code: String,
    pub message: String,
}

impl AppError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// The result of adding a torrent. Adding one that already exists isn't an error for a
/// plugin: a feed seeing the same release twice is normal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AddOutcome {
    /// The new torrent's id; `null` when it was a duplicate.
    pub id: Option<TorrentId>,
    pub duplicate: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TorrentControl {
    Pause,
    Resume,
    SetLabel(Option<String>),
    /// The torrent's own seed limits; `None` follows its group's or the instance's.
    SetSeedLimits(Option<SeedLimits>),
    /// The torrent's own rate limits in bytes per second (0: none of its own). Its
    /// connection limit is kept.
    SetRateLimits {
        download: u64,
        upload: u64,
    },
    /// Move it in the queue.
    Queue(QueueMove),
}

/// About this apitorrent instance, for plugins that describe it to other software (a
/// compatibility layer's "preferences", say). Needs `torrents:read`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    /// apitorrent's version.
    pub version: String,
    /// Where torrents are saved unless they say otherwise (absolute). Plugins name save
    /// paths relative to it.
    pub download_dir: String,
    /// The instance's seed limits, which torrents without their own (or their group's)
    /// follow.
    pub seed_limits: SeedLimits,
    pub queue: QueueSettings,
    /// The port peers connect to.
    pub peer_port: u16,
}

/// Something that happened to a torrent. Rate and progress ticks are not events; poll
/// `torrents.list()` for those.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type")]
pub enum AppEvent {
    #[serde(rename = "torrent.added")]
    TorrentAdded { torrent: TorrentSummary },
    #[serde(rename = "torrent.removed")]
    TorrentRemoved { id: TorrentId },
    #[serde(rename = "torrent.stateChanged")]
    TorrentStateChanged {
        torrent: TorrentSummary,
        previous: TorrentState,
    },
    /// Every wanted piece is downloaded (progress reached 1).
    #[serde(rename = "torrent.finished")]
    TorrentFinished { torrent: TorrentSummary },
}

/// What plugins may ask of apitorrent. The host checks each call against the plugin's
/// grants before it gets here, so implementations don't.
///
/// Futures are `'static` and `Send` because the host runs them on the daemon's runtime, not
/// on a plugin's thread: work they spawn must outlive any one plugin.
pub trait AppApi: Send + Sync + 'static {
    fn info(&self) -> AppInfo;
    fn torrents(&self) -> Vec<TorrentSummary>;
    fn torrent(&self, id: TorrentId) -> BoxFuture<Result<TorrentDetail, AppError>>;
    fn add_torrent(&self, req: AddTorrentRequest) -> BoxFuture<Result<AddOutcome, AppError>>;
    fn control(&self, id: TorrentId, action: TorrentControl) -> BoxFuture<Result<(), AppError>>;
    fn remove(&self, id: TorrentId, delete_files: bool) -> BoxFuture<Result<(), AppError>>;
    /// A stream of events from now on. A receiver that falls behind skips ahead (`Lagged`).
    fn events(&self) -> broadcast::Receiver<AppEvent>;

    /// Store release candidates from `source`, a plugin with the `source` role (the host
    /// checks the role), for the core's rules. Submitting a candidate again updates it.
    /// Implementations without rules refuse.
    fn submit_candidates(
        &self,
        source: &str,
        candidates: Vec<ReleaseCandidate>,
    ) -> BoxFuture<Result<SubmitCandidatesOutcome, AppError>> {
        let _ = (source, candidates);
        Box::pin(async {
            Err(AppError::new(
                "unsupported",
                "this server doesn't take release candidates",
            ))
        })
    }
}

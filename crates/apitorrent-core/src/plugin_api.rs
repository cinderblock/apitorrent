//! The core's side of the plugin API: [`AppApi`] over a running [`Core`].
//!
//! The plugin host checks every call against the plugin's grants before it gets here.
//! What this adds is the core's own view of plugin requests: save paths are relative to the
//! download directory (plugins get no say over the rest of the filesystem), a duplicate add
//! is a normal outcome rather than an error, and torrent changes become events.

use std::collections::HashMap;
use std::path::{Component, Path};
use std::sync::Arc;

use tokio::sync::broadcast;

use apitorrent_plugin_api::{AddOutcome, AppApi, AppError, AppEvent, BoxFuture, TorrentControl};
use apitorrent_types::{AddTorrentRequest, TorrentDetail, TorrentId, TorrentState, TorrentSummary};

use crate::{Core, CoreError, Patch};

/// How many events a slow plugin may fall behind before it skips some.
const EVENT_BACKLOG: usize = 1024;

impl From<CoreError> for AppError {
    fn from(e: CoreError) -> Self {
        let message = match &e {
            // Internal details go to the daemon's log, not to plugins.
            CoreError::Database(_) | CoreError::Io(_) => {
                tracing::error!(error = %e, "a plugin's request failed");
                "internal error".to_owned()
            }
            _ => e.to_string(),
        };
        AppError::new(e.code(), message)
    }
}

/// [`AppApi`] for plugins, backed by the core.
#[derive(Debug, Clone)]
pub struct CorePluginApi {
    core: Core,
    events: broadcast::Sender<AppEvent>,
}

impl CorePluginApi {
    /// Start translating the core's state changes into plugin events. Call inside the
    /// daemon's tokio runtime; the translator runs until the core shuts down.
    pub fn new(core: Core) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BACKLOG);
        tokio::spawn(translate(core.clone(), events.clone()));
        Arc::new(Self { core, events })
    }
}

/// A plugin's save path, joined under the download directory. Absolute paths, `..`, drive
/// prefixes and the like are refused.
fn plugin_save_path(download_dir: &Path, requested: &str) -> Result<String, AppError> {
    let rel = Path::new(requested);
    let plain = !requested.is_empty()
        && !requested.contains(':')
        && rel.components().all(|c| matches!(c, Component::Normal(_)));
    if !plain {
        return Err(AppError::new(
            "invalid_request",
            format!("savePath `{requested}` must be relative to the download directory"),
        ));
    }
    Ok(download_dir.join(rel).to_string_lossy().into_owned())
}

impl AppApi for CorePluginApi {
    fn torrents(&self) -> Vec<TorrentSummary> {
        self.core.list()
    }

    fn torrent(&self, id: TorrentId) -> BoxFuture<Result<TorrentDetail, AppError>> {
        let core = self.core.clone();
        Box::pin(async move { Ok(core.detail(&id).await?) })
    }

    fn add_torrent(&self, mut req: AddTorrentRequest) -> BoxFuture<Result<AddOutcome, AppError>> {
        let core = self.core.clone();
        Box::pin(async move {
            if let Some(p) = &req.save_path {
                req.save_path = Some(plugin_save_path(&core.inner.config.download_dir, p)?);
            }
            match core.add(req).await {
                Ok(id) => Ok(AddOutcome {
                    id: Some(id),
                    duplicate: false,
                }),
                Err(CoreError::Duplicate) => Ok(AddOutcome {
                    id: None,
                    duplicate: true,
                }),
                Err(e) => Err(e.into()),
            }
        })
    }

    fn control(&self, id: TorrentId, action: TorrentControl) -> BoxFuture<Result<(), AppError>> {
        let core = self.core.clone();
        Box::pin(async move {
            match action {
                TorrentControl::Pause => core.pause(&id).await?,
                TorrentControl::Resume => core.resume(&id).await?,
                TorrentControl::SetLabel(label) => core.set_label(&id, label).await?,
            }
            Ok(())
        })
    }

    fn remove(&self, id: TorrentId, delete_files: bool) -> BoxFuture<Result<(), AppError>> {
        let core = self.core.clone();
        Box::pin(async move { Ok(core.remove(&id, delete_files).await?) })
    }

    fn events(&self) -> broadcast::Receiver<AppEvent> {
        self.events.subscribe()
    }

    fn submit_candidates(
        &self,
        source: &str,
        candidates: Vec<apitorrent_types::automation::ReleaseCandidate>,
    ) -> BoxFuture<Result<apitorrent_types::automation::SubmitCandidatesOutcome, AppError>> {
        let core = self.core.clone();
        let source = format!("plugin:{source}");
        Box::pin(async move { Ok(core.submit_candidates(&source, candidates).await?) })
    }
}

/// What the translator remembers about each torrent.
#[derive(Debug, Clone, Copy)]
struct Seen {
    state: TorrentState,
    finished: bool,
}

fn seen(s: &TorrentSummary) -> Seen {
    Seen {
        state: s.state,
        finished: s.progress >= 1.0,
    }
}

/// The events one updated summary means, given what was known before.
fn events_for(before: Option<Seen>, now: &TorrentSummary) -> Vec<AppEvent> {
    let mut out = Vec::new();
    match before {
        None => out.push(AppEvent::TorrentAdded {
            torrent: now.clone(),
        }),
        Some(b) => {
            if b.state != now.state {
                out.push(AppEvent::TorrentStateChanged {
                    torrent: now.clone(),
                    previous: b.state,
                });
            }
            // A torrent that is checked on add and already complete isn't "finished" by
            // downloading; only the transition counts.
            if !b.finished && now.progress >= 1.0 {
                out.push(AppEvent::TorrentFinished {
                    torrent: now.clone(),
                });
            }
        }
    }
    out
}

fn apply(known: &mut HashMap<TorrentId, Seen>, patch: &Patch, tx: &broadcast::Sender<AppEvent>) {
    for s in &patch.upserted {
        for e in events_for(known.get(&s.id).copied(), s) {
            let _ = tx.send(e);
        }
        known.insert(s.id.clone(), seen(s));
    }
    for id in &patch.removed {
        if known.remove(id).is_some() {
            let _ = tx.send(AppEvent::TorrentRemoved { id: id.clone() });
        }
    }
}

async fn translate(core: Core, tx: broadcast::Sender<AppEvent>) {
    loop {
        let (snapshot, mut rx) = core.subscribe();
        let mut known: HashMap<TorrentId, Seen> = snapshot
            .torrents
            .iter()
            .map(|s| (s.id.clone(), seen(s)))
            .collect();
        loop {
            match rx.recv().await {
                Ok(patch) => apply(&mut known, &patch, &tx),
                // Fell behind the store: start again from a fresh snapshot. Changes in the
                // gap are lost, as the event stream documents.
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "plugin event translation fell behind");
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(state: TorrentState, progress: f64) -> TorrentSummary {
        TorrentSummary {
            id: "aa".into(),
            name: "x".into(),
            info_hash_v1: None,
            info_hash_v2: None,
            state,
            progress,
            size: 1,
            pieces_have: 0,
            pieces_total: 1,
            downloaded: 0,
            uploaded: 0,
            download_rate: 0,
            upload_rate: 0,
            peers: 0,
            seeds: 0,
            added_at: 0,
            label: None,
            save_path: String::new(),
            private: false,
            error: None,
            queue_position: 0,
            group: None,
            superseded_by: None,
        }
    }

    fn kinds(events: &[AppEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|e| match e {
                AppEvent::TorrentAdded { .. } => "added",
                AppEvent::TorrentRemoved { .. } => "removed",
                AppEvent::TorrentStateChanged { .. } => "state",
                AppEvent::TorrentFinished { .. } => "finished",
            })
            .collect()
    }

    #[test]
    fn transitions_become_events() {
        let downloading = summary(TorrentState::Downloading, 0.5);
        assert_eq!(kinds(&events_for(None, &downloading)), ["added"]);
        let before = Some(seen(&downloading));
        // Rate and progress ticks are not events.
        assert!(events_for(before, &summary(TorrentState::Downloading, 0.6)).is_empty());
        assert_eq!(
            kinds(&events_for(before, &summary(TorrentState::Seeding, 1.0))),
            ["state", "finished"]
        );
        let done = Some(seen(&summary(TorrentState::Seeding, 1.0)));
        assert_eq!(
            kinds(&events_for(done, &summary(TorrentState::Paused, 1.0))),
            ["state"]
        );
    }

    #[test]
    fn plugin_save_paths_stay_under_the_download_directory() {
        let dl = Path::new("downloads");
        assert_eq!(
            plugin_save_path(dl, "tv/shows").unwrap(),
            Path::new("downloads").join("tv/shows").to_string_lossy()
        );
        for bad in ["", "/etc", "../up", "a/../../b", "C:\\x", "C:x"] {
            assert!(plugin_save_path(dl, bad).is_err(), "{bad}");
        }
    }
}

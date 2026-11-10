//! Pack switchover with tracker adapters (see the architecture doc, "Pack switchover").
//!
//! Every [`RETIRE_EVERY`](crate) the core looks at torrents whose trackers report errors:
//!
//! - **Is it unregistered?** For a tracker a `tracker-adapter` plugin covers, the adapter
//!   decides, and the built-in patterns ([`crate::library::is_unregistered`]) don't apply.
//!   For any other tracker the built-in patterns do.
//! - **Superseded and unregistered:** it is retired (removed, files kept: the torrent that
//!   superseded it holds the same data).
//! - **Unregistered, nothing supersedes it yet, and an adapter covers the tracker:** the
//!   adapter's `findPack` may name the pack that holds its files. The core adds that pack
//!   next to it (same folder, same label). Library matching reuses the files, the pack
//!   supersedes the single once its check proves them, and a later pass retires the
//!   single. A torrent is asked about at most once per [`PACK_RETRY`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use apitorrent_engine::TorrentHandle;
use apitorrent_plugin_api::{PackFile, PackQuery, TrackerAdapters, UnregisteredQuery};
use apitorrent_types::{AddTorrentRequest, UnwantedPolicy};

use crate::{Core, CoreError, Key, library};

/// How long before a torrent whose pack wasn't found is asked about again.
pub const PACK_RETRY: Duration = Duration::from_secs(6 * 3600);

/// The core's tracker adapters, and which torrents it has asked them about.
#[derive(Default)]
pub(crate) struct AdapterState {
    adapters: RwLock<Option<Arc<dyn TrackerAdapters>>>,
    asked: Mutex<HashMap<Key, Instant>>,
}

impl std::fmt::Debug for AdapterState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdapterState")
            .field(
                "adapters",
                &self
                    .adapters
                    .read()
                    .expect("adapters lock poisoned")
                    .is_some(),
            )
            .finish_non_exhaustive()
    }
}

/// What one pass needs to know about a torrent.
struct Candidate {
    id: Key,
    name: String,
    handle: TorrentHandle,
    superseded: bool,
}

impl Core {
    /// Use these tracker adapters (the plugin host's), or none.
    pub fn set_tracker_adapters(&self, adapters: Option<Arc<dyn TrackerAdapters>>) {
        *self
            .inner
            .adapters
            .adapters
            .write()
            .expect("adapters lock poisoned") = adapters;
    }

    fn tracker_adapters(&self) -> Option<Arc<dyn TrackerAdapters>> {
        self.inner
            .adapters
            .adapters
            .read()
            .expect("adapters lock poisoned")
            .clone()
    }
}

/// One pass of pack switchover; see the module docs.
pub(crate) async fn retire_superseded(core: &Core) {
    if !core.settings().library.retire_superseded {
        return;
    }
    let adapters = core.tracker_adapters();
    let candidates: Vec<Candidate> = core
        .inner
        .torrents
        .read()
        .expect("torrents lock poisoned")
        .iter()
        // Without adapters only superseded torrents can lead anywhere.
        .filter(|(_, e)| e.superseded_by.is_some() || adapters.is_some())
        .map(|(id, e)| Candidate {
            id: id.clone(),
            name: e.torrent.info.name.clone(),
            handle: e.handle.clone(),
            superseded: e.superseded_by.is_some(),
        })
        .collect();
    for c in candidates {
        // `Some(covering tracker)` once a tracker says the torrent is gone.
        let mut gone: Option<Option<String>> = None;
        for t in c.handle.trackers_of(Some(c.id.engine())).await {
            let Some(message) = t.error.as_deref() else {
                continue;
            };
            let covering = adapters.as_ref().filter(|a| a.covers(&t.url));
            let verdict = match covering {
                Some(a) => a
                    .unregistered(UnregisteredQuery {
                        torrent: c.id.id.clone(),
                        name: c.name.clone(),
                        tracker: t.url.clone(),
                        message: message.to_owned(),
                    })
                    .await
                    // An adapter that fails to answer doesn't retire anything.
                    .unwrap_or(false),
                // Built-in patterns only decide retirements, never pack lookups.
                None => c.superseded && library::is_unregistered(message),
            };
            if verdict {
                gone = Some(covering.is_some().then(|| t.url.clone()));
                break;
            }
        }
        let Some(covered_by) = gone else {
            continue;
        };
        if c.superseded {
            tracing::info!(id = %c.id, "retiring a superseded torrent its tracker no longer knows");
            if let Err(e) = core.remove(&c.id, false).await {
                tracing::warn!(id = %c.id, error = %e, "could not retire");
            }
        } else if let (Some(a), Some(tracker)) = (&adapters, covered_by) {
            fetch_pack(core, a.as_ref(), &c, tracker).await;
        }
    }
    // Forget torrents that are gone.
    let torrents = core.inner.torrents.read().expect("torrents lock poisoned");
    core.inner
        .adapters
        .asked
        .lock()
        .expect("asked lock poisoned")
        .retain(|id, at| torrents.contains_key(id) && at.elapsed() < PACK_RETRY);
}

/// Ask the adapter for the pack that holds `c`'s files, and add it.
async fn fetch_pack(core: &Core, adapters: &dyn TrackerAdapters, c: &Candidate, tracker: String) {
    {
        let mut asked = core
            .inner
            .adapters
            .asked
            .lock()
            .expect("asked lock poisoned");
        if asked.get(&c.id).is_some_and(|at| at.elapsed() < PACK_RETRY) {
            return;
        }
        asked.insert(c.id.clone(), Instant::now());
    }
    let detail = match core.detail(&c.id).await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(id = %c.id, error = %e, "could not describe a torrent for a pack lookup");
            return;
        }
    };
    let query = PackQuery {
        torrent: c.id.id.clone(),
        name: detail.summary.name.clone(),
        info_hash_v1: detail.summary.info_hash_v1.clone(),
        info_hash_v2: detail.summary.info_hash_v2.clone(),
        tracker,
        files: detail
            .files
            .iter()
            .map(|f| PackFile {
                path: f.path.clone(),
                length: f.length,
            })
            .collect(),
    };
    let Some(source) = adapters.find_pack(query).await else {
        tracing::debug!(id = %c.id, "the tracker adapter knows no pack for this torrent");
        return;
    };
    let req = AddTorrentRequest {
        source,
        save_path: Some(detail.summary.save_path.clone()),
        paused: false,
        label: detail.summary.label.clone(),
        file_priorities: None,
        unwanted_policy: UnwantedPolicy::default(),
    };
    match core.add(c.id.space, req).await {
        Ok(pack) => tracing::info!(
            id = %c.id,
            %pack,
            "added the pack that replaces an unregistered torrent"
        ),
        Err(CoreError::Duplicate) => {
            tracing::debug!(id = %c.id, "the pack that replaces this torrent is already here");
        }
        Err(e) => {
            tracing::warn!(id = %c.id, error = %e, "could not add the pack a tracker adapter named");
        }
    }
}

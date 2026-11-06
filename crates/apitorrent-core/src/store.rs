//! The revisioned state store behind `/api/v1/sync`.
//!
//! Every change bumps the revision by one and is broadcast as a patch. A subscriber gets a
//! snapshot and a receiver atomically, so it sees every patch after its snapshot and no
//! patch twice.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use tokio::sync::broadcast;

use apitorrent_types::{TorrentId, TorrentSummary};

/// How many patches a slow subscriber may fall behind before it must resynchronise.
const BACKLOG: usize = 256;

#[derive(Debug, Clone, PartialEq)]
pub struct Patch {
    pub revision: u64,
    pub upserted: Vec<TorrentSummary>,
    pub removed: Vec<TorrentId>,
}

#[derive(Debug)]
pub struct Snapshot {
    pub revision: u64,
    pub torrents: Vec<TorrentSummary>,
}

#[derive(Debug, Default)]
struct Inner {
    revision: u64,
    torrents: BTreeMap<TorrentId, TorrentSummary>,
}

#[derive(Debug)]
pub struct Store {
    inner: RwLock<Inner>,
    tx: broadcast::Sender<Arc<Patch>>,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

impl Store {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Inner::default()),
            tx: broadcast::channel(BACKLOG).0,
        }
    }

    /// Replace summaries and remove ids. Only real changes count: an upsert identical to the
    /// stored summary is dropped, and nothing is broadcast if nothing changed.
    pub fn apply(&self, upserted: Vec<TorrentSummary>, removed: Vec<TorrentId>) {
        let mut inner = self.inner.write().expect("store lock poisoned");
        let upserted: Vec<TorrentSummary> = upserted
            .into_iter()
            .filter(|s| inner.torrents.get(&s.id) != Some(s))
            .collect();
        let removed: Vec<TorrentId> = removed
            .into_iter()
            .filter(|id| inner.torrents.contains_key(id))
            .collect();
        if upserted.is_empty() && removed.is_empty() {
            return;
        }
        for s in &upserted {
            inner.torrents.insert(s.id.clone(), s.clone());
        }
        for id in &removed {
            inner.torrents.remove(id);
        }
        inner.revision += 1;
        // Sending under the lock keeps patches in revision order for every subscriber.
        let _ = self.tx.send(Arc::new(Patch {
            revision: inner.revision,
            upserted,
            removed,
        }));
    }

    pub fn subscribe(&self) -> (Snapshot, broadcast::Receiver<Arc<Patch>>) {
        let inner = self.inner.read().expect("store lock poisoned");
        let rx = self.tx.subscribe();
        (
            Snapshot {
                revision: inner.revision,
                torrents: inner.torrents.values().cloned().collect(),
            },
            rx,
        )
    }

    pub fn get(&self, id: &str) -> Option<TorrentSummary> {
        self.inner
            .read()
            .expect("store lock poisoned")
            .torrents
            .get(id)
            .cloned()
    }

    pub fn list(&self) -> Vec<TorrentSummary> {
        self.inner
            .read()
            .expect("store lock poisoned")
            .torrents
            .values()
            .cloned()
            .collect()
    }

    pub fn revision(&self) -> u64 {
        self.inner.read().expect("store lock poisoned").revision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apitorrent_types::TorrentState;

    fn summary(id: &str, peers: u32) -> TorrentSummary {
        TorrentSummary {
            id: id.into(),
            name: id.into(),
            info_hash_v1: None,
            info_hash_v2: None,
            state: TorrentState::Downloading,
            progress: 0.0,
            size: 0,
            pieces_have: 0,
            pieces_total: 1,
            downloaded: 0,
            uploaded: 0,
            download_rate: 0,
            upload_rate: 0,
            peers,
            seeds: 0,
            added_at: 0,
            label: None,
            save_path: String::new(),
            private: false,
            error: None,
        }
    }

    #[test]
    fn revisions_and_dedup() {
        let store = Store::new();
        let (snap, mut rx) = store.subscribe();
        assert_eq!(snap.revision, 0);
        store.apply(vec![summary("a", 1)], vec![]);
        store.apply(vec![summary("a", 1)], vec![]); // unchanged: no revision
        store.apply(vec![summary("a", 2)], vec![]);
        store.apply(vec![], vec!["missing".into()]); // nothing to remove: no revision
        store.apply(vec![], vec!["a".into()]);
        let revs: Vec<u64> =
            std::iter::from_fn(|| rx.try_recv().ok().map(|p| p.revision)).collect();
        assert_eq!(revs, [1, 2, 3]);
        assert_eq!(store.revision(), 3);
        assert_eq!(store.list(), [] as [apitorrent_types::TorrentSummary; 0]);
    }
}

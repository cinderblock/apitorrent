//! The revisioned state store behind `/api/v1/sync`.
//!
//! Every change bumps the revision by one and is broadcast as a patch. A subscriber gets a
//! snapshot and a receiver atomically, so it sees every patch after its snapshot and no
//! patch twice. Summaries are keyed by space and torrent id; a subscriber watching one
//! space filters the patches (see [`Patch::for_space`]), so it may see revisions skip.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use tokio::sync::broadcast;

use apitorrent_types::TorrentSummary;
use apitorrent_types::spaces::SpaceId;

use crate::Key;

/// How many patches a slow subscriber may fall behind before it must resynchronise.
const BACKLOG: usize = 256;

#[derive(Debug, Clone, PartialEq)]
pub struct Patch {
    pub revision: u64,
    pub upserted: Vec<TorrentSummary>,
    pub removed: Vec<Key>,
}

impl Patch {
    /// The part of the patch about one space (all of it for `None`), or `None` when that
    /// part is empty.
    pub fn for_space(&self, space: Option<SpaceId>) -> Option<Patch> {
        let Some(space) = space else {
            return Some(self.clone());
        };
        let upserted: Vec<TorrentSummary> = self
            .upserted
            .iter()
            .filter(|s| s.space == space)
            .cloned()
            .collect();
        let removed: Vec<Key> = self
            .removed
            .iter()
            .filter(|k| k.space == space)
            .cloned()
            .collect();
        (!upserted.is_empty() || !removed.is_empty()).then_some(Patch {
            revision: self.revision,
            upserted,
            removed,
        })
    }
}

#[derive(Debug)]
pub struct Snapshot {
    pub revision: u64,
    pub torrents: Vec<TorrentSummary>,
}

#[derive(Debug, Default)]
struct Inner {
    revision: u64,
    torrents: BTreeMap<Key, TorrentSummary>,
}

fn key_of(s: &TorrentSummary) -> Key {
    Key::new(s.space, s.id.clone())
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
    pub fn apply(&self, upserted: Vec<TorrentSummary>, removed: Vec<Key>) {
        let mut inner = self.inner.write().expect("store lock poisoned");
        let upserted: Vec<TorrentSummary> = upserted
            .into_iter()
            .filter(|s| inner.torrents.get(&key_of(s)) != Some(s))
            .collect();
        let removed: Vec<Key> = removed
            .into_iter()
            .filter(|id| inner.torrents.contains_key(id))
            .collect();
        if upserted.is_empty() && removed.is_empty() {
            return;
        }
        for s in &upserted {
            inner.torrents.insert(key_of(s), s.clone());
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

    /// A snapshot of one space (or every space) and every patch after it.
    pub fn subscribe(&self, space: Option<SpaceId>) -> (Snapshot, broadcast::Receiver<Arc<Patch>>) {
        let inner = self.inner.read().expect("store lock poisoned");
        let rx = self.tx.subscribe();
        (
            Snapshot {
                revision: inner.revision,
                torrents: inner
                    .torrents
                    .values()
                    .filter(|s| space.is_none_or(|sp| s.space == sp))
                    .cloned()
                    .collect(),
            },
            rx,
        )
    }

    pub fn get(&self, key: &Key) -> Option<TorrentSummary> {
        self.inner
            .read()
            .expect("store lock poisoned")
            .torrents
            .get(key)
            .cloned()
    }

    pub fn list(&self, space: Option<SpaceId>) -> Vec<TorrentSummary> {
        self.inner
            .read()
            .expect("store lock poisoned")
            .torrents
            .values()
            .filter(|s| space.is_none_or(|sp| s.space == sp))
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
            space: 1,
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
            queue_position: 0,
            group: None,
            superseded_by: None,
        }
    }

    #[test]
    fn revisions_and_dedup() {
        let store = Store::new();
        let (snap, mut rx) = store.subscribe(None);
        assert_eq!(snap.revision, 0);
        store.apply(vec![summary("a", 1)], vec![]);
        store.apply(vec![summary("a", 1)], vec![]); // unchanged: no revision
        store.apply(vec![summary("a", 2)], vec![]);
        store.apply(vec![], vec![Key::new(1, "missing")]); // nothing to remove: no revision
        store.apply(vec![], vec![Key::new(1, "a")]);
        let revs: Vec<u64> =
            std::iter::from_fn(|| rx.try_recv().ok().map(|p| p.revision)).collect();
        assert_eq!(revs, [1, 2, 3]);
        assert_eq!(store.revision(), 3);
        assert_eq!(
            store.list(None),
            [] as [apitorrent_types::TorrentSummary; 0]
        );
    }

    #[test]
    fn spaces_are_apart() {
        let store = Store::new();
        let other = TorrentSummary {
            space: 2,
            ..summary("a", 5)
        };
        store.apply(vec![summary("a", 1), other.clone()], vec![]);
        assert_eq!(store.list(None).len(), 2, "the same id in two spaces");
        assert_eq!(store.list(Some(2)), std::slice::from_ref(&other));
        let (snap, mut rx) = store.subscribe(Some(1));
        assert_eq!(snap.torrents.len(), 1);
        store.apply(vec![], vec![Key::new(2, "a")]);
        let patch = rx.try_recv().unwrap();
        assert!(patch.for_space(Some(1)).is_none(), "nothing about space 1");
        assert_eq!(
            patch.for_space(Some(2)).unwrap().removed,
            [Key::new(2, "a")]
        );
        assert_eq!(store.get(&Key::new(1, "a")).unwrap().peers, 1);
    }
}

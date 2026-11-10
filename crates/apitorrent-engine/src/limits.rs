//! Rate and connection limits.
//!
//! Limits form a hierarchy: session, then space, then group, then torrent entry. A transfer
//! must fit every level it belongs to, so the tightest one wins. Rates are enforced by the peer I/O tasks:
//! after reading (or before writing) `n` bytes they reserve `n` from each bucket in their
//! chain and sleep for the longest wait. Reading slower is real backpressure: TCP's window
//! closes and the remote end slows down.
//!
//! Every byte on the wire counts, protocol overhead included, so a limit means what the
//! network sees.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// How much a bucket may run ahead of its rate: a burst of this long at full rate passes
/// without waiting. Short enough that limits look smooth, long enough to absorb one
/// message batch.
const BURST: Duration = Duration::from_millis(250);

/// A rate limit, as a GCRA ("generic cell rate algorithm") bucket. `rate` is bytes per
/// second; 0 means unlimited. The rate can change at any time.
#[derive(Debug)]
pub struct Bucket {
    rate: AtomicU64,
    /// The theoretical arrival time: when the bucket would be empty if nothing more came.
    tat: Mutex<Option<Instant>>,
}

impl Bucket {
    pub fn new(rate: u64) -> Self {
        Self {
            rate: AtomicU64::new(rate),
            tat: Mutex::new(None),
        }
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    pub fn set_rate(&self, rate: u64) {
        self.rate.store(rate, Ordering::Relaxed);
    }

    /// Take `n` units and return how long to wait before using them. Always succeeds: a
    /// large reservation just means a longer wait, so big messages can't starve.
    pub fn reserve(&self, n: u64) -> Duration {
        self.reserve_at(n, Instant::now())
    }

    fn reserve_at(&self, n: u64, now: Instant) -> Duration {
        let rate = self.rate();
        if rate == 0 {
            return Duration::ZERO;
        }
        let mut tat = self.tat.lock().expect("bucket lock poisoned");
        let start = tat.map_or(now, |t| t.max(now));
        let next = start + cost(n, rate);
        *tat = Some(next);
        next.saturating_duration_since(now + BURST)
    }

    /// Take `n` units only if that needs no wait. For connection attempts, which are
    /// skipped rather than delayed.
    pub fn try_take(&self, n: u64) -> bool {
        let rate = self.rate();
        if rate == 0 {
            return true;
        }
        let now = Instant::now();
        let mut tat = self.tat.lock().expect("bucket lock poisoned");
        let start = tat.map_or(now, |t| t.max(now));
        let next = start + cost(n, rate);
        if next > now + BURST {
            return false;
        }
        *tat = Some(next);
        true
    }
}

/// How long `n` units take at `rate` per second.
fn cost(n: u64, rate: u64) -> Duration {
    let nanos = u128::from(n) * 1_000_000_000 / u128::from(rate);
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

/// A count of something (open connections, connection attempts in progress) with an
/// optional maximum. A [`Slot`] holds one unit until dropped.
#[derive(Debug, Default)]
pub struct Counter {
    limit: AtomicUsize,
    used: AtomicUsize,
}

impl Counter {
    /// `limit` 0 means unlimited.
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit: AtomicUsize::new(limit),
            used: AtomicUsize::new(0),
        })
    }

    pub fn set_limit(&self, limit: usize) {
        self.limit.store(limit, Ordering::Relaxed);
    }

    pub fn limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }

    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    /// Take a unit if one is free.
    pub fn try_acquire(self: &Arc<Self>) -> Option<Slot> {
        let limit = self.limit();
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            if limit != 0 && used >= limit {
                return None;
            }
            match self.used.compare_exchange_weak(
                used,
                used + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Slot(self.clone())),
                Err(now) => used = now,
            }
        }
    }
}

/// One unit of a [`Counter`], given back on drop.
#[derive(Debug)]
pub struct Slot(Arc<Counter>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.used.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Limits shared by a named set of torrents: a group (selected by label) or a space.
#[derive(Debug)]
pub struct Group {
    pub download: Bucket,
    pub upload: Bucket,
    pub connections: Arc<Counter>,
}

/// Limits for a group. 0 means unlimited.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GroupLimits {
    pub download_rate: u64,
    pub upload_rate: u64,
    pub max_connections: usize,
}

impl Group {
    pub fn new(limits: GroupLimits) -> Arc<Self> {
        Arc::new(Self {
            download: Bucket::new(limits.download_rate),
            upload: Bucket::new(limits.upload_rate),
            connections: Counter::new(limits.max_connections),
        })
    }

    pub fn set(&self, limits: GroupLimits) {
        self.download.set_rate(limits.download_rate);
        self.upload.set_rate(limits.upload_rate);
        self.connections.set_limit(limits.max_connections);
    }

    pub fn limits(&self) -> GroupLimits {
        GroupLimits {
            download_rate: self.download.rate(),
            upload_rate: self.upload.rate(),
            max_connections: self.connections.limit(),
        }
    }
}

/// A torrent's own limits. 0 or `None` means unlimited (the session's and group's limits
/// still apply).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TorrentLimits {
    pub download_rate: u64,
    pub upload_rate: u64,
    /// Overrides the session's per-torrent peer maximum when set.
    pub max_connections: Option<usize>,
    /// This torrent's own cap on piece data in memory, below the session's.
    pub memory: Option<u64>,
}

/// Which way bytes go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Download,
    Upload,
}

/// The chain of buckets a torrent entry's connections draw from. The space and group can
/// change while connections are open.
#[derive(Debug)]
pub struct Throttle {
    session_down: Arc<Bucket>,
    session_up: Arc<Bucket>,
    space: Mutex<Option<Arc<Group>>>,
    group: Mutex<Option<Arc<Group>>>,
    pub(crate) torrent_down: Bucket,
    pub(crate) torrent_up: Bucket,
}

impl Throttle {
    pub(crate) fn new(
        session_down: Arc<Bucket>,
        session_up: Arc<Bucket>,
        space: Option<Arc<Group>>,
        group: Option<Arc<Group>>,
        limits: TorrentLimits,
    ) -> Self {
        Self {
            session_down,
            session_up,
            space: Mutex::new(space),
            group: Mutex::new(group),
            torrent_down: Bucket::new(limits.download_rate),
            torrent_up: Bucket::new(limits.upload_rate),
        }
    }

    pub(crate) fn set_group(&self, group: Option<Arc<Group>>) {
        *self.group.lock().expect("throttle lock poisoned") = group;
    }

    pub(crate) fn group(&self) -> Option<Arc<Group>> {
        self.group.lock().expect("throttle lock poisoned").clone()
    }

    pub(crate) fn set_space(&self, space: Option<Arc<Group>>) {
        *self.space.lock().expect("throttle lock poisoned") = space;
    }

    pub(crate) fn space(&self) -> Option<Arc<Group>> {
        self.space.lock().expect("throttle lock poisoned").clone()
    }

    /// Reserve `n` bytes at every level and return the longest wait.
    pub(crate) fn reserve(&self, dir: Direction, n: u64) -> Duration {
        let shared = [self.space(), self.group()];
        let (session, torrent) = match dir {
            Direction::Download => (&self.session_down, &self.torrent_down),
            Direction::Upload => (&self.session_up, &self.torrent_up),
        };
        let mut wait = session.reserve(n).max(torrent.reserve(n));
        for g in shared.iter().flatten() {
            let bucket = match dir {
                Direction::Download => &g.download,
                Direction::Upload => &g.upload,
            };
            wait = wait.max(bucket.reserve(n));
        }
        wait
    }

    /// Reserve `n` bytes and sleep until they may be used.
    pub(crate) async fn take(&self, dir: Direction, n: u64) {
        let wait = self.reserve(dir, n);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_never_waits() {
        let b = Bucket::new(0);
        assert_eq!(b.reserve(u64::MAX / 2), Duration::ZERO);
        assert!(b.try_take(1_000_000_000));
    }

    #[test]
    fn bursts_then_paces() {
        let b = Bucket::new(1000);
        let now = Instant::now();
        // 250 bytes of burst are free.
        assert_eq!(b.reserve_at(250, now), Duration::ZERO);
        // The next 500 bytes take half a second at 1000 B/s.
        let wait = b.reserve_at(500, now);
        assert!((wait.as_secs_f64() - 0.5).abs() < 0.01, "{wait:?}");
        // Time passing pays the debt down.
        let later = now + Duration::from_secs(2);
        assert_eq!(b.reserve_at(100, later), Duration::ZERO);
    }

    #[test]
    fn try_take_refuses_instead_of_waiting() {
        let b = Bucket::new(4); // four per second; the burst allows one
        assert!(b.try_take(1));
        assert!(!b.try_take(1));
    }

    #[test]
    fn counters_hand_out_and_return_slots() {
        let c = Counter::new(2);
        let a = c.try_acquire().unwrap();
        let _b = c.try_acquire().unwrap();
        assert!(c.try_acquire().is_none());
        drop(a);
        assert!(c.try_acquire().is_some());
        c.set_limit(0);
        let many: Vec<Slot> = (0..10).filter_map(|_| c.try_acquire()).collect();
        assert_eq!(many.len(), 10, "0 is unlimited");
        assert_eq!(c.used(), 11);
    }

    #[test]
    fn the_tightest_level_wins() {
        let session = Arc::new(Bucket::new(1_000_000));
        let up = Arc::new(Bucket::new(0));
        let group = Group::new(GroupLimits {
            download_rate: 1000,
            ..GroupLimits::default()
        });
        let t = Throttle::new(session, up, None, Some(group), TorrentLimits::default());
        t.reserve(Direction::Download, 250);
        let wait = t.reserve(Direction::Download, 1000);
        assert!(wait >= Duration::from_millis(900), "{wait:?}");
        assert_eq!(t.reserve(Direction::Upload, 1_000_000), Duration::ZERO);
        t.set_group(None);
        assert!(t.reserve(Direction::Download, 1000) < Duration::from_millis(10));
    }

    #[test]
    fn a_space_limits_its_entries() {
        let session = Arc::new(Bucket::new(0));
        let up = Arc::new(Bucket::new(0));
        let space = Group::new(GroupLimits {
            upload_rate: 1000,
            ..GroupLimits::default()
        });
        let a = Throttle::new(
            session.clone(),
            up.clone(),
            Some(space.clone()),
            None,
            TorrentLimits::default(),
        );
        let b = Throttle::new(session, up, Some(space), None, TorrentLimits::default());
        // Two entries in one space share its bucket.
        a.reserve(Direction::Upload, 250);
        let wait = b.reserve(Direction::Upload, 1000);
        assert!(wait >= Duration::from_millis(900), "{wait:?}");
        assert_eq!(a.reserve(Direction::Download, 1_000_000), Duration::ZERO);
        b.set_space(None);
        assert!(b.reserve(Direction::Upload, 1000) < Duration::from_millis(10));
    }
}

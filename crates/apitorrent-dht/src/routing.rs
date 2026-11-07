//! The routing table of [BEP 5]: k-buckets over the XOR metric, with replacement caches.
//!
//! Buckets are stored as a list indexed by the length of the prefix a node shares with our
//! own ID. Bucket `i` holds nodes sharing exactly `i` bits, and the last bucket holds every
//! node sharing at least that many. Only the last bucket (the one our own ID falls in) can
//! split, which is the tree BEP 5 describes.
//!
//! Only nodes that answered one of our queries enter the table, so a spoofed source address
//! can't place a node in it. A full bucket keeps a small cache of replacements, and asks
//! the caller to ping its questionable nodes; a node that stops answering is replaced from
//! the cache.
//!
//! [BEP 5]: https://www.bittorrent.org/beps/bep_0005.html

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use crate::id::{ID_BITS, NodeId, is_exempt};
use crate::krpc::{NodeInfo, canonical};

/// BEP 5's bucket size.
pub const K: usize = 8;

/// A node that has answered within this long is good (BEP 5: 15 minutes).
pub const GOOD_FOR: Duration = Duration::from_secs(15 * 60);

/// A bucket that hasn't changed for this long is refreshed (BEP 5: 15 minutes).
pub const REFRESH_AFTER: Duration = Duration::from_secs(15 * 60);

/// After this many unanswered queries in a row, a node is bad.
pub const MAX_FAILURES: u8 = 3;

/// Don't ping the same questionable node more often than this.
const PING_INTERVAL: Duration = Duration::from_secs(60);

/// One node in the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: NodeId,
    pub addr: SocketAddr,
    /// When it last answered one of our queries.
    pub last_response: Instant,
    /// Unanswered queries since its last answer.
    pub failures: u8,
    /// When we last pinged it to check that it's alive.
    pub last_ping: Option<Instant>,
}

/// How a node stands, from BEP 5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Good,
    Questionable,
    Bad,
}

impl Entry {
    pub fn status(&self, now: Instant) -> Status {
        if self.failures >= MAX_FAILURES {
            Status::Bad
        } else if self.failures == 0 && now.saturating_duration_since(self.last_response) < GOOD_FOR
        {
            Status::Good
        } else {
            Status::Questionable
        }
    }

    pub fn info(&self) -> NodeInfo {
        NodeInfo {
            id: self.id,
            addr: self.addr,
        }
    }
}

#[derive(Debug, Clone)]
struct Bucket {
    nodes: Vec<Entry>,
    replacements: VecDeque<Entry>,
    last_changed: Instant,
}

impl Bucket {
    fn new(now: Instant) -> Self {
        Self {
            nodes: Vec::with_capacity(K),
            replacements: VecDeque::new(),
            last_changed: now,
        }
    }
}

/// What [`RoutingTable::insert`] did with a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertOutcome {
    /// The node is new to the table.
    Added,
    /// The node was already there; its liveness was refreshed.
    Updated,
    /// The bucket is full. The node went to the replacement cache, and the caller should
    /// ping these questionable nodes so that dead ones can be replaced.
    Full { ping: Vec<NodeInfo> },
    /// The node can't enter the table: it's our own ID, its address conflicts with a node
    /// already there, or (with IP restriction on) its IP already has a node.
    Rejected,
}

/// The routing table for one address family.
#[derive(Debug, Clone)]
pub struct RoutingTable {
    own_id: NodeId,
    buckets: Vec<Bucket>,
    /// Allow one node per IP address, except for local and private addresses.
    restrict_ips: bool,
    /// IPs of nodes in the table (not the replacement caches).
    ips: HashMap<IpAddr, usize>,
}

impl RoutingTable {
    pub fn new(own_id: NodeId, restrict_ips: bool, now: Instant) -> Self {
        Self {
            own_id,
            buckets: vec![Bucket::new(now)],
            restrict_ips,
            ips: HashMap::new(),
        }
    }

    pub fn own_id(&self) -> NodeId {
        self.own_id
    }

    /// The number of nodes in the table, not counting replacement caches.
    pub fn len(&self) -> usize {
        self.buckets.iter().map(|b| b.nodes.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.buckets.iter().all(|b| b.nodes.is_empty())
    }

    /// The number of good nodes.
    pub fn good_len(&self, now: Instant) -> usize {
        self.iter()
            .filter(|e| e.status(now) == Status::Good)
            .count()
    }

    /// The number of buckets (the table's depth).
    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// Every node in the table.
    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.buckets.iter().flat_map(|b| b.nodes.iter())
    }

    fn bucket_index(&self, id: &NodeId) -> usize {
        self.own_id
            .common_prefix_len(id)
            .min(self.buckets.len() - 1)
    }

    /// Whether the table holds this ID.
    pub fn contains(&self, id: &NodeId) -> bool {
        let b = &self.buckets[self.bucket_index(id)];
        b.nodes.iter().any(|e| e.id == *id)
    }

    /// Whether a node with this ID could join the table now, without displacing anyone:
    /// its bucket has room, or can split. Used to decide whether a stranger that queried us
    /// is worth a verifying ping.
    pub fn has_room_for(&self, id: &NodeId) -> bool {
        if *id == self.own_id {
            return false;
        }
        let idx = self.bucket_index(id);
        let bucket = &self.buckets[idx];
        !bucket.nodes.iter().any(|e| e.id == *id)
            && (bucket.nodes.len() < K
                || (idx == self.buckets.len() - 1 && self.buckets.len() < ID_BITS))
    }

    fn ip_taken(&self, addr: &SocketAddr) -> bool {
        let ip = canonical(*addr).ip();
        self.restrict_ips && !is_exempt(ip) && self.ips.contains_key(&ip)
    }

    /// Record that `node` answered one of our queries: add it, refresh it, or cache it as a
    /// replacement.
    pub fn insert(&mut self, node: NodeInfo, now: Instant) -> InsertOutcome {
        if node.id == self.own_id {
            return InsertOutcome::Rejected;
        }
        // A known address answering with a new ID has changed its ID, as BEP 42 makes nodes
        // do once they learn their external address. It answered a query we sent to that
        // address, so the address really is its own: the old entry goes.
        let moved = self
            .iter()
            .find(|e| e.addr == node.addr && e.id != node.id)
            .map(|e| e.id);
        if let Some(old) = moved {
            self.remove(&old);
        }
        loop {
            let idx = self.bucket_index(&node.id);
            let bucket = &mut self.buckets[idx];

            if let Some(e) = bucket.nodes.iter_mut().find(|e| e.id == node.id) {
                // A known ID answering from a new address could be a hijack attempt: keep
                // the address we verified first.
                if e.addr != node.addr {
                    return InsertOutcome::Rejected;
                }
                e.last_response = now;
                e.failures = 0;
                bucket.last_changed = now;
                return InsertOutcome::Updated;
            }
            if bucket.nodes.iter().any(|e| e.addr == node.addr) {
                return InsertOutcome::Rejected;
            }

            let entry = Entry {
                id: node.id,
                addr: node.addr,
                last_response: now,
                failures: 0,
                last_ping: None,
            };

            if bucket.nodes.len() < K {
                if self.ip_taken(&node.addr) {
                    return InsertOutcome::Rejected;
                }
                let bucket = &mut self.buckets[idx];
                bucket.nodes.push(entry);
                bucket.last_changed = now;
                track_ip(&mut self.ips, &node.addr);
                return InsertOutcome::Added;
            }

            // Full. A bad node gives up its place first.
            if let Some(pos) = bucket
                .nodes
                .iter()
                .position(|e| e.status(now) == Status::Bad)
            {
                if self.ip_taken(&node.addr) {
                    return InsertOutcome::Rejected;
                }
                let bucket = &mut self.buckets[idx];
                let old = std::mem::replace(&mut bucket.nodes[pos], entry);
                bucket.last_changed = now;
                untrack_ip(&mut self.ips, &old.addr);
                track_ip(&mut self.ips, &node.addr);
                return InsertOutcome::Added;
            }

            // The bucket holding our own ID splits, and we try again.
            if idx == self.buckets.len() - 1 && self.buckets.len() < ID_BITS {
                self.split_last(now);
                continue;
            }

            // Cache it as a replacement, most recent last.
            let bucket = &mut self.buckets[idx];
            bucket
                .replacements
                .retain(|r| r.id != node.id && r.addr != node.addr);
            if bucket.replacements.len() >= K {
                bucket.replacements.pop_front();
            }
            bucket.replacements.push_back(entry);
            let ping = bucket
                .nodes
                .iter_mut()
                .filter(|e| {
                    e.status(now) == Status::Questionable
                        && e.last_ping
                            .is_none_or(|t| now.saturating_duration_since(t) >= PING_INTERVAL)
                })
                .map(|e| {
                    e.last_ping = Some(now);
                    e.info()
                })
                .collect();
            return InsertOutcome::Full { ping };
        }
    }

    /// Split the last bucket: nodes that share one more bit with us move to a new bucket.
    fn split_last(&mut self, now: Instant) {
        let depth = self.buckets.len(); // nodes with prefix >= depth move
        let own = self.own_id;
        let last = self.buckets.last_mut().expect("at least one bucket");
        let (stay, go): (Vec<_>, Vec<_>) = last
            .nodes
            .drain(..)
            .partition(|e| own.common_prefix_len(&e.id) < depth);
        let (stay_r, go_r): (VecDeque<_>, VecDeque<_>) = last
            .replacements
            .drain(..)
            .partition(|e| own.common_prefix_len(&e.id) < depth);
        last.nodes = stay;
        last.replacements = stay_r;
        last.last_changed = now;
        let mut new = Bucket::new(now);
        new.nodes = go;
        new.replacements = go_r;
        self.buckets.push(new);
    }

    /// Record that a query to `addr` went unanswered. A node that becomes bad is replaced
    /// from its bucket's cache when one is available.
    pub fn failed(&mut self, id: Option<NodeId>, addr: SocketAddr, now: Instant) {
        let idx = match id {
            Some(id) => self.bucket_index(&id),
            None => match self.find_by_addr(addr) {
                Some(i) => i,
                None => return,
            },
        };
        let bucket = &mut self.buckets[idx];
        let Some(pos) = bucket.nodes.iter().position(|e| e.addr == addr) else {
            bucket.replacements.retain(|r| r.addr != addr);
            return;
        };
        let e = &mut bucket.nodes[pos];
        e.failures = e.failures.saturating_add(1);
        if e.status(now) != Status::Bad {
            return;
        }
        // Promote the most recent replacement whose IP doesn't conflict.
        let old_ip = canonical(addr).ip();
        let Self {
            buckets,
            ips,
            restrict_ips,
            ..
        } = self;
        let bucket = &mut buckets[idx];
        while let Some(r) = bucket.replacements.pop_back() {
            let ip = canonical(r.addr).ip();
            if *restrict_ips && !is_exempt(ip) && ip != old_ip && ips.contains_key(&ip) {
                continue; // conflicts with a node already in the table
            }
            let old = std::mem::replace(&mut bucket.nodes[pos], r);
            bucket.last_changed = now;
            untrack_ip(ips, &old.addr);
            track_ip(ips, &bucket.nodes[pos].addr);
            return;
        }
    }

    fn find_by_addr(&self, addr: SocketAddr) -> Option<usize> {
        self.buckets
            .iter()
            .position(|b| b.nodes.iter().any(|e| e.addr == addr))
    }

    /// Remove a node outright, as when it turns out to be read-only or misbehaving.
    pub fn remove(&mut self, id: &NodeId) {
        let idx = self.bucket_index(id);
        let bucket = &mut self.buckets[idx];
        if let Some(pos) = bucket.nodes.iter().position(|e| e.id == *id) {
            let old = bucket.nodes.remove(pos);
            if let Some(r) = bucket.replacements.pop_back() {
                bucket.nodes.push(r.clone());
                track_ip(&mut self.ips, &r.addr);
            }
            untrack_ip(&mut self.ips, &old.addr);
        }
    }

    /// Up to `n` non-bad nodes, closest to `target` first.
    pub fn closest(&self, target: &NodeId, n: usize, now: Instant) -> Vec<NodeInfo> {
        let mut all: Vec<&Entry> = self
            .iter()
            .filter(|e| e.status(now) != Status::Bad)
            .collect();
        all.sort_unstable_by_key(|e| e.id.distance(target));
        all.into_iter().take(n).map(Entry::info).collect()
    }

    /// Lookup targets for buckets that haven't changed in [`REFRESH_AFTER`]. Each returned
    /// bucket is marked as refreshed, so it isn't returned again straight away.
    pub fn refresh_targets(&mut self, now: Instant, rng: &mut impl rand::Rng) -> Vec<NodeId> {
        let own = self.own_id;
        let mut out = Vec::new();
        for (i, b) in self.buckets.iter_mut().enumerate() {
            if now.saturating_duration_since(b.last_changed) >= REFRESH_AFTER {
                b.last_changed = now;
                out.push(own.random_with_prefix(i, rng));
            }
        }
        out
    }

    /// Questionable nodes due a liveness ping, at most `max`. Each is marked as pinged.
    pub fn due_pings(&mut self, now: Instant, max: usize) -> Vec<NodeInfo> {
        let mut out = Vec::new();
        for b in &mut self.buckets {
            for e in &mut b.nodes {
                if out.len() >= max {
                    return out;
                }
                if e.status(now) == Status::Questionable
                    && e.last_ping
                        .is_none_or(|t| now.saturating_duration_since(t) >= PING_INTERVAL)
                {
                    e.last_ping = Some(now);
                    out.push(e.info());
                }
            }
        }
        out
    }

    /// Good nodes, for persistence: up to `max`, those that answered most recently first.
    pub fn good_nodes(&self, now: Instant, max: usize) -> Vec<NodeInfo> {
        let mut good: Vec<&Entry> = self
            .iter()
            .filter(|e| e.status(now) == Status::Good)
            .collect();
        good.sort_unstable_by_key(|e| std::cmp::Reverse(e.last_response));
        good.into_iter().take(max).map(Entry::info).collect()
    }

    /// Rebuild the table around a new own ID (after a BEP 42 ID change), keeping every
    /// node's state.
    pub fn rebuild(&mut self, new_id: NodeId, now: Instant) {
        let old: Vec<Entry> = self.iter().cloned().collect();
        *self = Self::new(new_id, self.restrict_ips, now);
        for e in old {
            if self.insert(e.info(), now) == InsertOutcome::Added {
                let idx = self.bucket_index(&e.id);
                if let Some(slot) = self.buckets[idx].nodes.iter_mut().find(|x| x.id == e.id) {
                    *slot = e;
                }
            }
        }
    }
}

fn track_ip(ips: &mut HashMap<IpAddr, usize>, addr: &SocketAddr) {
    *ips.entry(canonical(*addr).ip()).or_default() += 1;
}

fn untrack_ip(ips: &mut HashMap<IpAddr, usize>, addr: &SocketAddr) {
    let ip = canonical(*addr).ip();
    if let Some(n) = ips.get_mut(&ip) {
        *n -= 1;
        if *n == 0 {
            ips.remove(&ip);
        }
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    fn node(id: NodeId, port: u16) -> NodeInfo {
        NodeInfo {
            id,
            addr: SocketAddr::from(([8, 8, (port >> 8) as u8, port as u8], port)),
        }
    }

    fn table(restrict: bool) -> (RoutingTable, Instant, StdRng) {
        let mut rng = StdRng::seed_from_u64(3);
        let now = Instant::now();
        (
            RoutingTable::new(NodeId::random(&mut rng), restrict, now),
            now,
            rng,
        )
    }

    #[test]
    fn rejects_own_id_and_moves() {
        let (mut t, now, mut rng) = table(false);
        let own = t.own_id();
        assert_eq!(t.insert(node(own, 1), now), InsertOutcome::Rejected);
        let id = NodeId::random(&mut rng);
        assert_eq!(t.insert(node(id, 1), now), InsertOutcome::Added);
        assert_eq!(t.insert(node(id, 1), now), InsertOutcome::Updated);
        assert_eq!(t.insert(node(id, 2), now), InsertOutcome::Rejected);
        assert_eq!(t.len(), 1);
        // The same address with a new ID replaces the old entry.
        let id2 = NodeId::random(&mut rng);
        assert_eq!(t.insert(node(id2, 1), now), InsertOutcome::Added);
        assert!(!t.contains(&id) && t.contains(&id2));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn far_bucket_fills_then_caches() {
        let (mut t, now, mut rng) = table(false);
        let own = t.own_id();
        // Nodes sharing no prefix all land in bucket 0, which stops splitting once deeper
        // buckets exist. Force a split first by adding a close node.
        t.insert(node(own.random_with_prefix(10, &mut rng), 999), now);
        for i in 0..K as u16 {
            let r = t.insert(node(own.random_with_prefix(0, &mut rng), i), now);
            assert_eq!(r, InsertOutcome::Added, "{i}");
        }
        let later = now + GOOD_FOR + Duration::from_secs(1);
        let r = t.insert(node(own.random_with_prefix(0, &mut rng), 100), later);
        let InsertOutcome::Full { ping } = r else {
            panic!("{r:?}")
        };
        assert_eq!(ping.len(), K, "all questionable after 15 minutes");
        // Every far node fails three times: the replacement takes the first slot freed.
        let far: Vec<_> = t
            .iter()
            .filter(|e| own.common_prefix_len(&e.id) == 0)
            .map(Entry::info)
            .collect();
        for _ in 0..MAX_FAILURES {
            t.failed(Some(far[0].id), far[0].addr, later);
        }
        assert!(!t.contains(&far[0].id));
        assert!(t.iter().any(|e| e.addr.port() == 100));
    }

    #[test]
    fn splits_down_to_own_id() {
        let (mut t, now, mut rng) = table(false);
        let own = t.own_id();
        for depth in 0..40 {
            for j in 0..K as u16 {
                t.insert(
                    node(
                        own.random_with_prefix(depth, &mut rng),
                        depth as u16 * 16 + j,
                    ),
                    now,
                );
            }
        }
        assert!(t.bucket_count() >= 39, "{}", t.bucket_count());
        assert_eq!(t.len(), 40 * K);
        // Every bucket holds at most K.
        for b in &t.buckets {
            assert!(b.nodes.len() <= K);
        }
        let target = own.random_with_prefix(30, &mut rng);
        let c = t.closest(&target, K, now);
        assert_eq!(c.len(), K);
        for w in c.windows(2) {
            assert!(w[0].id.distance(&target) <= w[1].id.distance(&target));
        }
    }

    #[test]
    fn ip_restriction() {
        let (mut t, now, mut rng) = table(true);
        let a = NodeInfo {
            id: NodeId::random(&mut rng),
            addr: "8.8.8.8:1".parse().unwrap(),
        };
        let b = NodeInfo {
            id: NodeId::random(&mut rng),
            addr: "8.8.8.8:2".parse().unwrap(),
        };
        assert_eq!(t.insert(a, now), InsertOutcome::Added);
        assert_eq!(t.insert(b, now), InsertOutcome::Rejected);
        // Loopback is exempt, so a local test network works.
        for port in 1..5 {
            let n = NodeInfo {
                id: NodeId::random(&mut rng),
                addr: SocketAddr::from(([127, 0, 0, 1], port)),
            };
            assert_eq!(t.insert(n, now), InsertOutcome::Added);
        }
        t.remove(&a.id);
        assert_eq!(t.insert(b, now), InsertOutcome::Added);
    }

    #[test]
    fn refresh_and_pings() {
        let (mut t, now, mut rng) = table(false);
        let id = NodeId::random(&mut rng);
        t.insert(node(id, 1), now);
        assert!(t.refresh_targets(now, &mut rng).is_empty());
        let later = now + REFRESH_AFTER;
        assert_eq!(t.refresh_targets(later, &mut rng).len(), 1);
        assert!(t.refresh_targets(later, &mut rng).is_empty());
        assert!(t.due_pings(now, 10).is_empty());
        assert_eq!(t.due_pings(later, 10).len(), 1);
        assert!(
            t.due_pings(later, 10).is_empty(),
            "not again within a minute"
        );
    }

    #[test]
    fn rebuild_keeps_nodes() {
        let (mut t, now, mut rng) = table(false);
        for i in 0..50 {
            t.insert(node(NodeId::random(&mut rng), i), now);
        }
        let before = t.len();
        t.rebuild(NodeId::random(&mut rng), now);
        // Bucket shapes differ around a new ID, so a few may not fit; most must.
        assert!(t.len() >= before / 2 && t.len() <= before);
        assert_eq!(t.good_nodes(now, 1000).len(), t.len());
    }
}

//! The DHT node as a sans-IO state machine.
//!
//! [`Node`] owns everything a DHT node knows: its ID, one routing table per address family
//! ([BEP 32]), the write-token secrets, the peer store, the outstanding queries and the
//! running lookups. It does no I/O and reads no clock. The caller feeds it datagrams
//! ([`Node::handle_datagram`]) and timer expiries ([`Node::handle_timeout`]), and drains
//! what it wants sent ([`Node::poll_transmit`]) and what happened ([`Node::poll_event`]).
//! [`Node::poll_timeout`] says when to call back. The tokio driver in [`crate::Dht`] is a
//! thin loop around this, and tests drive many nodes through an in-memory network.
//!
//! Lookups are iterative ([BEP 5]): up to [`ALPHA`] queries in flight, closing in on the
//! target until the [`K`] closest nodes found have all answered. A query that hasn't been
//! answered within [`SLOW_AFTER`] stops counting against the in-flight limit, so one dead
//! node doesn't stall a lookup for the full timeout.
//!
//! [BEP 5]: https://www.bittorrent.org/beps/bep_0005.html
//! [BEP 32]: https://www.bittorrent.org/beps/bep_0032.html

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use tracing::{debug, trace};

use crate::id::{NodeId, is_exempt};
use crate::krpc::{self, Body, Message, NodeInfo, Query, Response, Want, canonical, error_code};
use crate::rate::RateLimiter;
use crate::routing::{InsertOutcome, K, RoutingTable};
use crate::state::{MAX_SAVED_NODES, PersistedState};
use crate::store::{Bloom, PeerStore};
use crate::token::TokenSecrets;

/// Queries in flight per lookup and address family (BEP 5's alpha).
pub const ALPHA: usize = 3;

/// After this long without an answer, a query no longer counts against [`ALPHA`]. Its
/// answer is still accepted until the full query timeout.
pub const SLOW_AFTER: Duration = Duration::from_millis(1500);

/// How many of the closest candidates a lookup remembers per address family.
const MAX_CANDIDATES: usize = 64;
/// Queries one lookup may send per address family. A lookup converges in a few dozen; the
/// cap stops hostile nodes from stretching one out by feeding it ever-closer fake nodes.
const MAX_QUERIES_PER_LOOKUP: usize = 200;
/// Outstanding queries across the whole node.
const MAX_PENDING: usize = 4096;
/// Lookups running at once. More are finished at once with no result.
const MAX_LOOKUPS: usize = 256;
/// Internal (bootstrap and refresh) lookups running at once.
const MAX_INTERNAL_LOOKUPS: usize = 4;
/// Distinct peers a single lookup reports.
const MAX_PEERS_PER_LOOKUP: usize = 4096;
/// Peers we put in one `get_peers` response, which keeps it well inside one datagram.
const MAX_VALUES_PER_RESPONSE: usize = 50;
/// How often periodic maintenance runs.
const MAINTENANCE_EVERY: Duration = Duration::from_secs(1);
/// How often expired peers are dropped from the store.
const STORE_EXPIRE_EVERY: Duration = Duration::from_secs(60);
/// How long to wait before bootstrapping again when a table is still sparse.
const BOOTSTRAP_RETRY: Duration = Duration::from_secs(30);
/// A table with fewer nodes than this (one full bucket) bootstraps again. A node that
/// joined while the network was busy may have learned only a few neighbours, and bucket
/// refresh alone would take many minutes to fill the rest.
const SPARSE_TABLE: usize = 8;
/// Strangers we pinged recently, so a chatty querier isn't pinged on every query.
const MAX_RECENT_VERIFY: usize = 1024;
const VERIFY_AGAIN_AFTER: Duration = Duration::from_secs(5 * 60);
/// External-address votes remembered per family (BEP 42).
const MAX_IP_VOTES: usize = 32;
/// Votes needed before we act on an external address.
const MIN_IP_VOTES: usize = 4;
/// A table smaller than this gets help from the other family's `nodes6`/`nodes`.
const SMALL_TABLE: usize = 32;

/// Settings for a [`Node`].
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)] // independent switches, each from a BEP
pub struct NodeConfig {
    /// The node ID to start with. A random one when `None`; with `secure_id` it is
    /// replaced by a BEP 42 ID once the external address is known.
    pub id: Option<NodeId>,
    /// Run the IPv4 DHT.
    pub ipv4: bool,
    /// Run the IPv6 DHT (BEP 32).
    pub ipv6: bool,
    /// BEP 43 read-only mode: answer no queries, and mark our queries `ro` so other nodes
    /// leave us out of their routing tables.
    pub read_only: bool,
    /// BEP 42: switch to an ID derived from our external IP once enough nodes agree on it.
    pub secure_id: bool,
    /// BEP 42: admit to the routing table only nodes whose ID matches their IP.
    pub enforce_node_id: bool,
    /// One routing-table node per IP address (local and private addresses exempt).
    pub restrict_routing_ips: bool,
    /// Outgoing queries per second, on average.
    pub max_queries_per_second: u32,
    /// Outgoing queries allowed in a burst.
    pub query_burst: u32,
    /// Responses to incoming queries per second. Beyond this, queries are dropped, so the
    /// node can't be used as a traffic amplifier.
    pub max_responses_per_second: u32,
    /// Info-hashes the peer store holds.
    pub max_info_hashes: usize,
    /// Peers stored per info-hash.
    pub max_peers_per_info_hash: usize,
    /// How long to wait for an answer to a query.
    pub query_timeout: Duration,
    /// The `v` key we send: two letters of client id and two bytes of version.
    pub version: Option<Vec<u8>>,
    /// Seed for the node's random numbers. `None` seeds from the OS; tests pass a value
    /// for reproducible runs.
    pub rng_seed: Option<u64>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            id: None,
            ipv4: true,
            ipv6: true,
            read_only: false,
            secure_id: true,
            enforce_node_id: false,
            restrict_routing_ips: true,
            max_queries_per_second: 200,
            query_burst: 100,
            max_responses_per_second: 500,
            max_info_hashes: 4096,
            max_peers_per_info_hash: 256,
            query_timeout: Duration::from_secs(5),
            version: Some(b"AT\x00\x01".to_vec()),
            rng_seed: None,
        }
    }
}

/// Identifies a lookup started with [`Node::get_peers`] or [`Node::find_node`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LookupId(pub u64);

/// Announce our own presence at the end of a `get_peers` lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnnounceSpec {
    /// The port peers should connect to; `None` sends `implied_port`, so the DHT socket's
    /// port is used (useful behind NAT, with uTP on the same port).
    pub port: Option<u16>,
    /// BEP 33: announce as a seed.
    pub seed: bool,
}

/// A datagram to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transmit {
    pub to: SocketAddr,
    pub data: Vec<u8>,
}

/// Something the caller should know about.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// New peers for a `get_peers` lookup. Each address is reported once per lookup.
    Peers {
        lookup: LookupId,
        peers: Vec<SocketAddr>,
    },
    /// A lookup finished.
    LookupDone {
        lookup: LookupId,
        outcome: LookupOutcome,
    },
    /// Our node ID changed (BEP 42). Saved state should be refreshed.
    IdChanged { id: NodeId },
}

/// The result of a finished lookup.
#[derive(Debug, Clone, PartialEq)]
pub struct LookupOutcome {
    /// Nodes that answered during the lookup.
    pub responded: usize,
    /// Nodes we sent `announce_peer` to.
    pub announced_to: usize,
    /// The closest nodes that answered, closest first (both families).
    pub closest: Vec<NodeInfo>,
    /// BEP 33 scrape estimates, if asked for and any node supplied bloom filters.
    pub scrape: Option<ScrapeEstimate>,
}

/// Swarm size estimates from BEP 33 bloom filters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrapeEstimate {
    pub seeds: f64,
    pub peers: f64,
}

/// Counters and sizes, for display and debugging.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DhtStats {
    pub node_id: [u8; 20],
    pub read_only: bool,
    pub ipv4: Option<TableStats>,
    pub ipv6: Option<TableStats>,
    /// Our external addresses as the network reports them (BEP 42 votes).
    pub external_ipv4: Option<IpAddr>,
    pub external_ipv6: Option<IpAddr>,
    pub stored_info_hashes: usize,
    pub stored_peers: usize,
    pub active_lookups: usize,
    pub pending_queries: usize,
    pub queries_sent: u64,
    pub queries_received: u64,
    pub responses_received: u64,
    pub errors_received: u64,
    pub timeouts: u64,
    /// Datagrams we couldn't decode.
    pub malformed: u64,
    /// Incoming queries dropped by the response rate limit.
    pub rate_limited: u64,
    pub announces_stored: u64,
}

/// One routing table's size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TableStats {
    pub nodes: usize,
    pub good: usize,
    pub buckets: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingKind {
    Ping,
    FindNode,
    GetPeers,
    Announce,
}

#[derive(Debug, Clone)]
struct Pending {
    to: SocketAddr,
    expect_id: Option<NodeId>,
    kind: PendingKind,
    lookup: Option<(LookupId, usize)>,
    /// A bootstrap router: it answers, but doesn't belong in the routing table.
    bootstrap_seed: bool,
    slow: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CandidateState {
    Fresh,
    InFlight,
    Responded { token: Option<Vec<u8>> },
    Failed,
}

#[derive(Debug, Clone)]
struct Candidate {
    info: NodeInfo,
    distance: NodeId,
    state: CandidateState,
}

#[derive(Debug, Clone, Default)]
struct FamilyLookup {
    /// Sorted by distance to the target, closest first.
    candidates: Vec<Candidate>,
    /// Addresses to query whose IDs we don't know (bootstrap routers).
    seeds: Vec<SocketAddr>,
    /// Queries counting against alpha.
    in_flight: usize,
    /// All outstanding queries, slow ones included.
    outstanding: usize,
    queried: HashSet<SocketAddr>,
    done: bool,
}

#[derive(Debug, Clone)]
enum LookupKind {
    FindNode,
    GetPeers {
        announce: Option<AnnounceSpec>,
        scrape: bool,
    },
}

#[derive(Debug, Clone)]
struct Lookup {
    target: NodeId,
    kind: LookupKind,
    /// Bootstrap or refresh: no events.
    internal: bool,
    fams: [Option<FamilyLookup>; 2],
    peers_seen: HashSet<SocketAddr>,
    responded: usize,
    bloom_seeds: Bloom,
    bloom_peers: Bloom,
    got_bloom: bool,
}

/// The address family index: 0 for IPv4 (including IPv4-mapped), 1 for IPv6.
fn fam_of(addr: &SocketAddr) -> usize {
    usize::from(canonical(*addr).is_ipv6())
}

/// A DHT node. See the [module documentation](self).
#[derive(Debug)]
pub struct Node {
    cfg: NodeConfig,
    rng: StdRng,
    id: NodeId,
    tables: [Option<RoutingTable>; 2],
    tokens: TokenSecrets,
    store: PeerStore,
    pending: HashMap<u32, Pending>,
    /// (slow deadline, tid) and (timeout deadline, tid), in send order.
    slow_queue: VecDeque<(Instant, u32)>,
    timeout_queue: VecDeque<(Instant, u32)>,
    next_tid: u32,
    lookups: BTreeMap<LookupId, Lookup>,
    next_lookup: u64,
    query_limiter: RateLimiter,
    response_limiter: RateLimiter,
    /// Set when a lookup wanted to send but the rate limit said no.
    starved: bool,
    transmits: VecDeque<Transmit>,
    events: VecDeque<Event>,
    bootstrap_addrs: Vec<SocketAddr>,
    /// Nodes from saved state: candidates whenever a table is empty.
    saved_nodes: Vec<NodeInfo>,
    last_bootstrap: [Option<Instant>; 2],
    recent_verify: HashMap<SocketAddr, Instant>,
    ip_votes: [VecDeque<(IpAddr, IpAddr)>; 2],
    external_ip: [Option<IpAddr>; 2],
    next_maintenance: Instant,
    next_store_expire: Instant,
    stats: DhtStats,
}

impl Node {
    /// A new node. `saved` restores the ID and seeds the first bootstrap with known nodes.
    pub fn new(mut cfg: NodeConfig, saved: Option<&PersistedState>, now: Instant) -> Self {
        let mut rng = match cfg.rng_seed {
            Some(seed) => StdRng::seed_from_u64(seed),
            None => rand::make_rng::<StdRng>(),
        };
        if cfg
            .version
            .as_ref()
            .is_some_and(|v| v.len() > krpc::MAX_VERSION_LEN)
        {
            cfg.version = None;
        }
        let id = cfg
            .id
            .or_else(|| saved.map(|s| NodeId(s.id)))
            .unwrap_or_else(|| NodeId::random(&mut rng));
        let table = |on: bool| on.then(|| RoutingTable::new(id, cfg.restrict_routing_ips, now));
        let mut saved_nodes = Vec::new();
        if let Some(s) = saved {
            saved_nodes.extend(s.nodes_v4.iter().copied());
            saved_nodes.extend(s.nodes_v6.iter().copied());
        }
        Self {
            tables: [table(cfg.ipv4), table(cfg.ipv6)],
            tokens: TokenSecrets::new(&mut rng, now),
            store: PeerStore::new(id, cfg.max_info_hashes, cfg.max_peers_per_info_hash),
            pending: HashMap::new(),
            slow_queue: VecDeque::new(),
            timeout_queue: VecDeque::new(),
            next_tid: rng.random(),
            lookups: BTreeMap::new(),
            next_lookup: 1,
            query_limiter: RateLimiter::new(cfg.max_queries_per_second, cfg.query_burst, now),
            response_limiter: RateLimiter::new(
                cfg.max_responses_per_second,
                cfg.max_responses_per_second,
                now,
            ),
            starved: false,
            transmits: VecDeque::new(),
            events: VecDeque::new(),
            bootstrap_addrs: Vec::new(),
            saved_nodes,
            last_bootstrap: [None, None],
            recent_verify: HashMap::new(),
            ip_votes: [VecDeque::new(), VecDeque::new()],
            external_ip: [None, None],
            next_maintenance: now,
            next_store_expire: now + STORE_EXPIRE_EVERY,
            stats: DhtStats::default(),
            id,
            rng,
            cfg,
        }
    }

    pub fn id(&self) -> NodeId {
        self.id
    }

    /// The routing table for a family, if that family is enabled.
    pub fn table(&self, ipv6: bool) -> Option<&RoutingTable> {
        self.tables[usize::from(ipv6)].as_ref()
    }

    /// Add bootstrap addresses and bootstrap every enabled family from them (and from any
    /// saved nodes).
    pub fn add_bootstrap(&mut self, addrs: impl IntoIterator<Item = SocketAddr>, now: Instant) {
        for a in addrs {
            let a = canonical(a);
            if self.tables[fam_of(&a)].is_some() && !self.bootstrap_addrs.contains(&a) {
                self.bootstrap_addrs.push(a);
            }
        }
        self.bootstrap(now);
    }

    /// Start a `find_node` lookup for our own ID in every enabled family, seeded from the
    /// table, saved nodes and bootstrap routers.
    fn bootstrap(&mut self, now: Instant) {
        for fam in 0..2 {
            if self.tables[fam].is_some() {
                self.last_bootstrap[fam] = Some(now);
            }
        }
        let target = self.id;
        let lid = self.new_lookup(target, LookupKind::FindNode, true, now);
        if let Some(l) = self.lookups.get_mut(&lid) {
            for (fam, slot) in l.fams.iter_mut().enumerate() {
                if let Some(fl) = slot.as_mut() {
                    for n in self.saved_nodes.iter().filter(|n| fam_of(&n.addr) == fam) {
                        insert_candidate(fl, &target, *n, self.id);
                    }
                    fl.seeds = self
                        .bootstrap_addrs
                        .iter()
                        .copied()
                        .filter(|a| fam_of(a) == fam)
                        .collect();
                }
            }
        }
        self.drive_lookup(lid, now);
    }

    /// Ping a node, as when a peer advertises its DHT port (the wire protocol's `port`
    /// message). It enters the routing table if it answers.
    pub fn ping(&mut self, addr: SocketAddr, now: Instant) {
        let addr = canonical(addr);
        if self.tables[fam_of(&addr)].is_some() {
            self.send_query(addr, None, PendingKind::Ping, None, false, false, now);
        }
    }

    /// Start a `get_peers` lookup for `info_hash` in every enabled family. Peers arrive as
    /// [`Event::Peers`]; with `announce`, the closest nodes are told about us at the end.
    pub fn get_peers(
        &mut self,
        info_hash: NodeId,
        announce: Option<AnnounceSpec>,
        scrape: bool,
        now: Instant,
    ) -> LookupId {
        let lid = self.new_lookup(
            info_hash,
            LookupKind::GetPeers { announce, scrape },
            false,
            now,
        );
        self.drive_lookup(lid, now);
        lid
    }

    /// Start a `find_node` lookup for `target`.
    pub fn find_node(&mut self, target: NodeId, now: Instant) -> LookupId {
        let lid = self.new_lookup(target, LookupKind::FindNode, false, now);
        self.drive_lookup(lid, now);
        lid
    }

    /// Stop a lookup. No further events are reported for it.
    pub fn cancel(&mut self, lookup: LookupId) {
        self.lookups.remove(&lookup);
    }

    /// Whether a lookup is still running.
    pub fn is_running(&self, lookup: LookupId) -> bool {
        self.lookups.contains_key(&lookup)
    }

    fn new_lookup(
        &mut self,
        target: NodeId,
        kind: LookupKind,
        internal: bool,
        now: Instant,
    ) -> LookupId {
        let lid = LookupId(self.next_lookup);
        self.next_lookup += 1;
        let internal_running = self.lookups.values().filter(|l| l.internal).count();
        if self.lookups.len() >= MAX_LOOKUPS
            || (internal && internal_running >= MAX_INTERNAL_LOOKUPS)
        {
            if !internal {
                self.events.push_back(Event::LookupDone {
                    lookup: lid,
                    outcome: LookupOutcome {
                        responded: 0,
                        announced_to: 0,
                        closest: Vec::new(),
                        scrape: None,
                    },
                });
            }
            return lid;
        }
        let mut fams: [Option<FamilyLookup>; 2] = [None, None];
        for (fam, slot) in fams.iter_mut().enumerate() {
            if let Some(t) = &self.tables[fam] {
                let mut fl = FamilyLookup::default();
                for n in t.closest(&target, MAX_CANDIDATES, now) {
                    insert_candidate(&mut fl, &target, n, self.id);
                }
                if fl.candidates.is_empty() {
                    // An empty table (just started, or lost touch): fall back to the
                    // saved nodes and the bootstrap routers.
                    for n in self.saved_nodes.iter().filter(|n| fam_of(&n.addr) == fam) {
                        insert_candidate(&mut fl, &target, *n, self.id);
                    }
                    fl.seeds = self
                        .bootstrap_addrs
                        .iter()
                        .copied()
                        .filter(|a| fam_of(a) == fam)
                        .collect();
                }
                *slot = Some(fl);
            }
        }
        self.lookups.insert(
            lid,
            Lookup {
                target,
                kind,
                internal,
                fams,
                peers_seen: HashSet::new(),
                responded: 0,
                bloom_seeds: Bloom::default(),
                bloom_peers: Bloom::default(),
                got_bloom: false,
            },
        );
        lid
    }

    /// Send queries for a lookup until alpha are in flight, and finish it when done.
    fn drive_lookup(&mut self, lid: LookupId, now: Instant) {
        for fam in 0..2 {
            self.drive_family(lid, fam, now);
        }
        let Some(l) = self.lookups.get(&lid) else {
            return;
        };
        if l.fams.iter().flatten().all(|f| f.done) {
            self.finish_lookup(lid, now);
        }
    }

    fn drive_family(&mut self, lid: LookupId, fam: usize, now: Instant) {
        loop {
            let Some(l) = self.lookups.get_mut(&lid) else {
                return;
            };
            let target = l.target;
            let (kind, scrape) = match &l.kind {
                LookupKind::FindNode => (PendingKind::FindNode, false),
                LookupKind::GetPeers { scrape, .. } => (PendingKind::GetPeers, *scrape),
            };
            let Some(fl) = l.fams[fam].as_mut() else {
                return;
            };
            if fl.done {
                return;
            }
            if fl.in_flight >= ALPHA {
                break;
            }
            if fl.queried.len() >= MAX_QUERIES_PER_LOOKUP {
                // Stop asking; whatever is outstanding may still answer.
                fl.seeds.clear();
                for c in &mut fl.candidates {
                    if c.state == CandidateState::Fresh {
                        c.state = CandidateState::Failed;
                    }
                }
                break;
            }
            // Routers first, then the closest candidate not yet asked.
            let next = if let Some(addr) = fl.seeds.pop() {
                if fl.queried.contains(&addr) {
                    continue;
                }
                Some((addr, None, None))
            } else {
                let mut considered = 0;
                let mut found = None;
                for (i, c) in fl.candidates.iter().enumerate() {
                    if c.state == CandidateState::Failed {
                        continue;
                    }
                    if c.state == CandidateState::Fresh {
                        found = Some((c.info.addr, Some(c.info.id), Some(i)));
                        break;
                    }
                    considered += 1;
                    if considered >= K {
                        break;
                    }
                }
                found
            };
            let Some((addr, expect_id, idx)) = next else {
                break;
            };
            let sent = self.send_lookup_query(addr, expect_id, kind, target, scrape, lid, fam, now);
            let Some(fl) = self
                .lookups
                .get_mut(&lid)
                .and_then(|l| l.fams[fam].as_mut())
            else {
                return;
            };
            if !sent {
                if idx.is_none() {
                    fl.seeds.push(addr);
                }
                break;
            }
            fl.queried.insert(addr);
            fl.in_flight += 1;
            fl.outstanding += 1;
            if let Some(i) = idx {
                fl.candidates[i].state = CandidateState::InFlight;
            }
        }

        // Done when no router is left to ask and the K closest live candidates have all
        // answered (or there is nothing left to ask at all).
        let Some(fl) = self
            .lookups
            .get_mut(&lid)
            .and_then(|l| l.fams[fam].as_mut())
        else {
            return;
        };
        if !fl.seeds.is_empty() {
            return;
        }
        let mut considered = 0;
        let mut waiting = false;
        for c in &fl.candidates {
            match c.state {
                CandidateState::Failed => continue,
                CandidateState::Fresh | CandidateState::InFlight => {
                    waiting = true;
                    break;
                }
                CandidateState::Responded { .. } => {}
            }
            considered += 1;
            if considered >= K {
                break;
            }
        }
        // Without candidates, wait for answers from routers still outstanding.
        if fl
            .candidates
            .iter()
            .all(|c| c.state == CandidateState::Failed)
            && fl.outstanding > 0
        {
            waiting = true;
        }
        if !waiting {
            fl.done = true;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn send_lookup_query(
        &mut self,
        to: SocketAddr,
        expect_id: Option<NodeId>,
        kind: PendingKind,
        target: NodeId,
        scrape: bool,
        lid: LookupId,
        fam: usize,
        now: Instant,
    ) -> bool {
        let id = self.id;
        let want = self.want_for(fam);
        let query = match kind {
            PendingKind::GetPeers => Query::GetPeers {
                id,
                info_hash: target,
                want,
                noseed: false,
                scrape,
            },
            _ => Query::FindNode { id, target, want },
        };
        self.send_query_msg(
            to,
            expect_id,
            kind,
            query,
            Some((lid, fam)),
            expect_id.is_none(),
            false,
            now,
        )
    }

    /// The `want` to send on a family's socket: normally nothing (the socket's own family),
    /// but both when the other family's table is small and could use the help.
    fn want_for(&self, fam: usize) -> Want {
        let other = 1 - fam;
        match &self.tables[other] {
            Some(t) if t.len() < SMALL_TABLE => Want { v4: true, v6: true },
            _ => Want::NONE,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn send_query(
        &mut self,
        to: SocketAddr,
        expect_id: Option<NodeId>,
        kind: PendingKind,
        lookup: Option<(LookupId, usize)>,
        bootstrap_seed: bool,
        force: bool,
        now: Instant,
    ) -> bool {
        let id = self.id;
        let query = match kind {
            PendingKind::Ping => Query::Ping { id },
            PendingKind::FindNode => Query::FindNode {
                id,
                target: id,
                want: Want::NONE,
            },
            PendingKind::GetPeers | PendingKind::Announce => {
                unreachable!("built by their callers")
            }
        };
        self.send_query_msg(
            to,
            expect_id,
            kind,
            query,
            lookup,
            bootstrap_seed,
            force,
            now,
        )
    }

    /// Encode and queue a query, and track it. Returns `false` (sending nothing) when the
    /// rate limit or the pending cap says no; `force` skips the rate limit.
    #[allow(clippy::too_many_arguments)]
    fn send_query_msg(
        &mut self,
        to: SocketAddr,
        expect_id: Option<NodeId>,
        kind: PendingKind,
        query: Query,
        lookup: Option<(LookupId, usize)>,
        bootstrap_seed: bool,
        force: bool,
        now: Instant,
    ) -> bool {
        if self.tables[fam_of(&to)].is_none() || self.pending.len() >= MAX_PENDING {
            return false;
        }
        if !force && !self.query_limiter.try_take(now) {
            self.starved = true;
            return false;
        }
        let mut tid = self.next_tid;
        while self.pending.contains_key(&tid) {
            tid = tid.wrapping_add(1);
        }
        self.next_tid = tid.wrapping_add(1);
        let msg = Message {
            transaction_id: tid.to_be_bytes().to_vec(),
            version: self.cfg.version.clone(),
            read_only: self.cfg.read_only,
            ip: None,
            body: Body::Query(query),
        };
        self.transmits.push_back(Transmit {
            to,
            data: msg.encode(),
        });
        self.pending.insert(
            tid,
            Pending {
                to,
                expect_id,
                kind,
                lookup,
                bootstrap_seed,
                slow: false,
            },
        );
        self.slow_queue.push_back((now + SLOW_AFTER, tid));
        self.timeout_queue
            .push_back((now + self.cfg.query_timeout, tid));
        self.stats.queries_sent += 1;
        true
    }

    fn finish_lookup(&mut self, lid: LookupId, now: Instant) {
        let Some(l) = self.lookups.remove(&lid) else {
            return;
        };
        let mut announced_to = 0;
        let mut closest = Vec::new();
        for fl in l.fams.iter().flatten() {
            let responded = fl
                .candidates
                .iter()
                .filter(|c| matches!(c.state, CandidateState::Responded { .. }))
                .take(K);
            for c in responded {
                closest.push(c.info);
                if let (
                    LookupKind::GetPeers {
                        announce: Some(spec),
                        ..
                    },
                    CandidateState::Responded { token: Some(token) },
                ) = (&l.kind, &c.state)
                {
                    let query = Query::AnnouncePeer {
                        id: self.id,
                        info_hash: l.target,
                        port: spec.port.unwrap_or(0),
                        implied_port: spec.port.is_none(),
                        token: token.clone(),
                        seed: spec.seed,
                    };
                    // Announces are few and are the point of the lookup: skip the limit.
                    if self.send_query_msg(
                        c.info.addr,
                        Some(c.info.id),
                        PendingKind::Announce,
                        query,
                        None,
                        false,
                        true,
                        now,
                    ) {
                        announced_to += 1;
                    }
                }
            }
        }
        closest.sort_by_key(|n| n.id.distance(&l.target));
        if l.internal {
            return;
        }
        let scrape = (matches!(l.kind, LookupKind::GetPeers { scrape: true, .. }) && l.got_bloom)
            .then(|| ScrapeEstimate {
                seeds: l.bloom_seeds.estimate(),
                peers: l.bloom_peers.estimate(),
            });
        self.events.push_back(Event::LookupDone {
            lookup: lid,
            outcome: LookupOutcome {
                responded: l.responded,
                announced_to,
                closest,
                scrape,
            },
        });
    }

    /// Handle one received datagram.
    pub fn handle_datagram(&mut self, from: SocketAddr, data: &[u8], now: Instant) {
        let from = canonical(from);
        if self.tables[fam_of(&from)].is_none() || from.port() == 0 {
            return;
        }
        let msg = match krpc::decode(data) {
            Ok(m) => m,
            Err(e) => {
                self.stats.malformed += 1;
                trace!(%from, reason = e.reason, "malformed KRPC message");
                if let Some(tid) = e.transaction_id
                    && !self.cfg.read_only
                    && self.response_limiter.try_take(now)
                {
                    self.send_error(from, tid, error_code::PROTOCOL, "invalid arguments");
                }
                return;
            }
        };
        match msg.body {
            Body::Query(q) => self.handle_query(from, msg.transaction_id, msg.read_only, q, now),
            Body::Response(r) => {
                self.handle_response(from, &msg.transaction_id, msg.read_only, msg.ip, r, now);
            }
            Body::Error { code, .. } => {
                self.stats.errors_received += 1;
                trace!(%from, code, "KRPC error");
                self.handle_error(from, &msg.transaction_id, now);
            }
        }
    }

    fn take_pending(&mut self, from: SocketAddr, tid: &[u8]) -> Option<Pending> {
        let tid = u32::from_be_bytes(tid.try_into().ok()?);
        let p = self.pending.get(&tid)?;
        // The answer must come from the address we asked: anyone can guess a tid.
        if p.to != from {
            return None;
        }
        self.pending.remove(&tid)
    }

    fn handle_response(
        &mut self,
        from: SocketAddr,
        tid: &[u8],
        read_only: bool,
        ip: Option<SocketAddr>,
        r: Response,
        now: Instant,
    ) {
        let Some(p) = self.take_pending(from, tid) else {
            trace!(%from, "unsolicited response");
            return;
        };
        self.stats.responses_received += 1;
        let fam = fam_of(&from);

        // A different ID from the one we expected usually means the node switched to a
        // BEP 42 ID. The routing table takes the new ID (see `RoutingTable::insert`); the
        // lookup harvests the answer but doesn't count the node as one of the closest.
        let mismatch = p.expect_id.is_some_and(|id| id != r.id);
        if mismatch {
            trace!(%from, "response from an unexpected node ID");
        }
        if let Some(ip) = ip {
            self.vote_external_ip(fam, from.ip(), canonical(ip).ip(), now);
        }

        let admissible = !read_only
            && !p.bootstrap_seed
            && r.id != self.id
            && (!self.cfg.enforce_node_id || r.id.is_secure_for(from.ip()));
        if admissible && let Some(t) = self.tables[fam].as_mut() {
            let info = NodeInfo {
                id: r.id,
                addr: from,
            };
            if let InsertOutcome::Full { ping } = t.insert(info, now) {
                for n in ping {
                    self.send_query(
                        n.addr,
                        Some(n.id),
                        PendingKind::Ping,
                        None,
                        false,
                        false,
                        now,
                    );
                }
            }
        } else if read_only && let Some(t) = self.tables[fam].as_mut() {
            t.remove(&r.id);
        }

        // Nodes of the other family help bootstrap its table.
        let other = 1 - fam;
        if self.tables[other]
            .as_ref()
            .is_some_and(|t| t.len() < SMALL_TABLE)
        {
            let cross = if other == 1 { &r.nodes6 } else { &r.nodes };
            for n in cross.iter().take(2) {
                self.maybe_verify(n.addr, n.id, now);
            }
        }

        if let Some((lid, lfam)) = p.lookup {
            self.on_lookup_response(lid, lfam, &p, mismatch, r, now);
        }
    }

    fn on_lookup_response(
        &mut self,
        lid: LookupId,
        fam: usize,
        p: &Pending,
        mismatch: bool,
        r: Response,
        now: Instant,
    ) {
        let own = self.id;
        let Some(l) = self.lookups.get_mut(&lid) else {
            return;
        };
        let target = l.target;
        l.responded += 1;
        let Some(fl) = l.fams[fam].as_mut() else {
            return;
        };
        fl.outstanding = fl.outstanding.saturating_sub(1);
        if !p.slow {
            fl.in_flight = fl.in_flight.saturating_sub(1);
        }
        if let Some(c) = fl.candidates.iter_mut().find(|c| c.info.addr == p.to) {
            c.state = if mismatch {
                CandidateState::Failed
            } else {
                CandidateState::Responded { token: r.token }
            };
        }
        let nodes = if fam == 1 { &r.nodes6 } else { &r.nodes };
        for n in nodes {
            if fam_of(&n.addr) == fam && !fl.queried.contains(&n.addr) {
                insert_candidate(fl, &target, *n, own);
            }
        }

        if let LookupKind::GetPeers { scrape, .. } = l.kind {
            let mut new = Vec::new();
            for v in &r.values {
                let v = canonical(*v);
                if l.peers_seen.len() < MAX_PEERS_PER_LOOKUP && l.peers_seen.insert(v) {
                    new.push(v);
                }
            }
            if scrape && let (Some(s), Some(pe)) = (&r.bf_seeds, &r.bf_peers) {
                l.bloom_seeds.union(&Bloom(**s));
                l.bloom_peers.union(&Bloom(**pe));
                l.got_bloom = true;
            }
            if !new.is_empty() && !l.internal {
                self.events.push_back(Event::Peers {
                    lookup: lid,
                    peers: new,
                });
            }
        }
        self.drive_lookup(lid, now);
    }

    fn handle_error(&mut self, from: SocketAddr, tid: &[u8], now: Instant) {
        if let Some(p) = self.take_pending(from, tid) {
            // It's alive, but the query failed: the lookup moves on without it.
            if let Some((lid, fam)) = p.lookup {
                self.lookup_failure(lid, fam, &p, now);
            }
        }
    }

    fn on_failure(&mut self, p: &Pending, now: Instant) {
        if let Some(t) = self.tables[fam_of(&p.to)].as_mut() {
            t.failed(p.expect_id, p.to, now);
        }
        if let Some((lid, fam)) = p.lookup {
            self.lookup_failure(lid, fam, p, now);
        }
    }

    fn lookup_failure(&mut self, lid: LookupId, fam: usize, p: &Pending, now: Instant) {
        let Some(fl) = self
            .lookups
            .get_mut(&lid)
            .and_then(|l| l.fams[fam].as_mut())
        else {
            return;
        };
        fl.outstanding = fl.outstanding.saturating_sub(1);
        if !p.slow {
            fl.in_flight = fl.in_flight.saturating_sub(1);
        }
        if let Some(c) = fl.candidates.iter_mut().find(|c| c.info.addr == p.to) {
            c.state = CandidateState::Failed;
        }
        self.drive_lookup(lid, now);
    }

    fn handle_query(
        &mut self,
        from: SocketAddr,
        tid: Vec<u8>,
        sender_read_only: bool,
        q: Query,
        now: Instant,
    ) {
        self.stats.queries_received += 1;
        if self.cfg.read_only {
            return;
        }
        if !self.response_limiter.try_take(now) {
            self.stats.rate_limited += 1;
            return;
        }
        let fam = fam_of(&from);

        if let Some(id) = q.sender_id()
            && !sender_read_only
            && id != self.id
        {
            if let Some(t) = self.tables[fam].as_ref()
                && t.contains(&id)
            {
                // Already known; queries alone don't refresh it (they could be spoofed).
            } else {
                self.maybe_verify(from, id, now);
            }
        }

        let body = match q {
            Query::Ping { .. } => Body::Response(Response::with_id(self.id)),
            Query::FindNode { target, want, .. } => {
                let mut r = Response::with_id(self.id);
                self.fill_nodes(&mut r, &target, want, fam, now);
                Body::Response(r)
            }
            Query::GetPeers {
                info_hash,
                want,
                noseed,
                scrape,
                ..
            } => {
                let mut r = Response::with_id(self.id);
                r.token = Some(self.tokens.token_for(from.ip()).to_vec());
                let (want_v4, want_v6) = resolve_want(want, fam);
                for (on, v6) in [(want_v4, false), (want_v6, true)] {
                    if on {
                        r.values.extend(self.store.peers(
                            &info_hash,
                            v6,
                            noseed,
                            MAX_VALUES_PER_RESPONSE,
                            now,
                            &mut self.rng,
                        ));
                    }
                }
                r.values.truncate(MAX_VALUES_PER_RESPONSE);
                if scrape && let Some((seeds, peers)) = self.store.blooms(&info_hash, now) {
                    r.bf_seeds = Some(Box::new(seeds.0));
                    r.bf_peers = Some(Box::new(peers.0));
                }
                // Nodes too, always: it costs little and keeps lookups converging.
                self.fill_nodes(&mut r, &info_hash, want, fam, now);
                Body::Response(r)
            }
            Query::AnnouncePeer {
                info_hash,
                port,
                implied_port,
                token,
                seed,
                ..
            } => {
                if self.tokens.validate(from.ip(), &token) {
                    let port = if implied_port { from.port() } else { port };
                    let peer = SocketAddr::new(from.ip(), port);
                    if self.store.announce(info_hash, peer, seed, now) {
                        self.stats.announces_stored += 1;
                    }
                    Body::Response(Response::with_id(self.id))
                } else {
                    Body::Error {
                        code: error_code::PROTOCOL,
                        message: "bad token".into(),
                    }
                }
            }
            Query::Unknown { .. } => Body::Error {
                code: error_code::METHOD_UNKNOWN,
                message: "method unknown".into(),
            },
        };
        let msg = Message {
            transaction_id: tid,
            version: self.cfg.version.clone(),
            read_only: false,
            ip: Some(from),
            body,
        };
        self.transmits.push_back(Transmit {
            to: from,
            data: msg.encode(),
        });
    }

    fn fill_nodes(&self, r: &mut Response, target: &NodeId, want: Want, fam: usize, now: Instant) {
        let (v4, v6) = resolve_want(want, fam);
        if v4 && let Some(t) = &self.tables[0] {
            r.nodes = t.closest(target, K, now);
        }
        if v6 && let Some(t) = &self.tables[1] {
            r.nodes6 = t.closest(target, K, now);
        }
    }

    fn send_error(&mut self, to: SocketAddr, tid: Vec<u8>, code: i64, message: &str) {
        let msg = Message {
            transaction_id: tid,
            version: self.cfg.version.clone(),
            read_only: false,
            ip: None,
            body: Body::Error {
                code,
                message: message.into(),
            },
        };
        self.transmits.push_back(Transmit {
            to,
            data: msg.encode(),
        });
    }

    /// Ping a node we heard of but haven't verified, if its bucket has room for it.
    fn maybe_verify(&mut self, addr: SocketAddr, id: NodeId, now: Instant) {
        let addr = canonical(addr);
        let fam = fam_of(&addr);
        let Some(t) = self.tables[fam].as_ref() else {
            return;
        };
        if !t.has_room_for(&id) || (self.cfg.enforce_node_id && !id.is_secure_for(addr.ip())) {
            return;
        }
        if let Some(at) = self.recent_verify.get(&addr)
            && now.saturating_duration_since(*at) < VERIFY_AGAIN_AFTER
        {
            return;
        }
        if self.recent_verify.len() >= MAX_RECENT_VERIFY {
            return;
        }
        if self.send_query(addr, Some(id), PendingKind::Ping, None, false, false, now) {
            self.recent_verify.insert(addr, now);
        }
    }

    /// Record what a node says our external address is, and switch to a BEP 42 ID if a
    /// clear majority agrees on an address our ID doesn't match.
    fn vote_external_ip(&mut self, fam: usize, voter: IpAddr, ip: IpAddr, now: Instant) {
        if fam_of(&SocketAddr::new(ip, 1)) != fam {
            return;
        }
        let votes = &mut self.ip_votes[fam];
        votes.retain(|(v, _)| *v != voter);
        if votes.len() >= MAX_IP_VOTES {
            votes.pop_front();
        }
        votes.push_back((voter, ip));
        if votes.len() < MIN_IP_VOTES {
            return;
        }
        let mut counts: HashMap<IpAddr, usize> = HashMap::new();
        for (_, ip) in votes.iter() {
            *counts.entry(*ip).or_default() += 1;
        }
        let Some((&best, &n)) = counts.iter().max_by_key(|(_, n)| **n) else {
            return;
        };
        if n * 2 <= votes.len() {
            return;
        }
        self.external_ip[fam] = Some(best);

        // The ID can match only one address: follow IPv4 when it runs, else IPv6.
        let preferred = usize::from(self.tables[0].is_none());
        if self.cfg.secure_id
            && fam == preferred
            && !is_exempt(best)
            && !self.id.matches_secure_prefix(best)
        {
            let new_id = NodeId::secure(best, &mut self.rng);
            debug!(old = %self.id, new = %new_id, external = %best, "switching to a BEP 42 node ID");
            self.set_id(new_id, now);
        }
    }

    fn set_id(&mut self, id: NodeId, now: Instant) {
        self.id = id;
        for t in self.tables.iter_mut().flatten() {
            t.rebuild(id, now);
        }
        self.store.set_own_id(id);
        self.events.push_back(Event::IdChanged { id });
        // Introduce the new ID to the nodes around it.
        let lid = self.new_lookup(id, LookupKind::FindNode, true, now);
        self.drive_lookup(lid, now);
    }

    /// Handle timers: query timeouts, slow queries, rate-limited lookups and periodic
    /// maintenance. Call it at (or after) [`Node::poll_timeout`].
    pub fn handle_timeout(&mut self, now: Instant) {
        // Slow queries stop counting against alpha.
        let mut to_drive = Vec::new();
        while let Some(&(at, tid)) = self.slow_queue.front() {
            if at > now {
                break;
            }
            self.slow_queue.pop_front();
            if let Some(p) = self.pending.get_mut(&tid)
                && !p.slow
            {
                p.slow = true;
                if let Some((lid, fam)) = p.lookup {
                    if let Some(fl) = self
                        .lookups
                        .get_mut(&lid)
                        .and_then(|l| l.fams[fam].as_mut())
                    {
                        fl.in_flight = fl.in_flight.saturating_sub(1);
                    }
                    to_drive.push(lid);
                }
            }
        }
        while let Some(&(at, tid)) = self.timeout_queue.front() {
            if at > now {
                break;
            }
            self.timeout_queue.pop_front();
            if let Some(p) = self.pending.remove(&tid) {
                self.stats.timeouts += 1;
                trace!(to = %p.to, kind = ?p.kind, "DHT query timed out");
                self.on_failure(&p, now);
            }
        }
        for lid in to_drive {
            self.drive_lookup(lid, now);
        }

        if self.starved {
            self.starved = false;
            let ids: Vec<LookupId> = self.lookups.keys().copied().collect();
            for lid in ids {
                self.drive_lookup(lid, now);
            }
        }

        if now >= self.next_maintenance {
            self.next_maintenance = now + MAINTENANCE_EVERY;
            self.maintain(now);
        }
    }

    fn maintain(&mut self, now: Instant) {
        self.tokens.tick(now, &mut self.rng);
        if now >= self.next_store_expire {
            self.next_store_expire = now + STORE_EXPIRE_EVERY;
            self.store.expire(now);
            self.recent_verify
                .retain(|_, at| now.saturating_duration_since(*at) < VERIFY_AGAIN_AFTER);
        }

        // Re-bootstrap a family whose table is empty or sparse.
        let need_bootstrap = (0..2).any(|fam| {
            self.tables[fam]
                .as_ref()
                .is_some_and(|t| t.len() < SPARSE_TABLE)
                && self.last_bootstrap[fam]
                    .is_none_or(|t| now.saturating_duration_since(t) >= BOOTSTRAP_RETRY)
                && (!self.bootstrap_addrs.is_empty() || !self.saved_nodes.is_empty())
        });
        if need_bootstrap {
            self.bootstrap(now);
        }

        // Refresh stale buckets, and check on questionable nodes.
        for fam in 0..2 {
            let Some(t) = self.tables[fam].as_mut() else {
                continue;
            };
            if t.is_empty() {
                continue;
            }
            let targets = t.refresh_targets(now, &mut self.rng);
            let pings = t.due_pings(now, 4);
            for target in targets {
                let lid = self.new_lookup(target, LookupKind::FindNode, true, now);
                // Refreshing one family must not query the other.
                if let Some(l) = self.lookups.get_mut(&lid) {
                    l.fams[1 - fam] = None;
                }
                self.drive_lookup(lid, now);
            }
            for n in pings {
                self.send_query(
                    n.addr,
                    Some(n.id),
                    PendingKind::Ping,
                    None,
                    false,
                    false,
                    now,
                );
            }
        }
    }

    /// When [`Node::handle_timeout`] should next be called.
    pub fn poll_timeout(&mut self, now: Instant) -> Instant {
        let mut at = self.next_maintenance;
        if let Some(&(t, _)) = self.slow_queue.front() {
            at = at.min(t);
        }
        if let Some(&(t, _)) = self.timeout_queue.front() {
            at = at.min(t);
        }
        if self.starved {
            at = at.min(now + self.query_limiter.wait_time(now));
        }
        at
    }

    /// The next datagram to send.
    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        self.transmits.pop_front()
    }

    /// The next event.
    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    pub fn stats(&self, now: Instant) -> DhtStats {
        let table = |t: &Option<RoutingTable>| {
            t.as_ref().map(|t| TableStats {
                nodes: t.len(),
                good: t.good_len(now),
                buckets: t.bucket_count(),
            })
        };
        DhtStats {
            node_id: self.id.0,
            read_only: self.cfg.read_only,
            ipv4: table(&self.tables[0]),
            ipv6: table(&self.tables[1]),
            external_ipv4: self.external_ip[0],
            external_ipv6: self.external_ip[1],
            stored_info_hashes: self.store.info_hash_count(),
            stored_peers: self.store.peer_count(),
            active_lookups: self.lookups.len(),
            pending_queries: self.pending.len(),
            ..self.stats.clone()
        }
    }

    /// The state to persist: our ID and the good nodes of each table.
    pub fn export_state(&self, now: Instant) -> PersistedState {
        let nodes = |fam: usize| {
            self.tables[fam]
                .as_ref()
                .map(|t| t.good_nodes(now, MAX_SAVED_NODES))
                .unwrap_or_default()
        };
        PersistedState {
            id: self.id.0,
            nodes_v4: nodes(0),
            nodes_v6: nodes(1),
        }
    }
}

/// Which families to return nodes (and peers) for: what the querier asked, or the family
/// the query arrived on.
fn resolve_want(want: Want, fam: usize) -> (bool, bool) {
    if want.is_specified() {
        (want.v4, want.v6)
    } else {
        (fam == 0, fam == 1)
    }
}

/// Add a candidate in distance order, unless it's us, already there, or too far to matter.
fn insert_candidate(fl: &mut FamilyLookup, target: &NodeId, n: NodeInfo, own: NodeId) {
    if n.id == own
        || fl
            .candidates
            .iter()
            .any(|c| c.info.id == n.id || c.info.addr == n.addr)
    {
        return;
    }
    let distance = n.id.distance(target);
    let pos = fl.candidates.partition_point(|c| c.distance < distance);
    if pos >= MAX_CANDIDATES {
        return;
    }
    fl.candidates.insert(
        pos,
        Candidate {
            info: n,
            distance,
            state: CandidateState::Fresh,
        },
    );
    fl.candidates.truncate(MAX_CANDIDATES);
}

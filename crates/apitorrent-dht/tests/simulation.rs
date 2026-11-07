//! Deterministic network simulations of the sans-IO [`Node`]: many nodes exchanging
//! datagrams in memory, with a virtual clock and optional packet loss. Every run is
//! reproducible from its seed.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use apitorrent_dht::{AnnounceSpec, Event, LookupId, Node, NodeConfig, NodeId};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

struct Sim {
    nodes: Vec<Node>,
    addrs: Vec<SocketAddr>,
    by_addr: HashMap<SocketAddr, usize>,
    now: Instant,
    rng: StdRng,
    loss: f64,
    /// Events per node, collected as they happen.
    events: Vec<Vec<Event>>,
    delivered: u64,
}

impl Sim {
    /// `n` nodes on distinct public-looking IPv4 addresses (so BEP 42 and the
    /// one-node-per-IP rule apply), each bootstrapping from node 0.
    fn new(n: usize, seed: u64, loss: f64, tweak: impl Fn(usize, &mut NodeConfig)) -> Self {
        let now = Instant::now();
        let mut nodes = Vec::new();
        let mut addrs = Vec::new();
        for i in 0..n {
            let i16 = u16::try_from(i).unwrap();
            let ip = Ipv4Addr::new(23, 1, (i16 >> 8) as u8, (i16 & 0xff) as u8);
            addrs.push(SocketAddr::new(IpAddr::V4(ip), 6881));
            let mut cfg = NodeConfig {
                ipv6: false,
                rng_seed: Some(seed.wrapping_mul(1000).wrapping_add(i as u64)),
                ..NodeConfig::default()
            };
            tweak(i, &mut cfg);
            nodes.push(Node::new(cfg, None, now));
        }
        let by_addr = addrs.iter().enumerate().map(|(i, a)| (*a, i)).collect();
        let mut sim = Self {
            events: vec![Vec::new(); n],
            nodes,
            addrs,
            by_addr,
            now,
            rng: StdRng::seed_from_u64(seed),
            loss,
            delivered: 0,
        };
        let boot = sim.addrs[0];
        for i in 1..n {
            sim.nodes[i].add_bootstrap([boot], now);
            // Join one at a time, as real nodes do.
            sim.run_for(Duration::from_millis(200));
        }
        sim
    }

    /// Deliver every queued datagram (and whatever the replies queue) until quiet.
    fn flush(&mut self) {
        loop {
            let mut queue = Vec::new();
            for (i, n) in self.nodes.iter_mut().enumerate() {
                while let Some(t) = n.poll_transmit() {
                    queue.push((i, t));
                }
                while let Some(e) = n.poll_event() {
                    self.events[i].push(e);
                }
            }
            if queue.is_empty() {
                return;
            }
            for (from, t) in queue {
                if self.loss > 0.0 && self.rng.random_bool(self.loss) {
                    continue;
                }
                if let Some(&to) = self.by_addr.get(&t.to) {
                    let src = self.addrs[from];
                    self.nodes[to].handle_datagram(src, &t.data, self.now);
                    self.delivered += 1;
                }
            }
        }
    }

    fn run_for(&mut self, d: Duration) {
        let end = self.now + d;
        loop {
            self.flush();
            let now = self.now;
            let next = self
                .nodes
                .iter_mut()
                .map(|n| n.poll_timeout(now))
                .min()
                .unwrap();
            if next > end {
                break;
            }
            self.now = self.now.max(next);
            let now = self.now;
            for n in &mut self.nodes {
                if n.poll_timeout(now) <= now {
                    n.handle_timeout(now);
                }
            }
        }
        self.now = end;
    }

    /// Run until node `i`'s lookup finishes, returning the peers it found.
    fn run_lookup(&mut self, i: usize, lid: LookupId, limit: Duration) -> (Vec<SocketAddr>, bool) {
        let end = self.now + limit;
        while self.now < end && self.nodes[i].is_running(lid) {
            self.run_for(Duration::from_millis(100));
        }
        let mut peers = Vec::new();
        let mut done = false;
        for e in &self.events[i] {
            match e {
                Event::Peers { lookup, peers: p } if *lookup == lid => peers.extend(p),
                Event::LookupDone { lookup, .. } if *lookup == lid => done = true,
                _ => {}
            }
        }
        (peers, done)
    }

    fn announced_to(&self, i: usize, lid: LookupId) -> usize {
        self.events[i]
            .iter()
            .find_map(|e| match e {
                Event::LookupDone { lookup, outcome } if *lookup == lid => {
                    Some(outcome.announced_to)
                }
                _ => None,
            })
            .unwrap_or(0)
    }
}

fn info_hash(seed: u8) -> NodeId {
    let mut rng = StdRng::seed_from_u64(u64::from(seed));
    NodeId::random(&mut rng)
}

#[test]
fn announce_then_find_in_200_nodes() {
    let mut sim = Sim::new(200, 1, 0.0, |_, _| {});
    sim.run_for(Duration::from_secs(10));
    for (i, n) in sim.nodes.iter().enumerate() {
        let t = n.table(false).unwrap();
        assert!(t.len() >= 8, "node {i} knows only {} nodes", t.len());
    }

    let h = info_hash(1);
    let now = sim.now;
    let lid = sim.nodes[17].get_peers(
        h,
        Some(AnnounceSpec {
            port: Some(4444),
            seed: false,
        }),
        false,
        now,
    );
    let (_, done) = sim.run_lookup(17, lid, Duration::from_secs(30));
    assert!(done);
    let announced = sim.announced_to(17, lid);
    assert!(announced >= 6, "announced to {announced}");
    sim.run_for(Duration::from_secs(1));

    let now = sim.now;
    let lid = sim.nodes[123].get_peers(h, None, false, now);
    let (peers, done) = sim.run_lookup(123, lid, Duration::from_secs(30));
    assert!(done);
    let expected = SocketAddr::new(sim.addrs[17].ip(), 4444);
    assert_eq!(peers, vec![expected]);
}

#[test]
fn implied_port_and_dedup_across_announcers() {
    let mut sim = Sim::new(80, 2, 0.0, |_, _| {});
    sim.run_for(Duration::from_secs(5));
    let h = info_hash(2);
    let announcers = [3, 30, 60];
    for &a in &announcers {
        let now = sim.now;
        let lid = sim.nodes[a].get_peers(
            h,
            Some(AnnounceSpec {
                port: None,
                seed: false,
            }),
            false,
            now,
        );
        sim.run_lookup(a, lid, Duration::from_secs(30));
    }
    let now = sim.now;
    let lid = sim.nodes[70].get_peers(h, None, false, now);
    let (peers, _) = sim.run_lookup(70, lid, Duration::from_secs(30));
    let unique: HashSet<_> = peers.iter().collect();
    assert_eq!(unique.len(), peers.len(), "each peer reported once");
    for &a in &announcers {
        // implied_port: the DHT socket address itself.
        assert!(
            peers.contains(&sim.addrs[a]),
            "missing announcer {a}: {peers:?}"
        );
    }
}

#[test]
fn survives_ten_percent_packet_loss() {
    let mut sim = Sim::new(120, 3, 0.10, |_, _| {});
    sim.run_for(Duration::from_secs(20));
    let h = info_hash(3);
    let now = sim.now;
    let lid = sim.nodes[5].get_peers(
        h,
        Some(AnnounceSpec {
            port: Some(1000),
            seed: true,
        }),
        false,
        now,
    );
    sim.run_lookup(5, lid, Duration::from_secs(60));
    let now = sim.now;
    let lid = sim.nodes[99].get_peers(h, None, false, now);
    let (peers, done) = sim.run_lookup(99, lid, Duration::from_secs(60));
    assert!(done);
    assert!(
        peers.contains(&SocketAddr::new(sim.addrs[5].ip(), 1000)),
        "{peers:?}"
    );
}

#[test]
fn nodes_adopt_secure_ids_and_enforcement_works() {
    // Half the nodes enforce BEP 42 for their routing tables, but not node 0, the
    // bootstrap router. Everyone starts with a random
    // ID that the votes of its peers replace. (If every node enforced from the start, no
    // table would admit anyone, and nobody would collect the votes needed to fix its ID.)
    let mut sim = Sim::new(60, 4, 0.0, |i, c| c.enforce_node_id = i % 2 == 1);
    sim.run_for(Duration::from_secs(60));
    let secure = sim
        .nodes
        .iter()
        .zip(&sim.addrs)
        .filter(|(n, a)| n.id().is_secure_for(a.ip()))
        .count();
    assert_eq!(secure, 60, "only {secure}/60 nodes have a secure ID");
    for (i, n) in sim.nodes.iter().enumerate() {
        if let Some(stats) = n.stats(sim.now).external_ipv4 {
            assert_eq!(stats, sim.addrs[i].ip());
        }
        if i % 2 == 1 {
            let t = n.table(false).unwrap();
            assert!(t.len() >= 8, "enforcing node {i} has {} nodes", t.len());
            for e in t.iter() {
                assert!(
                    e.id.is_secure_for(e.addr.ip()),
                    "node {i} admitted an insecure ID"
                );
            }
        }
    }
    // The network still works after the ID changes.
    sim.run_for(Duration::from_secs(5));
    let h = info_hash(4);
    let now = sim.now;
    let lid = sim.nodes[10].get_peers(
        h,
        Some(AnnounceSpec {
            port: Some(7),
            seed: false,
        }),
        false,
        now,
    );
    sim.run_lookup(10, lid, Duration::from_secs(30));
    let now = sim.now;
    let lid = sim.nodes[50].get_peers(h, None, false, now);
    let (peers, _) = sim.run_lookup(50, lid, Duration::from_secs(30));
    assert_eq!(peers, vec![SocketAddr::new(sim.addrs[10].ip(), 7)]);
}

#[test]
fn read_only_nodes_stay_out_of_tables_but_can_look_up() {
    // A late joiner: nobody can discover a read-only node, so its table holds only what its
    // own lookups find, and an early joiner would see a small network.
    let ro = 45;
    let mut sim = Sim::new(50, 5, 0.0, |i, c| c.read_only = i == ro);
    sim.run_for(Duration::from_secs(10));
    let ro_id = sim.nodes[ro].id();
    for (i, n) in sim.nodes.iter().enumerate() {
        assert!(
            !n.table(false).unwrap().contains(&ro_id),
            "node {i} added the read-only node"
        );
    }
    assert!(sim.nodes[ro].table(false).unwrap().len() >= 8);

    // Someone announces; the read-only node finds them.
    let h = info_hash(5);
    let now = sim.now;
    let lid = sim.nodes[20].get_peers(
        h,
        Some(AnnounceSpec {
            port: Some(9),
            seed: false,
        }),
        false,
        now,
    );
    sim.run_lookup(20, lid, Duration::from_secs(30));
    let now = sim.now;
    let lid = sim.nodes[ro].get_peers(h, None, false, now);
    let (peers, _) = sim.run_lookup(ro, lid, Duration::from_secs(30));
    assert_eq!(peers, vec![SocketAddr::new(sim.addrs[20].ip(), 9)]);
}

#[test]
fn scrape_estimates_swarm() {
    let mut sim = Sim::new(60, 6, 0.0, |_, _| {});
    sim.run_for(Duration::from_secs(5));
    let h = info_hash(6);
    for (a, seed) in [(1, true), (2, true), (3, false), (4, false), (5, false)] {
        let now = sim.now;
        let lid = sim.nodes[a].get_peers(h, Some(AnnounceSpec { port: None, seed }), false, now);
        sim.run_lookup(a, lid, Duration::from_secs(30));
    }
    let now = sim.now;
    let lid = sim.nodes[40].get_peers(h, None, true, now);
    sim.run_lookup(40, lid, Duration::from_secs(30));
    let scrape = sim.events[40]
        .iter()
        .find_map(|e| match e {
            Event::LookupDone { lookup, outcome } if *lookup == lid => outcome.scrape,
            _ => None,
        })
        .expect("a scrape estimate");
    assert!((scrape.seeds - 2.0).abs() < 0.5, "{scrape:?}");
    assert!((scrape.peers - 3.0).abs() < 0.5, "{scrape:?}");
}

#[test]
fn query_rate_limit_is_respected() {
    // A tiny budget: lookups must still finish, just slower, and never exceed it.
    let mut sim = Sim::new(40, 7, 0.0, |_, c| {
        c.max_queries_per_second = 20;
        c.query_burst = 5;
    });
    sim.run_for(Duration::from_secs(20));
    let start = sim.now;
    let before = sim.nodes[9].stats(start).queries_sent;
    let lid = sim.nodes[9].find_node(info_hash(7), start);
    let (_, done) = sim.run_lookup(9, lid, Duration::from_secs(60));
    assert!(done);
    let elapsed = sim.now.duration_since(start).as_secs_f64();
    let sent = sim.nodes[9].stats(sim.now).queries_sent - before;
    #[allow(clippy::cast_precision_loss)]
    let sent_f = sent as f64;
    assert!(
        sent_f <= 5.0 + 20.0 * elapsed + 1.0,
        "{sent} queries in {elapsed}s"
    );
}

#[test]
fn state_roundtrip_rejoins_without_routers() {
    let mut sim = Sim::new(40, 8, 0.0, |_, _| {});
    sim.run_for(Duration::from_secs(5));
    let saved = sim.nodes[12].export_state(sim.now);
    assert_eq!(saved.id, sim.nodes[12].id().0);
    assert!(saved.nodes_v4.len() >= 8);
    let bytes = saved.to_bytes();
    let restored = apitorrent_dht::PersistedState::from_bytes(&bytes).unwrap();

    // Replace node 12 with a fresh node built from the saved state alone.
    let cfg = NodeConfig {
        ipv6: false,
        rng_seed: Some(999),
        ..NodeConfig::default()
    };
    sim.nodes[12] = Node::new(cfg, Some(&restored), sim.now);
    let now = sim.now;
    sim.nodes[12].add_bootstrap([], now);
    sim.run_for(Duration::from_secs(5));
    assert_eq!(sim.nodes[12].id(), NodeId(saved.id));
    assert!(sim.nodes[12].table(false).unwrap().len() >= 8);
}

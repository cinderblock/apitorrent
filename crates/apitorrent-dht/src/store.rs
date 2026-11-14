//! Peers announced to us with `announce_peer`, with hard memory bounds, and the bloom
//! filters of [BEP 33] for answering and merging scrapes.
//!
//! Both the number of info-hashes and the peers per info-hash are capped. When the
//! info-hash cap is reached, the info-hash farthest from our own ID makes way, because the
//! DHT expects the nodes closest to an info-hash to store it, and a far one is the least
//! likely to be asked about. Peers expire after [`PEER_TTL`] unless re-announced.
//!
//! [BEP 33]: https://www.bittorrent.org/beps/bep_0033.html

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use rand::seq::IteratorRandom;
use sha1::{Digest, Sha1};

use crate::id::NodeId;
use crate::krpc::{BLOOM_LEN, canonical};

/// How long an announced peer is kept without a fresh announce. Peers re-announce about
/// every 15 to 30 minutes.
pub const PEER_TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Copy)]
struct Stored {
    seed: bool,
    expires: Instant,
}

#[derive(Debug, Clone, Default)]
struct Swarm {
    peers: HashMap<SocketAddr, Stored>,
}

/// The peer store.
#[derive(Debug, Clone)]
pub struct PeerStore {
    own_id: NodeId,
    swarms: HashMap<NodeId, Swarm>,
    max_info_hashes: usize,
    max_peers_per_hash: usize,
    peer_count: usize,
}

impl PeerStore {
    pub fn new(own_id: NodeId, max_info_hashes: usize, max_peers_per_hash: usize) -> Self {
        Self {
            own_id,
            swarms: HashMap::new(),
            max_info_hashes: max_info_hashes.max(1),
            max_peers_per_hash: max_peers_per_hash.max(1),
            peer_count: 0,
        }
    }

    /// Follow a change of our own ID, which changes which info-hashes are "far".
    pub fn set_own_id(&mut self, id: NodeId) {
        self.own_id = id;
    }

    pub fn info_hash_count(&self) -> usize {
        self.swarms.len()
    }

    pub fn peer_count(&self) -> usize {
        self.peer_count
    }

    /// Store an announced peer. Returns `false` if the store chose not to keep it.
    pub fn announce(
        &mut self,
        info_hash: NodeId,
        peer: SocketAddr,
        seed: bool,
        now: Instant,
    ) -> bool {
        let peer = canonical(peer);
        if !self.swarms.contains_key(&info_hash) && self.swarms.len() >= self.max_info_hashes {
            self.expire(now);
            if self.swarms.len() >= self.max_info_hashes && !self.evict_farther_than(&info_hash) {
                return false;
            }
        }
        let swarm = self.swarms.entry(info_hash).or_default();
        let entry = Stored {
            seed,
            expires: now + PEER_TTL,
        };
        if let Some(e) = swarm.peers.get_mut(&peer) {
            *e = entry;
            return true;
        }
        if swarm.peers.len() >= self.max_peers_per_hash {
            // Make room by dropping the peer closest to expiry: the one heard from longest
            // ago.
            let oldest = swarm
                .peers
                .iter()
                .min_by_key(|(_, s)| s.expires)
                .map(|(a, _)| *a);
            if let Some(a) = oldest {
                swarm.peers.remove(&a);
                self.peer_count -= 1;
            }
        }
        swarm.peers.insert(peer, entry);
        self.peer_count += 1;
        true
    }

    /// Evict the stored info-hash farthest from our ID, if it is farther than `incoming`.
    fn evict_farther_than(&mut self, incoming: &NodeId) -> bool {
        let own = self.own_id;
        let Some(farthest) = self.swarms.keys().max_by_key(|h| h.distance(&own)).copied() else {
            return true;
        };
        if farthest.distance(&own) <= incoming.distance(&own) {
            return false;
        }
        if let Some(s) = self.swarms.remove(&farthest) {
            self.peer_count -= s.peers.len();
        }
        true
    }

    /// Up to `max` random peers for `info_hash` of the requested family. With `noseed`,
    /// seeds are left out (BEP 33).
    pub fn peers(
        &self,
        info_hash: &NodeId,
        v6: bool,
        noseed: bool,
        max: usize,
        now: Instant,
        rng: &mut impl rand::Rng,
    ) -> Vec<SocketAddr> {
        let Some(swarm) = self.swarms.get(info_hash) else {
            return Vec::new();
        };
        swarm
            .peers
            .iter()
            .filter(|(a, s)| a.is_ipv6() == v6 && s.expires > now && !(noseed && s.seed))
            .map(|(a, _)| *a)
            .sample(rng, max)
    }

    /// The BEP 33 bloom filters (seeds, downloaders) for `info_hash`, or `None` if we store
    /// no peers for it.
    pub fn blooms(&self, info_hash: &NodeId, now: Instant) -> Option<(Bloom, Bloom)> {
        let swarm = self.swarms.get(info_hash)?;
        let mut seeds = Bloom::default();
        let mut peers = Bloom::default();
        for (a, s) in &swarm.peers {
            if s.expires <= now {
                continue;
            }
            if s.seed {
                seeds.insert(a.ip());
            } else {
                peers.insert(a.ip());
            }
        }
        Some((seeds, peers))
    }

    /// Drop expired peers and empty swarms.
    pub fn expire(&mut self, now: Instant) {
        let mut removed = 0;
        self.swarms.retain(|_, swarm| {
            let before = swarm.peers.len();
            swarm.peers.retain(|_, s| s.expires > now);
            removed += before - swarm.peers.len();
            !swarm.peers.is_empty()
        });
        self.peer_count -= removed;
    }
}

/// A BEP 33 bloom filter of IP addresses: 2048 bits, two hash functions.
#[derive(Clone, PartialEq, Eq)]
pub struct Bloom(pub [u8; BLOOM_LEN]);

impl Default for Bloom {
    fn default() -> Self {
        Self([0; BLOOM_LEN])
    }
}

impl std::fmt::Debug for Bloom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bloom(~{:.0})", self.estimate())
    }
}

const BLOOM_BITS: usize = BLOOM_LEN * 8;

impl Bloom {
    /// Add an address. IPv4 hashes its 4 bytes, IPv6 its 16.
    pub fn insert(&mut self, ip: IpAddr) {
        let hash = match ip.to_canonical() {
            IpAddr::V4(v4) => Sha1::digest(v4.octets()),
            IpAddr::V6(v6) => Sha1::digest(v6.octets()),
        };
        let i1 = (usize::from(hash[0]) | (usize::from(hash[1]) << 8)) % BLOOM_BITS;
        let i2 = (usize::from(hash[2]) | (usize::from(hash[3]) << 8)) % BLOOM_BITS;
        self.0[i1 / 8] |= 1 << (i1 % 8);
        self.0[i2 / 8] |= 1 << (i2 % 8);
    }

    /// Merge another filter into this one (a set union).
    pub fn union(&mut self, other: &Bloom) {
        for (a, b) in self.0.iter_mut().zip(other.0.iter()) {
            *a |= b;
        }
    }

    /// The estimated number of distinct addresses inserted (BEP 33's formula). Reliable up
    /// to a few thousand. An empty filter estimates 0, where the formula as written (which
    /// clamps the zero count to `m - 1`) would say 0.5.
    pub fn estimate(&self) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        let m = BLOOM_BITS as f64;
        let zeros: u32 = self.0.iter().map(|b| b.count_zeros()).sum();
        if zeros as usize == BLOOM_BITS {
            return 0.0;
        }
        let c = f64::from(zeros).min(m - 1.0);
        (c / m).ln() / (2.0 * (1.0 - 1.0 / m).ln())
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn addr(i: u16) -> SocketAddr {
        SocketAddr::from(([8, 8, (i >> 8) as u8, i as u8], 6881))
    }

    /// The test vector from BEP 33.
    #[test]
    fn bep33_vector() {
        let expected_hex = "F6C3F5EAA07FFD91BDE89F777F26FB2BFF37BDB8FB2BBAA2FD3DDDE7BACFFF75EE7CCBAE\
            FE5EEDB1FBFAFF67F6ABFF5E43DDBCA3FD9B9FFDF4FFD3E9DFF12D1BDF59DB53DBE9FA5B\
            7FF3B8FDFCDE1AFB8BEDD7BE2F3EE71EBBBFE93BCDEEFE148246C2BC5DBFF7E7EFDCF24F\
            D8DC7ADFFD8FFFDFDDFFF7A4BBEEDF5CB95CE81FC7FCFF1FF4FFFFDFE5F7FDCBB7FD79B3\
            FA1FC77BFE07FFF905B7B7FFC7FEFEFFE0B8370BB0CD3F5B7F2BD93FEB4386CFDD6F7FD5\
            BFAF2E9EBFFFFEECD67ADBF7C67F17EFD5D75EBA6FFEBA7FFF47A91EB1BFBB53E8ABFB57\
            62ABE8FF237279BFEFBFEEF5FFC5FEBFDFE5ADFFADFEE1FB737FFFFBFD9F6AEFFEEE76B6\
            FD8F72EF";
        let expected: Vec<u8> = (0..BLOOM_LEN)
            .map(|i| u8::from_str_radix(&expected_hex[i * 2..i * 2 + 2], 16).unwrap())
            .collect();
        let mut b = Bloom::default();
        for i in 0..=255u8 {
            b.insert(IpAddr::V4(Ipv4Addr::new(192, 0, 2, i)));
        }
        for i in 0..=0x3e7u16 {
            b.insert(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, i)));
        }
        assert_eq!(b.0.to_vec(), expected);
        assert!((b.estimate() - 1224.9308).abs() < 0.001, "{}", b.estimate());
    }

    #[test]
    fn caps_peers_per_hash() {
        let now = Instant::now();
        let mut s = PeerStore::new(NodeId([0; 20]), 10, 3);
        let h = NodeId([1; 20]);
        for i in 0..10 {
            assert!(s.announce(h, addr(i), false, now + Duration::from_secs(u64::from(i))));
        }
        assert_eq!(s.peer_count(), 3);
        let mut rng = StdRng::seed_from_u64(0);
        let got = s.peers(&h, false, false, 50, now, &mut rng);
        assert_eq!(got.len(), 3);
        // The newest three survive.
        for i in 7..10 {
            assert!(got.contains(&addr(i)));
        }
    }

    #[test]
    fn caps_info_hashes_evicting_far_ones() {
        let now = Instant::now();
        let own = NodeId([0; 20]);
        let mut s = PeerStore::new(own, 2, 10);
        assert!(s.announce(NodeId([0x80; 20]), addr(1), false, now));
        assert!(s.announce(NodeId([0x40; 20]), addr(2), false, now));
        // Farther than everything stored: refused.
        assert!(!s.announce(NodeId([0xff; 20]), addr(3), false, now));
        // Closer: the farthest (0x80..) makes way.
        assert!(s.announce(NodeId([0x01; 20]), addr(4), false, now));
        assert_eq!(s.info_hash_count(), 2);
        assert_eq!(s.peer_count(), 2);
        let mut rng = StdRng::seed_from_u64(0);
        assert!(
            s.peers(&NodeId([0x80; 20]), false, false, 5, now, &mut rng)
                .is_empty()
        );
    }

    #[test]
    fn expiry_families_and_noseed() {
        let now = Instant::now();
        let mut s = PeerStore::new(NodeId([0; 20]), 10, 10);
        let h = NodeId([5; 20]);
        s.announce(h, addr(1), true, now);
        s.announce(h, "[2001:db8::1]:6881".parse().unwrap(), false, now);
        let mut rng = StdRng::seed_from_u64(0);
        assert_eq!(s.peers(&h, false, false, 5, now, &mut rng).len(), 1);
        assert_eq!(s.peers(&h, true, false, 5, now, &mut rng).len(), 1);
        assert!(s.peers(&h, false, true, 5, now, &mut rng).is_empty());
        let (seeds, peers) = s.blooms(&h, now).unwrap();
        assert!((seeds.estimate() - 1.0).abs() < 0.01);
        assert!((peers.estimate() - 1.0).abs() < 0.01);
        assert_eq!(Bloom::default().estimate(), 0.0);
        s.expire(now + PEER_TTL);
        assert_eq!(s.peer_count(), 0);
        assert_eq!(s.info_hash_count(), 0);
    }
}

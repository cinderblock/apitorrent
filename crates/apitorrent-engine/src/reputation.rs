//! Deciding which peers sent bad data. Pure state, no I/O.
//!
//! One failed piece is not proof: several peers usually contributed to it. The rules:
//!
//! - A failed piece that came wholly from one address proves that address sent bad data.
//! - A failed piece with several senders puts them all **on parole**, and the digest of
//!   every block is recorded with its sender. When the piece later verifies, any recorded
//!   block whose digest differs from the good copy proves its sender lied (libtorrent calls
//!   this "smart ban"). The other senders are cleared.
//! - Trust points: a piece that verifies with one sender, or being cleared, earns one. Only
//!   a piece that fails *again* costs trust, since proof may never come for it; reaching
//!   the floor is a ban for being untrusted. Parole ends when trust is non-negative and the
//!   peer has delivered good data.
//!
//! A peer on parole is only given whole pieces of its own (see
//! [`crate::picker::Picker::pick_exclusive`]), so its next failure is attributable.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;

use apitorrent_metainfo::Sha1Hash;

use crate::ban::BanReason;

const TRUST_GOOD: i32 = 1;
const TRUST_FAILED: i32 = 2;
const TRUST_MAX: i32 = 20;
const TRUST_BAN_AT: i32 = -7;
/// At most this many block records are kept per failed piece.
const MAX_RECORDS_PER_PIECE: usize = 4096;

/// A ban the caller should apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub ip: IpAddr,
    pub reason: BanReason,
    pub why: &'static str,
}

/// One torrent's view of its peers' honesty.
#[derive(Debug, Default)]
pub struct Reputation {
    /// Blocks of failed pieces: (sender, block index, digest).
    suspects: HashMap<u32, Vec<(IpAddr, usize, Sha1Hash)>>,
    trust: HashMap<IpAddr, i32>,
    parole: HashSet<IpAddr>,
}

impl Reputation {
    pub fn on_parole(&self, ip: IpAddr) -> bool {
        self.parole.contains(&ip)
    }

    pub fn trust(&self, ip: IpAddr) -> i32 {
        self.trust.get(&ip).copied().unwrap_or(0)
    }

    /// Whether the piece failed before, so a good copy's block digests are wanted.
    pub fn wants_digests(&self, piece: u32) -> bool {
        self.suspects.contains_key(&piece)
    }

    /// Forget an address that has been banned.
    pub fn forget(&mut self, ip: IpAddr) {
        self.parole.remove(&ip);
        self.trust.remove(&ip);
    }

    /// A piece failed verification. `senders` has the sender of each block, `digests` the
    /// SHA-1 of each block.
    pub fn piece_failed(
        &mut self,
        piece: u32,
        senders: &[Option<IpAddr>],
        digests: &[Sha1Hash],
    ) -> Vec<Verdict> {
        let ips = distinct(senders);
        match ips.as_slice() {
            [] => Vec::new(),
            [only] => vec![Verdict {
                ip: *only,
                reason: BanReason::BadData,
                why: "sent a whole piece that failed verification",
            }],
            many => {
                let failed_before = self.suspects.contains_key(&piece);
                let records = self.suspects.entry(piece).or_default();
                for (block, ip) in senders.iter().enumerate() {
                    if let (Some(ip), Some(d)) = (ip, digests.get(block))
                        && records.len() < MAX_RECORDS_PER_PIECE
                    {
                        records.push((*ip, block, *d));
                    }
                }
                let mut bans = Vec::new();
                for &ip in many {
                    self.parole.insert(ip);
                    if !failed_before {
                        continue;
                    }
                    let t = self.trust.entry(ip).or_insert(0);
                    *t -= TRUST_FAILED;
                    if *t <= TRUST_BAN_AT {
                        bans.push(Verdict {
                            ip,
                            reason: BanReason::Untrusted,
                            why: "took part in too many failed pieces",
                        });
                    }
                }
                bans
            }
        }
    }

    /// A piece verified. `digests` are its blocks' SHA-1s, if
    /// [`Reputation::wants_digests`] asked for them.
    pub fn piece_verified(
        &mut self,
        piece: u32,
        senders: &[Option<IpAddr>],
        digests: Option<&[Sha1Hash]>,
    ) -> Vec<Verdict> {
        let mut bans = Vec::new();
        if let (Some(records), Some(good)) = (self.suspects.remove(&piece), digests) {
            let mut liars: Vec<IpAddr> = records
                .iter()
                .filter(|(_, block, digest)| good.get(*block).is_some_and(|g| g != digest))
                .map(|(ip, _, _)| *ip)
                .collect();
            liars.sort_unstable();
            liars.dedup();
            let mut cleared: Vec<IpAddr> = records
                .iter()
                .map(|(ip, _, _)| *ip)
                .filter(|ip| liars.binary_search(ip).is_err())
                .collect();
            cleared.sort_unstable();
            cleared.dedup();
            for ip in cleared {
                self.credit(ip);
            }
            bans.extend(liars.into_iter().map(|ip| Verdict {
                ip,
                reason: BanReason::BadData,
                why: "sent blocks that differ from the verified piece",
            }));
        }
        if let [only] = distinct(senders).as_slice() {
            self.credit(*only);
        }
        bans
    }

    fn credit(&mut self, ip: IpAddr) {
        let t = self.trust.entry(ip).or_insert(0);
        *t = (*t + TRUST_GOOD).min(TRUST_MAX);
        if *t >= 0 {
            self.parole.remove(&ip);
        }
    }
}

fn distinct(senders: &[Option<IpAddr>]) -> Vec<IpAddr> {
    let mut ips: Vec<IpAddr> = senders.iter().flatten().copied().collect();
    ips.sort_unstable();
    ips.dedup();
    ips
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, n])
    }

    fn d(n: u8) -> Sha1Hash {
        Sha1Hash::digest(&[n])
    }

    #[test]
    fn a_whole_bad_piece_from_one_peer_is_proof() {
        let mut r = Reputation::default();
        let bans = r.piece_failed(0, &[Some(ip(1)), Some(ip(1))], &[d(0), d(1)]);
        assert_eq!(bans.len(), 1);
        assert_eq!(bans[0].ip, ip(1));
        assert_eq!(bans[0].reason, BanReason::BadData);
    }

    #[test]
    fn smart_ban_finds_the_liar_and_clears_the_rest() {
        let mut r = Reputation::default();
        // Peer 2 sent a bad block 1; peers 1 and 3 sent good blocks.
        let senders = [Some(ip(1)), Some(ip(2)), Some(ip(3))];
        assert!(r.piece_failed(5, &senders, &[d(0), d(99), d(2)]).is_empty());
        assert!(r.on_parole(ip(1)) && r.on_parole(ip(2)) && r.on_parole(ip(3)));
        assert!(r.wants_digests(5));
        // The good copy came from peer 3 alone.
        let bans = r.piece_verified(5, &[Some(ip(3)); 3], Some(&[d(0), d(1), d(2)]));
        assert_eq!(bans.len(), 1);
        assert_eq!(bans[0].ip, ip(2));
        assert_eq!(bans[0].reason, BanReason::BadData);
        assert!(!r.on_parole(ip(1)), "cleared");
        assert!(!r.on_parole(ip(3)), "cleared and credited");
        assert!(!r.wants_digests(5));
    }

    #[test]
    fn honest_peers_survive_many_mixed_failures() {
        // An honest peer shares many failed pieces with a liar, as happens before parole
        // takes effect. It must never be banned for that alone.
        let mut r = Reputation::default();
        for piece in 0..50 {
            let senders = [Some(ip(1)), Some(ip(2))];
            assert!(r.piece_failed(piece, &senders, &[d(0), d(66)]).is_empty());
        }
        for piece in 0..50 {
            let bans = r.piece_verified(piece, &[Some(ip(1)); 2], Some(&[d(0), d(1)]));
            assert_eq!(bans.iter().map(|b| b.ip).collect::<Vec<_>>(), [ip(2)]);
        }
        assert!(!r.on_parole(ip(1)));
        assert!(r.trust(ip(1)) > 0);
    }

    #[test]
    fn a_piece_that_keeps_failing_costs_trust() {
        let mut r = Reputation::default();
        let senders = [Some(ip(1)), Some(ip(2))];
        let mut banned = Vec::new();
        for _ in 0..10 {
            banned.extend(r.piece_failed(7, &senders, &[d(0), d(1)]));
            if !banned.is_empty() {
                break;
            }
        }
        assert_eq!(banned.len(), 2, "both untrusted after repeated failures");
        assert!(banned.iter().all(|b| b.reason == BanReason::Untrusted));
    }
}

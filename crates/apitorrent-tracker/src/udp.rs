//! The UDP tracker protocol ([BEP 15]) as plain data and codecs, with no I/O: packet
//! encoding and decoding, the retransmission schedule and the connection-id cache.
//!
//! [BEP 15]: https://www.bittorrent.org/beps/bep_0015.html

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use apitorrent_metainfo::Sha1Hash;
use apitorrent_wire::compact;

use crate::error::TrackerError;
use crate::types::{
    AnnounceRequest, AnnounceResponse, MAX_PEERS, MAX_SCRAPE_HASHES, ScrapeResponse, ScrapeStats,
};

/// The magic constant that opens a connect request.
pub const PROTOCOL_ID: u64 = 0x0417_2710_1980;

pub const ACTION_CONNECT: u32 = 0;
pub const ACTION_ANNOUNCE: u32 = 1;
pub const ACTION_SCRAPE: u32 = 2;
pub const ACTION_ERROR: u32 = 3;

pub const CONNECT_REQUEST_LEN: usize = 16;
pub const ANNOUNCE_REQUEST_LEN: usize = 98;

/// A receive buffer big enough for any UDP datagram.
pub const MAX_DATAGRAM: usize = 65_536;

/// How long a connection id may be used after it was received. BEP 15: "a client can use a
/// connection ID until one minute after it has received it"; trackers accept it for two.
pub const CONNECTION_ID_TTL: Duration = Duration::from_secs(60);

/// The size of each peer entry in an announce response. BEP 15 says this follows the
/// address family of the packet: 6 bytes when we talk to the tracker over IPv4, 18 over
/// IPv6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerFamily {
    V4,
    V6,
}

impl PeerFamily {
    /// The family of peers a tracker at `addr` returns.
    pub fn of(addr: &SocketAddr) -> Self {
        match addr {
            SocketAddr::V4(_) => Self::V4,
            // An IPv4-mapped address is still IPv4 on the wire.
            SocketAddr::V6(a) if a.ip().to_ipv4_mapped().is_some() => Self::V4,
            SocketAddr::V6(_) => Self::V6,
        }
    }

    fn entry_len(self) -> usize {
        match self {
            Self::V4 => 6,
            Self::V6 => 18,
        }
    }
}

pub fn encode_connect(transaction_id: u32) -> [u8; CONNECT_REQUEST_LEN] {
    let mut out = [0u8; CONNECT_REQUEST_LEN];
    out[0..8].copy_from_slice(&PROTOCOL_ID.to_be_bytes());
    out[8..12].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
    out[12..16].copy_from_slice(&transaction_id.to_be_bytes());
    out
}

pub fn encode_announce(
    connection_id: u64,
    transaction_id: u32,
    req: &AnnounceRequest,
) -> [u8; ANNOUNCE_REQUEST_LEN] {
    let mut out = [0u8; ANNOUNCE_REQUEST_LEN];
    out[0..8].copy_from_slice(&connection_id.to_be_bytes());
    out[8..12].copy_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
    out[12..16].copy_from_slice(&transaction_id.to_be_bytes());
    out[16..36].copy_from_slice(req.info_hash.as_bytes());
    out[36..56].copy_from_slice(&req.peer_id.0);
    out[56..64].copy_from_slice(&req.downloaded.to_be_bytes());
    out[64..72].copy_from_slice(&req.left.to_be_bytes());
    out[72..80].copy_from_slice(&req.uploaded.to_be_bytes());
    out[80..84].copy_from_slice(&req.event.udp_code().to_be_bytes());
    // 84..88: IP address, 0 = the packet's source address.
    out[88..92].copy_from_slice(&req.key.to_be_bytes());
    let num_want: i32 = req
        .num_want
        .map_or(-1, |n| i32::try_from(n).unwrap_or(i32::MAX));
    out[92..96].copy_from_slice(&num_want.to_be_bytes());
    out[96..98].copy_from_slice(&req.port.to_be_bytes());
    out
}

pub fn encode_scrape(
    connection_id: u64,
    transaction_id: u32,
    hashes: &[Sha1Hash],
) -> Result<Vec<u8>, TrackerError> {
    crate::http::check_scrape_hashes(hashes)?;
    debug_assert!(hashes.len() <= MAX_SCRAPE_HASHES);
    let mut out = Vec::with_capacity(16 + 20 * hashes.len());
    out.extend_from_slice(&connection_id.to_be_bytes());
    out.extend_from_slice(&ACTION_SCRAPE.to_be_bytes());
    out.extend_from_slice(&transaction_id.to_be_bytes());
    for h in hashes {
        out.extend_from_slice(h.as_bytes());
    }
    Ok(out)
}

fn be_u32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

/// The action and transaction id of a response, or `None` if it is too short to have them.
/// The I/O layer uses this to drop datagrams that answer some other request.
pub fn response_header(buf: &[u8]) -> Option<(u32, u32)> {
    (buf.len() >= 8).then(|| (be_u32(buf, 0), be_u32(buf, 4)))
}

/// Check the header and return the body after it. An error packet becomes
/// [`TrackerError::Failure`].
fn check_header(buf: &[u8], transaction_id: u32, action: u32) -> Result<&[u8], TrackerError> {
    let (got_action, got_tx) =
        response_header(buf).ok_or_else(|| TrackerError::malformed("packet too short"))?;
    if got_tx != transaction_id {
        return Err(TrackerError::malformed("transaction id mismatch"));
    }
    let body = &buf[8..];
    if got_action == ACTION_ERROR {
        let msg = String::from_utf8_lossy(body);
        let msg = msg.trim_end_matches('\0').trim();
        return Err(TrackerError::Failure(if msg.is_empty() {
            "(no reason given)".into()
        } else {
            msg.into()
        }));
    }
    if got_action != action {
        return Err(TrackerError::malformed(format!(
            "expected action {action}, got {got_action}"
        )));
    }
    Ok(body)
}

/// Decode a connect response and return the connection id.
pub fn decode_connect(buf: &[u8], transaction_id: u32) -> Result<u64, TrackerError> {
    let body = check_header(buf, transaction_id, ACTION_CONNECT)?;
    let id: [u8; 8] = body
        .get(..8)
        .ok_or_else(|| TrackerError::malformed("connect response too short"))?
        .try_into()
        .expect("8 bytes");
    Ok(u64::from_be_bytes(id))
}

pub fn decode_announce(
    buf: &[u8],
    transaction_id: u32,
    family: PeerFamily,
) -> Result<AnnounceResponse, TrackerError> {
    let body = check_header(buf, transaction_id, ACTION_ANNOUNCE)?;
    if body.len() < 12 {
        return Err(TrackerError::malformed("announce response too short"));
    }
    let interval = be_u32(body, 0);
    let leechers = be_u32(body, 4);
    let seeders = be_u32(body, 8);
    let peers = &body[12..];
    let entry = family.entry_len();
    if !peers.len().is_multiple_of(entry) {
        return Err(TrackerError::malformed(format!(
            "peer list of {} bytes is not a multiple of {entry}",
            peers.len()
        )));
    }
    let peers = &peers[..peers.len().min(MAX_PEERS * entry)];
    let mut peers = match family {
        PeerFamily::V4 => compact::parse_v4(peers),
        PeerFamily::V6 => compact::parse_v6(peers),
    };
    peers.retain(|p| p.port() != 0);
    Ok(AnnounceResponse {
        interval: Duration::from_secs(interval.into()),
        min_interval: None,
        peers,
        seeders: Some(seeders),
        leechers: Some(leechers),
        tracker_id: None,
        warning: None,
    })
}

/// Decode a scrape response. Entries come in the order of `hashes`; a tracker may send
/// fewer, and any extra are ignored.
pub fn decode_scrape(
    buf: &[u8],
    transaction_id: u32,
    hashes: &[Sha1Hash],
) -> Result<ScrapeResponse, TrackerError> {
    let body = check_header(buf, transaction_id, ACTION_SCRAPE)?;
    if !body.len().is_multiple_of(12) {
        return Err(TrackerError::malformed(format!(
            "scrape body of {} bytes is not a multiple of 12",
            body.len()
        )));
    }
    let mut out = ScrapeResponse::default();
    for (hash, c) in hashes.iter().zip(body.as_chunks::<12>().0) {
        out.files.insert(
            *hash,
            ScrapeStats {
                complete: be_u32(c, 0),
                downloaded: be_u32(c, 4),
                incomplete: be_u32(c, 8),
            },
        );
    }
    Ok(out)
}

/// When to retransmit a UDP request.
///
/// Attempt `n` (from 0) waits `initial_timeout · 2ⁿ` for a response. BEP 15 specifies
/// 15 s and up to 9 attempts, about an hour in all ([`RetryPolicy::BEP15`]). That is far too
/// slow when a torrent has other trackers to try, so the default is 3 attempts from 4 s:
/// 4 + 8 + 16 = 28 s per step before [`TrackerError::Timeout`]. Over a lossy path that still
/// gives three chances, and a dead tracker costs under half a minute, so the engine moves
/// on to the next tracker in the tier quickly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub initial_timeout: Duration,
    /// The number of transmissions, at least 1.
    pub attempts: u32,
}

impl RetryPolicy {
    /// The schedule BEP 15 describes: 15 s doubling, 9 attempts.
    pub const BEP15: Self = Self {
        initial_timeout: Duration::from_secs(15),
        attempts: 9,
    };

    /// How long to wait for a response to transmission `attempt` (from 0).
    pub fn timeout(&self, attempt: u32) -> Duration {
        self.initial_timeout
            .saturating_mul(2u32.saturating_pow(attempt.min(16)))
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            initial_timeout: Duration::from_secs(4),
            attempts: 3,
        }
    }
}

/// Connection ids by tracker address, each valid for [`CONNECTION_ID_TTL`].
///
/// Time is passed in, so this is deterministic in tests.
#[derive(Debug, Default)]
pub struct ConnectionCache {
    entries: HashMap<SocketAddr, (u64, Instant)>,
}

impl ConnectionCache {
    /// The most trackers remembered. A session rarely talks to more than a few hundred.
    pub const CAPACITY: usize = 1024;

    pub fn get(&self, addr: &SocketAddr, now: Instant) -> Option<u64> {
        let &(id, received) = self.entries.get(addr)?;
        (now.saturating_duration_since(received) < CONNECTION_ID_TTL).then_some(id)
    }

    pub fn insert(&mut self, addr: SocketAddr, id: u64, now: Instant) {
        if self.entries.len() >= Self::CAPACITY && !self.entries.contains_key(&addr) {
            self.entries
                .retain(|_, (_, t)| now.saturating_duration_since(*t) < CONNECTION_ID_TTL);
            if self.entries.len() >= Self::CAPACITY
                && let Some(oldest) = self
                    .entries
                    .iter()
                    .min_by_key(|(_, (_, t))| *t)
                    .map(|(a, _)| *a)
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(addr, (id, now));
    }

    pub fn remove(&mut self, addr: &SocketAddr) {
        self.entries.remove(addr);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use apitorrent_wire::PeerId;

    use super::*;
    use crate::types::AnnounceEvent;

    fn header(action: u32, tx: u32) -> Vec<u8> {
        let mut v = action.to_be_bytes().to_vec();
        v.extend_from_slice(&tx.to_be_bytes());
        v
    }

    #[test]
    fn connect_round_trip() {
        let req = encode_connect(0x1234_5678);
        assert_eq!(
            req,
            [
                0, 0, 0x04, 0x17, 0x27, 0x10, 0x19, 0x80, 0, 0, 0, 0, 0x12, 0x34, 0x56, 0x78
            ]
        );
        let mut resp = header(ACTION_CONNECT, 7);
        resp.extend_from_slice(&0xAABB_CCDD_EEFF_0011u64.to_be_bytes());
        assert_eq!(decode_connect(&resp, 7).unwrap(), 0xAABB_CCDD_EEFF_0011);
        assert!(matches!(
            decode_connect(&resp, 8),
            Err(TrackerError::Malformed(_))
        ));
        assert!(matches!(
            decode_connect(&resp[..15], 7),
            Err(TrackerError::Malformed(_))
        ));
        let mut wrong = header(ACTION_ANNOUNCE, 7);
        wrong.extend_from_slice(&[0; 8]);
        assert!(matches!(
            decode_connect(&wrong, 7),
            Err(TrackerError::Malformed(_))
        ));
    }

    #[test]
    fn announce_layout() {
        let req = AnnounceRequest {
            info_hash: Sha1Hash([1; 20]),
            peer_id: PeerId([2; 20]),
            port: 0x1AE1,
            uploaded: 3,
            downloaded: 4,
            left: 5,
            event: AnnounceEvent::Stopped,
            num_want: None,
            key: 0x0102_0304,
            tracker_id: None,
        };
        let p = encode_announce(9, 10, &req);
        assert_eq!(&p[0..8], &9u64.to_be_bytes());
        assert_eq!(&p[8..12], &[0, 0, 0, 1]);
        assert_eq!(&p[12..16], &10u32.to_be_bytes());
        assert_eq!(&p[16..36], &[1; 20]);
        assert_eq!(&p[36..56], &[2; 20]);
        assert_eq!(&p[56..64], &4u64.to_be_bytes());
        assert_eq!(&p[64..72], &5u64.to_be_bytes());
        assert_eq!(&p[72..80], &3u64.to_be_bytes());
        assert_eq!(&p[80..84], &3u32.to_be_bytes());
        assert_eq!(&p[84..88], &[0; 4]);
        assert_eq!(&p[88..92], &[1, 2, 3, 4]);
        assert_eq!(&p[92..96], &(-1i32).to_be_bytes());
        assert_eq!(&p[96..98], &[0x1A, 0xE1]);

        let p = encode_announce(
            9,
            10,
            &AnnounceRequest {
                num_want: Some(u32::MAX),
                ..req
            },
        );
        assert_eq!(&p[92..96], &i32::MAX.to_be_bytes());
    }

    #[test]
    fn announce_responses() {
        let mut resp = header(ACTION_ANNOUNCE, 1);
        for n in [1800u32, 4, 9] {
            resp.extend_from_slice(&n.to_be_bytes());
        }
        let mut v4 = resp.clone();
        v4.extend_from_slice(&[1, 2, 3, 4, 0x1a, 0xe1, 9, 9, 9, 9, 0, 0]);
        let r = decode_announce(&v4, 1, PeerFamily::V4).unwrap();
        assert_eq!(r.interval, Duration::from_secs(1800));
        assert_eq!(r.leechers, Some(4));
        assert_eq!(r.seeders, Some(9));
        assert_eq!(r.peers, ["1.2.3.4:6881".parse::<SocketAddr>().unwrap()]);
        // 12 bytes is not a whole number of IPv6 entries.
        assert!(decode_announce(&v4, 1, PeerFamily::V6).is_err());

        let mut v6 = resp.clone();
        let mut entry = [0u8; 18];
        entry[15] = 1;
        entry[16..].copy_from_slice(&[0x1a, 0xe1]);
        v6.extend_from_slice(&entry);
        let r = decode_announce(&v6, 1, PeerFamily::V6).unwrap();
        assert_eq!(r.peers, ["[::1]:6881".parse::<SocketAddr>().unwrap()]);

        assert!(decode_announce(&resp[..15], 1, PeerFamily::V4).is_err());

        let mut err = header(ACTION_ERROR, 1);
        err.extend_from_slice(b"torrent not registered\0");
        match decode_announce(&err, 1, PeerFamily::V4) {
            Err(TrackerError::Failure(m)) => assert_eq!(m, "torrent not registered"),
            other => panic!("{other:?}"),
        }
        // An error packet for another transaction is not ours to report.
        assert!(matches!(
            decode_announce(&err, 2, PeerFamily::V4),
            Err(TrackerError::Malformed(_))
        ));
    }

    #[test]
    fn family_of_address() {
        assert_eq!(
            PeerFamily::of(&"1.2.3.4:1".parse().unwrap()),
            PeerFamily::V4
        );
        assert_eq!(PeerFamily::of(&"[::1]:1".parse().unwrap()), PeerFamily::V6);
        assert_eq!(
            PeerFamily::of(&"[::ffff:1.2.3.4]:1".parse().unwrap()),
            PeerFamily::V4
        );
    }

    #[test]
    fn scrape_round_trip() {
        let hashes = [Sha1Hash([1; 20]), Sha1Hash([2; 20]), Sha1Hash([3; 20])];
        let p = encode_scrape(5, 6, &hashes).unwrap();
        assert_eq!(p.len(), 16 + 60);
        assert_eq!(&p[8..12], &[0, 0, 0, 2]);
        assert_eq!(&p[56..76], &[3; 20]);
        assert!(encode_scrape(5, 6, &[]).is_err());

        let mut resp = header(ACTION_SCRAPE, 6);
        for n in [10u32, 20, 30, 11, 21, 31] {
            resp.extend_from_slice(&n.to_be_bytes());
        }
        let r = decode_scrape(&resp, 6, &hashes).unwrap();
        assert_eq!(r.files.len(), 2);
        assert_eq!(
            r.files[&hashes[1]],
            ScrapeStats {
                complete: 11,
                downloaded: 21,
                incomplete: 31
            }
        );
        assert!(decode_scrape(&resp[..resp.len() - 1], 6, &hashes).is_err());
    }

    #[test]
    fn retry_schedule() {
        let p = RetryPolicy::default();
        let waits: Vec<_> = (0..p.attempts).map(|n| p.timeout(n).as_secs()).collect();
        assert_eq!(waits, [4, 8, 16]);
        assert_eq!(RetryPolicy::BEP15.timeout(8), Duration::from_secs(3840));
        // Absurd attempt numbers don't overflow.
        let _ = RetryPolicy::BEP15.timeout(u32::MAX);
    }

    #[test]
    fn connection_ids_expire() {
        let mut cache = ConnectionCache::default();
        let a: SocketAddr = "1.2.3.4:80".parse().unwrap();
        let t0 = Instant::now();
        cache.insert(a, 42, t0);
        assert_eq!(cache.get(&a, t0 + Duration::from_secs(59)), Some(42));
        assert_eq!(cache.get(&a, t0 + CONNECTION_ID_TTL), None);
        cache.remove(&a);
        assert!(cache.is_empty());
    }

    #[test]
    fn connection_cache_is_bounded() {
        let mut cache = ConnectionCache::default();
        let t0 = Instant::now();
        for i in 0..=ConnectionCache::CAPACITY {
            let port = u16::try_from(i + 1).unwrap();
            let t = t0 + Duration::from_millis(u64::from(port));
            cache.insert(SocketAddr::from(([127, 0, 0, 1], port)), 1, t);
        }
        assert_eq!(cache.len(), ConnectionCache::CAPACITY);
        // The oldest entry made room.
        let first = SocketAddr::from(([127, 0, 0, 1], 1));
        assert_eq!(cache.get(&first, t0), None);
    }
}

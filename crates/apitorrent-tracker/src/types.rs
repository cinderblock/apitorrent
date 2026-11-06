use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use apitorrent_metainfo::Sha1Hash;
use apitorrent_wire::PeerId;

/// The most peers kept from one response. A hostile tracker can send a 2 MiB peer list
/// (about 350,000 compact entries); nothing useful needs more than this.
pub const MAX_PEERS: usize = 2_000;

/// The most info-hashes in one scrape. BEP 15 limits a UDP scrape to about 74 hashes so the
/// response fits one datagram; we apply the same limit to HTTP so URLs stay short.
pub const MAX_SCRAPE_HASHES: usize = 74;

/// The `event` of an announce (BEP 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum AnnounceEvent {
    /// A regular, periodic announce.
    #[default]
    None,
    /// The first announce after the torrent starts.
    Started,
    /// The download finished. Sent once, not when starting an already complete torrent.
    Completed,
    /// The torrent is stopping.
    Stopped,
}

impl AnnounceEvent {
    /// The HTTP `event` parameter value; `None` for a regular announce, which omits it.
    pub fn http_name(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Started => Some("started"),
            Self::Completed => Some("completed"),
            Self::Stopped => Some("stopped"),
        }
    }

    /// The BEP 15 event code.
    pub fn udp_code(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Completed => 1,
            Self::Started => 2,
            Self::Stopped => 3,
        }
    }
}

/// One announce to one tracker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnounceRequest {
    /// The v1 info-hash, or the truncated v2 info-hash for a v2-only swarm.
    pub info_hash: Sha1Hash,
    pub peer_id: PeerId,
    /// The port we accept peer connections on.
    pub port: u16,
    pub uploaded: u64,
    pub downloaded: u64,
    /// Bytes still to download.
    pub left: u64,
    pub event: AnnounceEvent,
    /// How many peers we want; `None` lets the tracker choose (usually 50).
    pub num_want: Option<u32>,
    /// A random value, fixed for the session, that lets the tracker recognise us when our
    /// IP address changes.
    pub key: u32,
    /// The `tracker id` from this tracker's previous response, if it sent one.
    pub tracker_id: Option<String>,
}

/// A successful announce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnounceResponse {
    /// How long to wait before the next regular announce.
    pub interval: Duration,
    /// The shortest wait the tracker allows before announcing again, if it set one.
    pub min_interval: Option<Duration>,
    /// Peers in the swarm, IPv4 and IPv6, at most [`MAX_PEERS`]. Entries with port 0 are
    /// dropped.
    pub peers: Vec<SocketAddr>,
    pub seeders: Option<u32>,
    pub leechers: Option<u32>,
    /// To send back as [`AnnounceRequest::tracker_id`] next time.
    pub tracker_id: Option<String>,
    /// A `warning message`: the announce worked, but the tracker has something to say.
    pub warning: Option<String>,
}

/// Swarm statistics for one torrent, from a scrape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScrapeStats {
    /// Seeders.
    pub complete: u32,
    /// Leechers.
    pub incomplete: u32,
    /// How many times the torrent has been downloaded to completion.
    pub downloaded: u32,
}

/// A successful scrape. Holds an entry for each requested info-hash the tracker knows;
/// entries the tracker sent for hashes we didn't ask about are dropped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScrapeResponse {
    pub files: HashMap<Sha1Hash, ScrapeStats>,
}

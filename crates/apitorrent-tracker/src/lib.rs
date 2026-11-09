//! A BitTorrent tracker client: HTTP and HTTPS trackers ([BEP 3], with [BEP 23] compact
//! peers, [BEP 7] `peers6` and [BEP 48] scrape) and UDP trackers ([BEP 15]).
//!
//! [`TrackerClient`] does the I/O and picks the protocol from the URL scheme. The protocol
//! itself lives in plain functions that are tested without a network: [`http`] builds
//! request URLs and parses response bodies, and [`udp`] encodes and decodes packets. Every
//! decoder treats its input as hostile: lengths, transaction ids and action codes are
//! checked, peer lists are capped at [`MAX_PEERS`], and no input makes it panic.
//!
//! HTTPS certificates are always verified against the operating system's trust store, and
//! nothing can turn that off (see the `tls` module).
//!
//! Not yet supported: BEP 41 UDP URL options (the path and query of a `udp://` URL are
//! ignored), and announcing through a proxy.
//!
//! [BEP 3]: https://www.bittorrent.org/beps/bep_0003.html
//! [BEP 7]: https://www.bittorrent.org/beps/bep_0007.html
//! [BEP 15]: https://www.bittorrent.org/beps/bep_0015.html
//! [BEP 23]: https://www.bittorrent.org/beps/bep_0023.html
//! [BEP 48]: https://www.bittorrent.org/beps/bep_0048.html

mod client;
mod error;
pub mod http;
mod tls;
mod types;
pub mod udp;

pub use client::{TrackerClient, TrackerConfig, strict_http_client, strict_http_client_builder};
pub use error::TrackerError;
/// Re-exported so users of [`strict_http_client`] share the exact same version.
pub use reqwest;
pub use types::{
    AnnounceEvent, AnnounceRequest, AnnounceResponse, MAX_PEERS, MAX_SCRAPE_HASHES, ScrapeResponse,
    ScrapeStats,
};
pub use udp::RetryPolicy;

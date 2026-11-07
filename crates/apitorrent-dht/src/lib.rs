//! The Mainline DHT: finding peers for an info-hash, and announcing ourselves, without a
//! tracker.
//!
//! Implemented:
//!
//! - [BEP 5]: KRPC over bencode, `ping`, `find_node`, `get_peers` and `announce_peer`,
//!   write tokens from rotating secrets, a k-bucket routing table (K = 8) with replacement
//!   caches and bucket refresh, and iterative lookups with alpha = 3.
//! - [BEP 32]: IPv6, with its own socket and routing table, and the `want` key.
//! - [BEP 42]: secure node IDs derived from our external address (learned from the `ip`
//!   key in responses), and optional enforcement for nodes entering the routing table.
//! - [BEP 43]: read-only mode.
//! - [BEP 33]: DHT scrape with bloom filters, both answering and asking.
//!
//! The protocol is a sans-IO state machine, [`Node`], tested without sockets. [`Dht`] runs
//! it on tokio with one UDP socket per address family. Every decoder treats input as
//! hostile and caps what it builds (see [`krpc`]); outgoing queries and outgoing responses
//! are rate limited; the peer store caps both info-hashes and peers per info-hash.
//!
//! ```no_run
//! # async fn demo() -> Result<(), apitorrent_dht::DhtError> {
//! use futures_util::StreamExt;
//! use apitorrent_dht::{Dht, DhtConfig};
//!
//! let dht = Dht::start(DhtConfig::default()).await?;
//! let mut peers = dht.announce([0u8; 20], Some(51413));
//! while let Some(peer) = peers.next().await {
//!     println!("{peer}");
//! }
//! let saved = dht.export_state().await?;
//! dht.shutdown().await;
//! # let _ = saved; Ok(()) }
//! ```
//!
//! [BEP 5]: https://www.bittorrent.org/beps/bep_0005.html
//! [BEP 32]: https://www.bittorrent.org/beps/bep_0032.html
//! [BEP 33]: https://www.bittorrent.org/beps/bep_0033.html
//! [BEP 42]: https://www.bittorrent.org/beps/bep_0042.html
//! [BEP 43]: https://www.bittorrent.org/beps/bep_0043.html

mod dht;
pub mod id;
pub mod krpc;
pub mod node;
mod rate;
pub mod routing;
mod state;
pub mod store;
mod token;

pub use dht::{DEFAULT_BOOTSTRAP, Dht, DhtConfig, DhtError, PeerStream};
pub use id::NodeId;
pub use node::{
    AnnounceSpec, DhtStats, Event, LookupId, LookupOutcome, Node, NodeConfig, ScrapeEstimate,
    TableStats, Transmit,
};
pub use state::{PersistedState, StateError};

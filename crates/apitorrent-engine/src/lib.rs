//! The apitorrent BitTorrent engine.
//!
//! The protocol-level pieces (bitfields, layout, picking, verification) are plain state with
//! no I/O. The session layer drives them from tokio tasks: one actor per torrent, one task
//! per peer connection.

mod announce;
pub mod ban;
pub mod bitfield;
pub mod fileid;
pub mod hashreq;
pub mod identity;
pub mod layout;
pub mod limits;
pub mod lsd;
pub mod metadata;
pub mod parts;
mod peer;
pub mod picker;
pub mod reachability;
pub mod reputation;
mod session;
pub mod storage;
mod torrent;
pub mod verify;
mod webseed;

pub use announce::TrackerStatus;
pub use apitorrent_proxy::{Credentials, Proxy, ProxyKind};
pub use session::{AddError, ProxySettings, Session, SessionConfig};
pub use storage::{AdoptMode, Adopted};
pub use torrent::{
    AddTorrent, AdoptRefused, Adoption, EntryKey, EntryParams, EntryStats, FileEvent, Offered,
    PeerInfo, PieceEvent, PieceOrigin, TorrentHandle, TorrentState, TorrentStats, UnwantedPolicy,
};

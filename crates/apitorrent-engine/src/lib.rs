//! The apitorrent BitTorrent engine.
//!
//! The protocol-level pieces (bitfields, layout, picking, verification) are plain state with
//! no I/O. The session layer drives them from tokio tasks: one actor per torrent, one task
//! per peer connection.

mod announce;
pub mod ban;
pub mod bitfield;
pub mod layout;
pub mod limits;
pub mod lsd;
pub mod metadata;
pub mod parts;
mod peer;
pub mod picker;
pub mod reputation;
mod session;
pub mod storage;
mod torrent;
pub mod verify;

pub use announce::TrackerStatus;
pub use session::{AddError, Session, SessionConfig};
pub use torrent::{AddTorrent, TorrentHandle, TorrentState, TorrentStats, UnwantedPolicy};

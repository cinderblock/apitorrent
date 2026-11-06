//! The apitorrent BitTorrent engine.
//!
//! The protocol-level pieces (bitfields, layout, picking, verification) are plain state with
//! no I/O. The session layer drives them from tokio tasks.

pub mod bitfield;
pub mod layout;
pub mod picker;
pub mod storage;
pub mod verify;

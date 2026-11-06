//! The BitTorrent peer wire protocol as plain data and codecs, with no I/O.
//!
//! The engine owns the sockets. This crate turns bytes into messages and back, so that every
//! protocol rule can be tested without a network, and fuzzed.

pub mod compact;
pub mod extended;
mod handshake;
mod message;

pub use handshake::{
    HANDSHAKE_LEN, HANDSHAKE_PREFIX_LEN, Handshake, HandshakeError, PROTOCOL, PeerId, Reserved,
};
pub use message::{
    BlockRef, Decoder, HashRequest, MAX_BLOCK_LEN, Message, WireError, piece_header,
};

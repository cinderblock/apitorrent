//! Bencode, the serialisation format of BitTorrent ([BEP 3]).
//!
//! The decoder is built for hostile input:
//!
//! - It borrows from the input instead of copying, and records the raw span of every list and
//!   dictionary. Info-hashes are computed over those exact bytes, never over a re-encoding.
//! - Nesting depth is limited, so deeply nested input cannot overflow the stack.
//! - Integers reject leading zeros, `-0` and anything that does not fit an `i64`.
//! - Dictionaries with duplicate keys are always rejected: a duplicate key makes the meaning
//!   of a document depend on which parser reads it. Keys out of order are rejected by
//!   default, and accepted (then sorted) when [`DecodeOptions::require_sorted_keys`] is off,
//!   because real-world `.torrent` files sometimes have them.
//!
//! [BEP 3]: https://www.bittorrent.org/beps/bep_0003.html

mod decode;
mod encode;
mod value;

pub use decode::{DecodeOptions, Error, decode, decode_prefix, decode_with};
pub use encode::OwnedValue;
pub use value::{Dict, List, Value};

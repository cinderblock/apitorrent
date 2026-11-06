//! BitTorrent metainfo: `.torrent` files and magnet links, for v1 ([BEP 3]), v2 ([BEP 52]) and
//! hybrid torrents.
//!
//! Parsing validates everything that later code relies on: piece counts match the data
//! size, v2 piece layers match their merkle roots, and a hybrid torrent's v1 and v2 file
//! lists describe the same files. File names are sanitised into safe, portable paths (see
//! [`sanitize`]), and paths that would collide on a case-insensitive filesystem are
//! disambiguated.
//!
//! [BEP 3]: https://www.bittorrent.org/beps/bep_0003.html
//! [BEP 52]: https://www.bittorrent.org/beps/bep_0052.html

mod error;
mod hash;
mod info;
mod magnet;
pub mod merkle;
pub mod sanitize;
mod torrent;

pub use error::MetainfoError;
pub use hash::{InfoHashes, ParseHashError, Sha1Hash, Sha256Hash};
pub use info::{FileAttributes, FileEntry, Info, Limits, Version};
pub use magnet::{Magnet, MagnetError};
pub use torrent::Torrent;

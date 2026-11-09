//! What the core asks of `tracker-adapter` plugins (see the architecture doc, "Pack
//! switchover").
//!
//! Trackers word "this torrent is gone" differently, and some have APIs that know which
//! season pack replaced a single episode. A tracker adapter covers the trackers it names;
//! for those, its answers replace the core's built-in patterns, and it may name the pack
//! that holds a torrent's files so the core can add it.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use apitorrent_types::{AddSource, TorrentId};

use crate::BoxFuture;

/// "Does this announce failure mean the torrent was deleted from the tracker?"
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct UnregisteredQuery {
    pub torrent: TorrentId,
    pub name: String,
    /// The announce URL that failed.
    pub tracker: String,
    /// The tracker's failure message.
    pub message: String,
}

/// A file of the torrent whose pack is wanted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PackFile {
    /// Path inside the torrent, joined with `/`.
    pub path: String,
    #[ts(type = "number")]
    pub length: u64,
}

/// "Which pack contains this torrent's files?" Asked about a torrent its tracker no longer
/// knows, which nothing supersedes yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PackQuery {
    pub torrent: TorrentId,
    pub name: String,
    pub info_hash_v1: Option<String>,
    pub info_hash_v2: Option<String>,
    /// The announce URL the adapter covers.
    pub tracker: String,
    pub files: Vec<PackFile>,
}

/// The core's view of the `tracker-adapter` plugins.
pub trait TrackerAdapters: Send + Sync + 'static {
    /// Whether some running adapter covers the tracker at this announce URL.
    fn covers(&self, tracker: &str) -> bool;

    /// The covering adapter's verdict, or `None` when no adapter covers the tracker or the
    /// one that does failed to answer (the core then uses its built-in patterns, or does
    /// nothing).
    fn unregistered(&self, query: UnregisteredQuery) -> BoxFuture<Option<bool>>;

    /// A pack that holds the torrent's files, as something the core can add, or `None`.
    fn find_pack(&self, query: PackQuery) -> BoxFuture<Option<AddSource>>;
}

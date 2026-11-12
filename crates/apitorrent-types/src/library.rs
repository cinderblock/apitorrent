//! The content library's server-wide side: retroactive dedupe of identical files, and
//! library folders (files outside torrents that new torrents can reuse).
//!
//! Everything here is a server admin's: folder paths and file names are the instance's,
//! whichever spaces' torrents use them. The dedupe numbers are aggregates.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// What the dedupe pass does with identical files that torrents downloaded separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum DedupeMode {
    /// No pass runs.
    Off,
    /// Find them and report the space that merging them would free, but change nothing.
    /// The default, so the savings are seen before anything is merged.
    #[default]
    Report,
    /// Merge them into hard links (or clones) of one copy.
    On,
}

/// What the last dedupe pass found and did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DedupeStatus {
    pub mode: DedupeMode,
    /// A pass is running now.
    pub running: bool,
    /// When the last pass ended (ms since the Unix epoch).
    #[ts(type = "number | null")]
    pub last_pass_at: Option<i64>,
    /// Bytes that merging the identical copies found would free.
    #[ts(type = "number")]
    pub reclaimable_bytes: u64,
    /// Files that would be replaced by links.
    #[ts(type = "number")]
    pub reclaimable_files: u64,
    /// Sets of identical files found (each holding at least two separate copies).
    #[ts(type = "number")]
    pub groups: u64,
    /// Identical copies that can't be merged: on different volumes, or held by a link the
    /// library doesn't know (another space's copy placed by hand, say).
    #[ts(type = "number")]
    pub unmergeable_files: u64,
    /// Bytes still to be read before every same-size file is known (hashed in idle time).
    #[ts(type = "number")]
    pub pending_hash_bytes: u64,
    /// Bytes freed by merging so far, over the instance's life.
    #[ts(type = "number")]
    pub reclaimed_bytes: u64,
    /// Files replaced by links so far, over the instance's life.
    #[ts(type = "number")]
    pub reclaimed_files: u64,
    /// Files the last pass left alone because they changed, were busy, or failed.
    #[ts(type = "number")]
    pub skipped_files: u64,
}

/// A folder of files outside torrents (a media folder, say) that new torrents may reuse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct LibraryFolder {
    #[ts(type = "number")]
    pub id: i64,
    /// The folder's absolute path on the daemon's machine.
    pub path: String,
    #[ts(type = "number")]
    pub added_at: i64,
    /// Being walked now.
    pub scanning: bool,
    /// When the last walk ended (ms since the Unix epoch).
    #[ts(type = "number | null")]
    pub last_scan_at: Option<i64>,
    /// Files found, and their total size.
    #[ts(type = "number")]
    pub files: u64,
    #[ts(type = "number")]
    pub bytes: u64,
    /// Files fingerprinted (v2 root and SHA-256), and their total size.
    #[ts(type = "number")]
    pub hashed_files: u64,
    #[ts(type = "number")]
    pub hashed_bytes: u64,
    /// Why the last walk failed (the folder is gone, say).
    pub error: Option<String>,
}

/// `GET /api/v1/library`: dedupe and library folders.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct LibraryStatus {
    pub dedupe: DedupeStatus,
    pub folders: Vec<LibraryFolder>,
}

/// `POST /api/v1/library/folders`: add a library folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct AddLibraryFolderRequest {
    /// An absolute path to an existing folder on the daemon's machine.
    pub path: String,
}

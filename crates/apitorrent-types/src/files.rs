//! How a torrent's files download and where they go when complete: the file order,
//! sequential files, and moving completed files.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// The order a torrent's files download in. A strong preference, not a rule: within a
/// priority, the first unfinished file's pieces are asked for first, but peers that don't
/// have them still supply later files. Explicit priorities come first.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", tag = "by")]
#[ts(export)]
pub enum FileOrder {
    /// No preference between files: rarest pieces first.
    #[default]
    Off,
    /// By path, numbers compared by value (`ep2` before `ep10`).
    Name,
    SmallestFirst,
    LargestFirst,
    /// As listed in the torrent.
    Torrent,
    /// This list of file indexes first, in this order, then the rest by name. Only for a
    /// torrent's own order; as a default it means no preference.
    Manual {
        files: Vec<u32>,
    },
}

/// When completed files move.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum MoveWhen {
    /// Each file as soon as it is complete, before the rest of the torrent.
    #[default]
    EachFile,
    /// Every file once the whole torrent is (Deluge's "move completed").
    WholeTorrent,
}

/// Files matching `pattern` go to `to` instead of the default destination.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct MoveRule {
    /// A glob over the file's path inside the torrent, with `/` between folders: `*`
    /// within a name, `**` across folders, `?` one character, case ignored. Without a `/`
    /// it matches the file name alone, at any depth (`*.mkv`).
    pub pattern: String,
    /// Where matching files go. Empty keeps them where they are.
    pub to: String,
}

/// Moving completed files out of the save directory. A moved file keeps its path relative
/// to the save directory under its destination, and keeps seeding from there.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
pub struct MoveCompleted {
    pub enabled: bool,
    /// Where files go that no rule sends elsewhere. Empty leaves them in place.
    pub to: String,
    pub when: MoveWhen,
    /// Checked in order; the first match decides.
    pub rules: Vec<MoveRule>,
}

impl MoveCompleted {
    /// Whether this moves anything at all.
    pub fn active(&self) -> bool {
        self.enabled && (!self.to.is_empty() || self.rules.iter().any(|r| !r.to.is_empty()))
    }
}

/// `PUT /torrents/{id}/file-order`: the torrent's own file order. `null` follows its
/// group's or the instance's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SetFileOrderRequest {
    pub order: Option<FileOrder>,
}

/// `PUT /torrents/{id}/sequential`: download these files front to back, to read them while
/// they download. Files not listed keep their setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SetSequentialRequest {
    pub files: Vec<SequentialFile>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SequentialFile {
    /// [`crate::TorrentFile::index`].
    pub index: u32,
    pub sequential: bool,
    /// The byte the reader needs next (a player's position). Downloading continues from
    /// there to the end, then from the start. 0 when omitted.
    #[ts(type = "number")]
    #[serde(default)]
    pub from: u64,
}

/// `PUT /torrents/{id}/first-last`: fetch each wanted file's first and last piece before
/// the rest of its priority, so files can be previewed (a player reads a container's header
/// and often its index at the end) before they complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SetFirstLastRequest {
    pub on: bool,
}

/// `PUT /torrents/{id}/super-seed`: super-seeding (BEP 16), for the first seed of a new
/// torrent. Once complete, it shows each peer a piece or two at a time, the rarest it
/// lacks, and another once it has one, so fewer bytes leave this seed before the swarm
/// holds a full copy. It applies to peers that connect afterwards. Once others seed too,
/// it only slows things down: turn it off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SetSuperSeedRequest {
    pub on: bool,
}

/// `PUT /torrents/{id}/move-completed`: the torrent's own setting. `null` follows its
/// group's or the instance's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SetMoveCompletedRequest {
    pub settings: Option<MoveCompleted>,
}

//! Types of the apitorrent HTTP and WebSocket API (`/api/v1`).
//!
//! These are the wire contract between the server and every client: the web UI, the desktop
//! app, the CLI and plugins. TypeScript definitions are generated from them (run
//! `cargo test -p apitorrent-types export_bindings`), so the UI cannot drift from the server.
//!
//! JSON field names are camelCase. Byte counts and rates are plain numbers (bytes and
//! bytes per second); timestamps are milliseconds since the Unix epoch.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub mod auth;
pub mod tls;

/// The API version in the URL prefix.
pub const API_VERSION: u32 = 1;

/// A torrent's id: its v2 info-hash in hex if it has one, otherwise its v1 info-hash.
/// Stable across restarts and the same on every instance.
pub type TorrentId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum TorrentState {
    /// A magnet link whose info dictionary is still being fetched from peers.
    Metadata,
    /// Checking files already on disk.
    Checking,
    Downloading,
    /// Every wanted piece is downloaded; uploading to others.
    Seeding,
    Paused,
    /// Waiting for a free slot in the queue (see [`QueueSettings`]).
    Queued,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct TorrentSummary {
    pub id: TorrentId,
    pub name: String,
    pub info_hash_v1: Option<String>,
    pub info_hash_v2: Option<String>,
    pub state: TorrentState,
    /// Fraction of the *wanted* pieces downloaded, 0 to 1. Skipped files don't count, so
    /// a finished torrent shows 1 even when some files are skipped.
    pub progress: f64,
    /// Total size of the torrent's files.
    #[ts(type = "number")]
    pub size: u64,
    pub pieces_have: u32,
    pub pieces_total: u32,
    /// All-time totals, including previous sessions.
    #[ts(type = "number")]
    pub downloaded: u64,
    #[ts(type = "number")]
    pub uploaded: u64,
    #[ts(type = "number")]
    pub download_rate: u64,
    #[ts(type = "number")]
    pub upload_rate: u64,
    pub peers: u32,
    pub seeds: u32,
    #[ts(type = "number")]
    pub added_at: i64,
    pub label: Option<String>,
    pub save_path: String,
    pub private: bool,
    pub error: Option<String>,
    /// Position in the queue; lower runs first.
    pub queue_position: u32,
    /// The group whose shared limits apply, chosen by label.
    pub group: Option<String>,
    /// Another torrent holds all of this one's files (a pack that replaced it).
    pub superseded_by: Option<TorrentId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct TorrentFile {
    pub index: u32,
    /// Path inside the save directory, joined with `/`.
    pub path: String,
    #[ts(type = "number")]
    pub length: u64,
    /// 0 means skipped.
    pub priority: u8,
    /// Fraction of the file's pieces downloaded, 0 to 1.
    pub progress: f64,
    pub placement: FilePlacement,
}

/// Where a file's data is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum FilePlacement {
    /// In the save directory.
    Save,
    /// Deselected, and kept in the torrent's held area so it can come back without a
    /// download.
    Held,
    /// Not in the save directory. Only bytes that share pieces with wanted files are kept,
    /// in the torrent's part store.
    Parts,
}

/// What happens to a file that is deselected after some of it was downloaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum UnwantedPolicy {
    /// Move it to the held area (the default).
    #[default]
    Hold,
    /// Delete it.
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SetUnwantedPolicyRequest {
    pub policy: UnwantedPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ReleaseHeldRequest {
    /// [`TorrentFile::index`] of each file to release. Omit to release every held file.
    #[serde(default)]
    pub files: Option<Vec<u32>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct TorrentDetail {
    pub summary: TorrentSummary,
    pub files: Vec<TorrentFile>,
    pub trackers: Vec<Vec<String>>,
    pub comment: Option<String>,
    pub created_by: Option<String>,
    #[ts(type = "number | null")]
    pub creation_date: Option<i64>,
    #[ts(type = "number")]
    pub piece_length: u64,
    pub magnet: String,
    pub unwanted_policy: UnwantedPolicy,
    pub limits: TorrentLimits,
    /// The naming the torrent's files were laid out with.
    pub naming: NamingPolicy,
    pub peers: Vec<PeerInfo>,
    pub tracker_status: Vec<TrackerInfo>,
    /// The torrent's own seed limits (`null`: its group's or the instance's).
    pub seed_limits: Option<SeedLimits>,
    /// The limits that apply, wherever they come from.
    pub effective_seed_limits: SeedLimits,
    /// Time spent seeding, across restarts.
    #[ts(type = "number")]
    pub seeding_secs: u64,
    /// The limits were reached and acted on. A torrent resumed by hand afterwards seeds on
    /// until its own limits change.
    pub seed_limit_reached: bool,
}

/// A connected peer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PeerInfo {
    /// `ip:port`.
    pub address: String,
    pub client: String,
    pub progress: f64,
    #[ts(type = "number")]
    pub download_rate: u64,
    #[ts(type = "number")]
    pub upload_rate: u64,
    #[ts(type = "number")]
    pub downloaded: u64,
    #[ts(type = "number")]
    pub uploaded: u64,
    /// One letter each, like other clients show: `O` outbound, `C` we choke it, `I` we're
    /// interested, `c` it chokes us, `i` it's interested, `P` on parole, `U` over uTP,
    /// `E` encrypted.
    pub flags: String,
}

/// A tracker and how its last announce went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct TrackerInfo {
    pub url: String,
    pub tier: u32,
    pub peers: Option<u32>,
    pub seeders: Option<u32>,
    pub leechers: Option<u32>,
    pub error: Option<String>,
    pub warning: Option<String>,
}

/// Session-wide numbers for a status bar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SessionStats {
    pub connections: u32,
    #[ts(type = "number")]
    pub memory_used: u64,
    #[ts(type = "number")]
    pub memory_limit: u64,
    pub banned: u32,
    /// The port peers connect to.
    pub peer_port: u16,
    /// Nodes in the DHT's routing tables, or null when the DHT is off.
    pub dht_nodes: Option<u32>,
}

/// A torrent's own limits. 0 or null means no limit of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct TorrentLimits {
    /// Bytes per second.
    #[ts(type = "number")]
    #[serde(default)]
    pub download_rate: u64,
    #[ts(type = "number")]
    #[serde(default)]
    pub upload_rate: u64,
    /// Peers at once; null uses the server's default.
    #[serde(default)]
    pub max_connections: Option<u32>,
}

/// When a finished torrent stops seeding. 0 means no limit of that kind; with neither
/// limit set it seeds for ever.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
pub struct SeedLimits {
    /// Stop at this share ratio: uploaded divided by downloaded (by the size, for a
    /// torrent that downloaded nothing, such as one that came from the library).
    pub ratio: f64,
    /// Stop after seeding this many minutes in total, across restarts.
    pub seed_minutes: u32,
    pub action: SeedLimitAction,
}

impl SeedLimits {
    /// Whether either limit is set.
    pub fn active(&self) -> bool {
        self.ratio > 0.0 || self.seed_minutes > 0
    }
}

/// What happens to a torrent that reaches its seed limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum SeedLimitAction {
    /// Pause it. Resuming it by hand lets it seed on.
    #[default]
    Pause,
    /// Remove it from apitorrent; its files stay.
    Remove,
    /// Remove it and delete its files.
    RemoveWithData,
}

/// A torrent's own seed limits. `null` follows its group's, or the instance's.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SetSeedLimitsRequest {
    pub limits: Option<SeedLimits>,
}

/// Instance-wide settings. Rates are bytes per second; 0 means unlimited throughout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Settings {
    #[ts(type = "number")]
    pub download_rate: u64,
    #[ts(type = "number")]
    pub upload_rate: u64,
    /// Open peer connections across all torrents.
    pub max_connections: u32,
    /// Outgoing connection attempts in progress at once.
    pub max_half_open: u32,
    /// New outgoing connection attempts per second.
    pub connect_rate: u32,
    pub queue: QueueSettings,
    pub groups: Vec<GroupSettings>,
    /// How file names are made safe for new torrents. Existing torrents keep theirs.
    #[serde(default)]
    pub naming: NamingPolicy,
    #[serde(default)]
    pub library: LibrarySettings,
    /// Encryption for peer connections. New connections use a change; open ones keep
    /// theirs.
    #[serde(default)]
    pub encryption: EncryptionPolicy,
    /// When finished torrents stop seeding, unless their group or the torrent itself says
    /// otherwise.
    #[serde(default)]
    pub seed_limits: SeedLimits,
}

/// Message stream encryption (MSE) for peer connections. It hides BitTorrent traffic from
/// passive inspection (and ISP throttling); it doesn't authenticate the peer. Data
/// integrity never depends on it: every piece is checked against the torrent's hashes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum EncryptionPolicy {
    /// Plain BitTorrent only.
    Disabled,
    /// Encrypt when the peer can, fall back to plain BitTorrent otherwise. The default.
    #[default]
    Enabled,
    /// Like `Enabled`, but only obfuscate the handshake when the peer allows it, leaving
    /// the payload unencrypted.
    PreferPlaintext,
    /// Encrypted connections only; peers without encryption are refused.
    Required,
}

/// Reusing files that other torrents already have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
#[allow(clippy::struct_excessive_bools, reason = "independent on/off settings")]
pub struct LibrarySettings {
    /// Look for a new torrent's files among existing torrents before downloading them.
    pub enabled: bool,
    /// Copy matches that can't be hard-linked (another volume). Off means download them.
    pub allow_copy: bool,
    /// Reuse a file that matches only in places (a repack that changed one part, say): it
    /// is cloned or copied, and only the pieces that differ are downloaded.
    pub partial_reuse: bool,
    /// Fingerprint files from v1 torrents in idle time (their v2 root and SHA-256), so a
    /// later v2 torrent of the same file matches without reading it. At most this many
    /// bytes per second; 0 turns it off.
    #[ts(type = "number")]
    pub fingerprint_rate: u64,
    /// Remove a torrent whose files another torrent has fully taken over (a season pack
    /// replacing single episodes) once its tracker says it is unregistered. Its files stay.
    pub retire_superseded: bool,
}

impl Default for LibrarySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_copy: true,
            partial_reuse: true,
            fingerprint_rate: 16 * 1024 * 1024,
            retire_superseded: true,
        }
    }
}

/// How torrent file names are turned into names on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum NamingPolicy {
    /// The same safe names on every OS, so a download directory can be shared over SMB
    /// or moved to exFAT. The default.
    #[default]
    Portable,
    /// Only what the save directory's filesystem forbids is changed. Falls back to
    /// portable when the filesystem needs it (NTFS, exFAT, FAT, SMB shares).
    Native,
}

/// How many torrents run at once. The rest wait, in queue order. 0 means no limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
pub struct QueueSettings {
    /// Torrents still downloading.
    pub max_active_downloads: u32,
    /// Finished torrents that are seeding.
    pub max_active_seeds: u32,
    /// Both together. Slow torrents count toward this one too.
    pub max_active: u32,
    /// Slow torrents don't count toward the download and seed limits, so a dead swarm
    /// doesn't hold a slot. They keep running, and the next torrent in the queue starts.
    pub ignore_slow: bool,
    /// A downloading torrent is slow below this rate, in bytes per second.
    #[ts(type = "number")]
    pub slow_download_rate: u64,
    /// A seeding torrent is slow below this upload rate, in bytes per second.
    #[ts(type = "number")]
    pub slow_upload_rate: u64,
    /// How long a torrent must stay below its rate to be slow. A torrent that has just
    /// started gets this long to find peers.
    pub slow_after_secs: u32,
}

impl Default for QueueSettings {
    /// No limits. The slow-torrent thresholds are libtorrent's (2 KiB/s, 60 s).
    fn default() -> Self {
        Self {
            max_active_downloads: 0,
            max_active_seeds: 0,
            max_active: 0,
            ignore_slow: false,
            slow_download_rate: 2048,
            slow_upload_rate: 2048,
            slow_after_secs: 60,
        }
    }
}

impl QueueSettings {
    /// Whether any limit is set, so torrents wait for the queue's decision.
    pub fn limited(&self) -> bool {
        self.max_active_downloads != 0 || self.max_active_seeds != 0 || self.max_active != 0
    }
}

/// Limits shared by every torrent whose label is in `labels`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct GroupSettings {
    pub name: String,
    pub labels: Vec<String>,
    #[ts(type = "number")]
    pub download_rate: u64,
    #[ts(type = "number")]
    pub upload_rate: u64,
    pub max_connections: u32,
    /// Seed limits for the group's torrents, in place of the instance's. `null` follows the
    /// instance.
    #[serde(default)]
    pub seed_limits: Option<SeedLimits>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SetLabelRequest {
    pub label: Option<String>,
}

/// Where to move a torrent in the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum QueueMove {
    Top,
    Up,
    Down,
    Bottom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct QueueMoveRequest {
    pub to: QueueMove,
}

/// Where a new torrent comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
#[ts(export)]
pub enum AddSource {
    /// A `.torrent` file, base64-encoded.
    TorrentFile {
        data: String,
    },
    Magnet {
        uri: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct AddTorrentRequest {
    pub source: AddSource,
    /// Defaults to the server's download directory.
    #[serde(default)]
    pub save_path: Option<String>,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub label: Option<String>,
    /// One per file. Omit to download everything.
    #[serde(default)]
    pub file_priorities: Option<Vec<u8>>,
    #[serde(default)]
    pub unwanted_policy: UnwantedPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct AddTorrentResponse {
    pub id: TorrentId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SetFilePrioritiesRequest {
    /// Files to change; the rest keep their priority.
    pub priorities: Vec<FilePriority>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct FilePriority {
    /// [`TorrentFile::index`].
    pub index: u32,
    /// 0 skips the file; up to 7.
    pub priority: u8,
}

/// The body of every error response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ApiError {
    /// Stable, machine-readable: `not_found`, `invalid_torrent`, `duplicate`, …
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ServerInfo {
    pub version: String,
    pub api_version: u32,
    /// Feature flags clients can check before using optional features.
    pub capabilities: Vec<String>,
}

/// A directory the daemon manages downloads in. Clients map it to a local path (a network
/// share, say) to open its files, and check the mapping through its marker file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct StorageRoot {
    /// Random, and stable for this path; also written into the root's marker file.
    pub id: String,
    /// The path on the daemon's machine.
    pub path: String,
}

/// The contents of `.apitorrent-root`, the marker file at the top of every storage root.
/// A client trusts a path mapping only once it reads the right id through the local path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct RootMarker {
    /// Always [`RootMarker::KIND`], so a stray file of the same name isn't taken for one.
    pub kind: String,
    pub id: String,
}

impl RootMarker {
    /// The marker's file name.
    pub const FILE_NAME: &'static str = ".apitorrent-root";
    pub const KIND: &'static str = "apitorrent-storage-root";

    pub fn new(id: impl Into<String>) -> Self {
        Self {
            kind: Self::KIND.to_owned(),
            id: id.into(),
        }
    }

    /// Parse a marker file; `None` if it isn't one.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice::<Self>(bytes)
            .ok()
            .filter(|m| m.kind == Self::KIND && !m.id.is_empty())
    }
}

/// Import a Deluge installation's torrents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ImportDelugeRequest {
    /// Deluge's config directory on the machine running apitorrent (the one holding
    /// `state/`), such as `~/.config/deluge`.
    pub config_dir: String,
    /// Rewrite save paths that start with `from` to start with `to` instead, for when
    /// Deluge ran elsewhere or saw the downloads under another path.
    #[serde(default)]
    pub path_map: Vec<PathMapping>,
    /// Also take Deluge's speed, connection and queue limits.
    #[serde(default)]
    pub import_settings: bool,
    /// Add everything paused, whatever its state in Deluge.
    #[serde(default)]
    pub paused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PathMapping {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ImportReport {
    /// Names of the torrents added.
    pub imported: Vec<String>,
    pub skipped: Vec<ImportSkip>,
    /// Problems that didn't stop the import.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ImportSkip {
    pub name: String,
    pub reason: String,
}

/// Why an address is banned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum BanReason {
    /// Sent data that failed verification, with proof.
    BadData,
    /// Took part in too many failed pieces, without proof.
    Untrusted,
    /// Banned by hand.
    Manual,
}

/// A banned peer address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Ban {
    pub ip: String,
    pub reason: BanReason,
    /// How many times it has been banned this session; each repeat doubles the length.
    pub count: u32,
    /// When the ban ends, in milliseconds since the Unix epoch.
    #[ts(type = "number")]
    pub until: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct BanRequest {
    pub ip: String,
}

/// Messages from the server on the `/api/v1/sync` WebSocket.
///
/// The server keeps a revisioned store. A client first gets a `snapshot` at some revision,
/// then `patch`es that each advance the revision by one. If a client falls behind, the
/// server sends a fresh snapshot instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(export)]
pub enum SyncMessage {
    #[serde(rename_all = "camelCase")]
    Snapshot {
        #[ts(type = "number")]
        revision: u64,
        torrents: Vec<TorrentSummary>,
    },
    #[serde(rename_all = "camelCase")]
    Patch {
        #[ts(type = "number")]
        revision: u64,
        upserted: Vec<TorrentSummary>,
        removed: Vec<TorrentId>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shape() {
        let req: AddTorrentRequest = serde_json::from_str(
            r#"{"source":{"kind":"magnet","uri":"magnet:?xt=urn:btih:00"},"paused":true}"#,
        )
        .unwrap();
        assert_eq!(
            req.source,
            AddSource::Magnet {
                uri: "magnet:?xt=urn:btih:00".into()
            }
        );
        assert!(req.paused);
        assert_eq!(req.save_path, None);

        let msg = SyncMessage::Patch {
            revision: 3,
            upserted: vec![],
            removed: vec!["ab".into()],
        };
        assert_eq!(
            serde_json::to_string(&msg).unwrap(),
            r#"{"type":"patch","revision":3,"upserted":[],"removed":["ab"]}"#
        );
    }
}

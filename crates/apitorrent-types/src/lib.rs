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
pub mod automation;
pub mod bulk;
pub mod files;
pub mod library;
pub mod plugins;
pub mod share;
pub mod spaces;
pub mod tls;
pub mod update;
pub mod users;

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
    /// Its files are being moved to another folder, or waiting their turn to be.
    Moving,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct TorrentSummary {
    pub id: TorrentId,
    /// The space the torrent is in (see [`spaces`]). The same torrent can be in several,
    /// each with its own summary. Daemons from before spaces don't send it: `default`.
    #[ts(type = "number")]
    #[serde(default = "spaces::default_space")]
    pub space: spaces::SpaceId,
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
    /// Imported from Deluge, which may still run it: the Deluge config directory it came
    /// from. Starting it here pauses it in Deluge first. Daemons from before this don't
    /// send it.
    #[serde(default)]
    pub in_deluge: Option<String>,
    /// Imported from a `.torrent` file left in this Deluge config directory's state
    /// folder, which Deluge itself didn't list (so it never ran there).
    #[serde(default)]
    pub leftover_from: Option<String>,
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
    /// Where the file's bytes came from, when the content library supplied some of them
    /// instead of peers. Daemons from before this don't send it.
    #[serde(default)]
    pub reused: Option<FileReuse>,
    /// Downloaded front to back, to be read while it downloads.
    #[serde(default)]
    pub sequential: bool,
    /// Where the file was moved when it completed (see [`files::MoveCompleted`]); `null`
    /// while it is in the save directory.
    #[serde(default)]
    pub moved_to: Option<String>,
    /// Why moving it failed, the last time it was tried.
    #[serde(default)]
    pub move_error: Option<String>,
}

/// How much of a file the content library supplied, and from where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct FileReuse {
    /// The torrent the bytes came from. It may have been removed since.
    pub source_id: TorrentId,
    /// Its name, as it was then.
    pub source_name: String,
    pub method: ReuseMethod,
    /// Bytes of the file that were verified without being downloaded.
    #[ts(type = "number")]
    pub bytes: u64,
}

/// How the content library supplied a file's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum ReuseMethod {
    /// A hard link: both torrents share one file on disk.
    HardLink,
    /// A copy-on-write clone (Btrfs, XFS, APFS, `ReFS`): a private file sharing extents.
    Clone,
    /// A plain copy (another volume).
    Copy,
    /// Piece by piece, from a torrent with the same file that was still downloading too.
    PieceSharing,
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
    /// Bytes of this torrent's files the content library supplied, so they weren't
    /// downloaded (the sum of the files' `reused` bytes).
    #[ts(type = "number")]
    #[serde(default)]
    pub library_bytes: u64,
    /// The torrent's own file order (`null`: its group's or the instance's).
    #[serde(default)]
    pub file_order: Option<files::FileOrder>,
    /// The file order that applies, wherever it comes from.
    #[serde(default)]
    pub effective_file_order: files::FileOrder,
    /// The torrent's own move-when-complete setting (`null`: its group's or the
    /// instance's).
    #[serde(default)]
    pub move_completed: Option<files::MoveCompleted>,
    /// The setting that applies, wherever it comes from.
    #[serde(default)]
    pub effective_move_completed: files::MoveCompleted,
    /// Each file's first and last piece first (see [`files::SetFirstLastRequest`]).
    #[serde(default)]
    pub first_last: bool,
    /// Super-seeding (see [`files::SetSuperSeedRequest`]).
    #[serde(default)]
    pub super_seed: bool,
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
    /// The router's port forwards; null when port forwarding is off.
    #[serde(default)]
    pub port_forward: Option<PortForwardStatus>,
    /// Whether peers on the internet can connect in, and our public address.
    #[serde(default)]
    pub reachability: Reachability,
    /// Bytes the content library supplied across the torrents here, so they weren't
    /// downloaded.
    #[ts(type = "number")]
    #[serde(default)]
    pub library_bytes: u64,
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
    /// Peers uploaded to at once; null uses the server's default.
    #[serde(default)]
    pub upload_slots: Option<u32>,
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
    /// The port peers connect to (TCP, and UDP for uTP). Picked at random from a high
    /// range on the first start and kept; 0 picks a new one on the next start. Applies on
    /// restart. The daemon's `--peer-listen` (or a desktop profile's port) overrides it.
    #[serde(default)]
    pub peer_port: u16,
    /// The DHT's UDP port, chosen and kept like `peer_port`. `--dht-port` overrides it.
    #[serde(default)]
    pub dht_port: u16,
    /// Finding peers beyond the trackers, and opening the ports on the router. Applies on
    /// restart; the daemon's `--no-dht` and `--no-lsd` turn those off whatever this says.
    #[serde(default)]
    pub discovery: DiscoverySettings,
    /// Which client apitorrent presents itself as, per tracker.
    #[serde(default)]
    pub identity: IdentitySettings,
    /// The order torrents' files download in, unless their group or the torrent itself
    /// says otherwise.
    #[serde(default)]
    pub file_order: files::FileOrder,
    /// Moving completed files, unless their group or the torrent itself says otherwise.
    #[serde(default)]
    pub move_completed: files::MoveCompleted,
    /// A proxy for peer, web seed and tracker connections. Applies at once to new
    /// connections.
    #[serde(default)]
    pub proxy: ProxySettings,
}

impl Settings {
    /// The settings as the API shows them: secrets left out (see
    /// [`ProxySettings::password`]).
    #[must_use]
    pub fn redacted(mut self) -> Self {
        self.proxy.password_set = self.proxy.password.as_ref().is_some_and(|p| !p.is_empty());
        self.proxy.password = None;
        self
    }
}

/// The kind of proxy, or none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum ProxyType {
    /// No proxy: every connection is direct. The other proxy settings are kept.
    #[default]
    None,
    /// SOCKS4, or SOCKS4a when the proxy resolves host names. IPv4 peers only; the
    /// username is sent as the SOCKS4 user id, and there is no password.
    Socks4,
    /// SOCKS5, with or without a username and password. The only kind that carries UDP
    /// trackers.
    Socks5,
    /// An HTTP proxy that allows `CONNECT`, with or without Basic authentication.
    Http,
}

/// A proxy for outgoing connections. The options follow Deluge's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
#[allow(clippy::struct_excessive_bools, reason = "independent on/off settings")]
pub struct ProxySettings {
    #[serde(rename = "type")]
    pub kind: ProxyType,
    /// The proxy's host name or IP address.
    pub host: String,
    pub port: u16,
    /// Empty for a proxy without authentication.
    pub username: String,
    /// Write-only: the API never sends it back (see `password_set`). A settings update
    /// without it keeps the stored password; an empty string removes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub password: Option<String>,
    /// Read-only: whether a password is stored. Ignored in updates.
    pub password_set: bool,
    /// Let the proxy resolve host names (trackers', and peers' given by name), so they
    /// never reach the local resolver.
    pub proxy_hostnames: bool,
    /// Peer connections and web seeds go through the proxy. They are TCP only then: uTP
    /// can't go through a proxy.
    pub proxy_peers: bool,
    /// Tracker announces and scrapes go through the proxy. Through an HTTP or SOCKS4
    /// proxy, UDP trackers can't be reached and are skipped.
    pub proxy_trackers: bool,
    /// Everything that can go through the proxy does, and everything that can't is off:
    /// no incoming connections, no DHT, no local peer discovery, no uTP, no UPnP or
    /// NAT-PMP port forwarding, whatever [`DiscoverySettings`] says. Turning it on takes
    /// effect at once; turning it off brings those back on the next start.
    pub force: bool,
}

impl Default for ProxySettings {
    fn default() -> Self {
        Self {
            kind: ProxyType::None,
            host: String::new(),
            port: 1080,
            username: String::new(),
            password: None,
            password_set: false,
            proxy_hostnames: true,
            proxy_peers: true,
            proxy_trackers: true,
            force: false,
        }
    }
}

/// Finding peers beyond the trackers (none of it for private torrents), and forwarding the
/// peer and DHT ports on the router so peers outside can connect in. All on by default.
#[allow(clippy::struct_excessive_bools, reason = "four independent switches")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
pub struct DiscoverySettings {
    /// The Mainline DHT (BEP 5).
    pub dht: bool,
    /// Local service discovery (BEP 14): peers on the same network, by multicast.
    pub lsd: bool,
    /// Port forwarding by UPnP IGD.
    pub upnp: bool,
    /// Port forwarding by NAT-PMP (RFC 6886), tried before UPnP.
    pub natpmp: bool,
}

impl Default for DiscoverySettings {
    fn default() -> Self {
        Self {
            dht: true,
            lsd: true,
            upnp: true,
            natpmp: true,
        }
    }
}

/// The router's port forwards, as last tried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PortForwardStatus {
    /// `"natPmp"` or `"upnp"` while forwards are in place; null otherwise.
    pub method: Option<String>,
    pub external_ip: Option<String>,
    /// Why there are none, after the last attempt.
    pub error: Option<String>,
}

/// Whether peers on the internet can connect to us, judged from what has happened: an
/// incoming connection from a public address proves a port open; none for a long while
/// after the swarms learned our address suggests it is closed.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
pub struct Reachability {
    pub ipv4: FamilyReachability,
    pub ipv6: FamilyReachability,
}

/// One address family's public address and ports.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
pub struct FamilyReachability {
    /// Our public address as the internet sees it; null until something has said.
    pub external_ip: Option<String>,
    /// What each source says it is: DHT nodes (by vote), peers (by majority), the last
    /// HTTP tracker to say, and the router (NAT-PMP or UPnP).
    pub from_dht: Option<String>,
    pub from_peers: Option<String>,
    pub from_tracker: Option<String>,
    pub from_router: Option<String>,
    /// The router's public address isn't the one the internet sees: another NAT (often
    /// the internet provider's) is in front of it, so a forward on the router can't open
    /// the port.
    pub double_nat: bool,
    /// Each port, or null when it doesn't listen on this family.
    pub tcp: Option<PortReachability>,
    pub utp: Option<PortReachability>,
    pub dht: Option<PortReachability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PortReachability {
    pub port: u16,
    pub state: PortState,
    /// Connections that came in on their own since the daemon started.
    #[ts(type = "number")]
    pub incoming: u64,
    /// When the last of them came, in milliseconds since the Unix epoch.
    #[ts(type = "number | null")]
    pub last_incoming: Option<i64>,
    /// Since when we've been waiting for one (ms since the epoch); null while no swarm
    /// knows our address yet.
    #[ts(type = "number | null")]
    pub waiting_since: Option<i64>,
    /// The last port check since our public address last changed (TCP only).
    #[serde(default)]
    pub check: Option<PortCheckResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum PortState {
    /// A connection came in on its own, or a port check got through, since our public
    /// address last changed.
    Open,
    /// None yet, and it's too soon to say.
    Waiting,
    /// None for half an hour since the swarms learned our address.
    ProbablyClosed,
    /// A port check couldn't connect, and nothing has come in since.
    Closed,
}

/// A public service that connects back to the port it's given, at the address asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum PortCheckService {
    /// Transmission's port test, `portcheck.transmissionbt.com`.
    Transmission,
    /// `ifconfig.co`.
    Ifconfig,
}

/// What a port check service found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PortCheckResult {
    pub service: PortCheckService,
    pub open: bool,
    /// When, in milliseconds since the Unix epoch.
    #[ts(type = "number")]
    pub at: i64,
}

/// `POST /api/v1/port-check`: ask a public service to connect back to the TCP peer port
/// from the daemon, once per address family it listens on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PortCheckRequest {
    pub service: PortCheckService,
}

/// The answer per family; null where the peer port doesn't listen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PortCheckResponse {
    pub ipv4: Option<PortCheckAnswer>,
    pub ipv6: Option<PortCheckAnswer>,
}

/// Whether the service got through, or why there's no answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PortCheckAnswer {
    pub open: Option<bool>,
    pub error: Option<String>,
}

/// Which client apitorrent presents itself as: its peer id, the `User-Agent` of HTTP
/// announces and the client name in the extension handshake. Some private trackers only
/// accept clients on a list; many others forbid presenting as another client and ban
/// accounts that do. Use another identity only where the tracker allows or needs it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
pub struct IdentitySettings {
    /// The identity of torrents no rule matches. `apitorrent` by default.
    pub default: ClientIdentity,
    /// Per-tracker identities. A torrent takes the first rule that matches the host of any
    /// of its trackers.
    pub rules: Vec<TrackerIdentityRule>,
}

/// The identity a torrent uses when one of its trackers is on `host`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct TrackerIdentityRule {
    /// A host name (`tracker.example.org`), or `*.example.org` for that domain and every
    /// name under it. Case doesn't matter; the port is ignored.
    pub host: String,
    pub identity: ClientIdentity,
}

/// A client to present as. Versions are dotted numbers (`2.1.1`); each part goes into the
/// peer id as one character, so parts above 35 can't be written.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(tag = "client", rename_all = "camelCase")]
#[ts(export)]
pub enum ClientIdentity {
    /// apitorrent itself: peer id `-ATxyz0-`.
    #[default]
    Apitorrent,
    /// Deluge on libtorrent: peer id `-DE211s-`, user agent
    /// `Deluge/2.1.1 libtorrent/2.0.10.0`.
    Deluge { version: String, libtorrent: String },
    /// qBittorrent (on libtorrent): peer id `-qB4650-`, user agent `qBittorrent/4.6.5`.
    QBittorrent { version: String },
    /// Transmission: peer id `-TR4060-`, user agent `Transmission/4.0.6`.
    Transmission { version: String },
    /// Anything else. The peer id is the prefix followed by random characters to 20 bytes.
    Custom {
        #[serde(rename = "peerIdPrefix")]
        peer_id_prefix: String,
        #[serde(rename = "userAgent")]
        user_agent: String,
        /// Sent to peers in the extension handshake (`v`).
        #[serde(rename = "clientName")]
        client_name: String,
    },
}

impl ClientIdentity {
    /// Deluge 2.1.1 on libtorrent 2.0.10, the newest stable Deluge release.
    pub fn deluge_default() -> Self {
        Self::Deluge {
            version: "2.1.1".into(),
            libtorrent: "2.0.10.0".into(),
        }
    }
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
    /// Merge identical files that torrents downloaded separately into hard links (see
    /// [`library::DedupeMode`]). Reads at no more than `fingerprint_rate`.
    pub dedupe: library::DedupeMode,
    /// How often library folders are walked again for new, changed and removed files, in
    /// hours. 0 means only when asked.
    pub folder_rescan_hours: u32,
    /// Share data between torrents while they run: a file completing in one is linked
    /// into others still waiting for it, and pieces one verifies fill the same pieces of
    /// others with the same file. At most this many bytes per second are read for it; 0
    /// turns it off.
    #[ts(type = "number")]
    pub share_rate: u64,
}

impl Default for LibrarySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_copy: true,
            partial_reuse: true,
            fingerprint_rate: 16 * 1024 * 1024,
            retire_superseded: true,
            dedupe: library::DedupeMode::On,
            folder_rescan_hours: 6,
            share_rate: 64 * 1024 * 1024,
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
    /// The file order for the group's torrents, in place of the instance's. `null` follows
    /// the instance.
    #[serde(default)]
    pub file_order: Option<files::FileOrder>,
    /// Moving the group's completed files, in place of the instance's setting. `null`
    /// follows the instance.
    #[serde(default)]
    pub move_completed: Option<files::MoveCompleted>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SetLabelRequest {
    pub label: Option<String>,
}

/// `PUT /torrents/{id}/trackers`: the entry's announce tiers, first tier first. Each URL
/// (`http`, `https` or `udp`) once; empty tiers are dropped, and an empty list announces
/// nowhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SetTrackersRequest {
    pub tiers: Vec<Vec<String>>,
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
    /// Where other machines reach this root: UNC paths, `smb://` and `nfs://` URLs (desktop
    /// clients suggest path mappings from them), and `http(s)://` URLs of a web server
    /// listing its files (the web UI links files there). Set by a server admin (see
    /// [`share`]).
    #[serde(default)]
    pub locators: Vec<String>,
}

/// `PUT /api/v1/roots/{id}/locators`: replace a storage root's share locators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SetLocatorsRequest {
    pub locators: Vec<String>,
}

/// `POST /api/v1/roots/{id}/locators/check`: check a web locator from the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct CheckLocatorRequest {
    /// An `http(s)://` locator, saved or not.
    pub locator: String,
}

/// What the daemon found when it fetched a web locator's marker file. Advisory: the
/// daemon may not reach a URL browsers can (its own public address, say), so a failed
/// check doesn't hide the web UI's links.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct LocatorCheck {
    /// The URL served this root's marker.
    pub verified: bool,
    /// What happened, to show the admin.
    pub message: String,
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
    /// Add everything paused, whatever its state in Deluge.
    #[serde(default)]
    pub paused: bool,
    /// Which torrents to import; the default takes them all.
    #[serde(default)]
    pub filter: DelugeImportFilter,
    /// What of each torrent, and of Deluge's settings, comes over; the default takes
    /// everything per torrent and none of Deluge's settings.
    #[serde(default)]
    pub options: DelugeImportOptions,
}

/// What a Deluge import carries over besides each torrent's files and progress (which
/// always come). The default carries everything per torrent and none of Deluge's own
/// settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
#[allow(clippy::struct_excessive_bools, reason = "one switch per option")]
pub struct DelugeImportOptions {
    /// Keep Deluge's labels (then `labelRenames` changes some).
    pub labels: bool,
    /// Labels to change on the way in: `from` (a Deluge label, compared without regard to
    /// case) becomes `to`, or no label when `to` is null or empty.
    pub label_renames: Vec<LabelRename>,
    /// Keep Deluge's queue order; otherwise they queue in the order they were added.
    pub queue_order: bool,
    /// Keep when each torrent was added to Deluge; otherwise now.
    pub added_dates: bool,
    /// Keep all-time downloaded and uploaded totals and seeding time (ratios and seed
    /// limits count from them); otherwise they start at zero.
    pub totals: bool,
    /// Files Deluge skips stay skipped; otherwise every file is downloaded.
    pub skipped_files: bool,
    /// Use Deluge's tracker lists (edited there, say for a new passkey); otherwise each
    /// `.torrent` file's own.
    pub trackers: bool,
    /// Keep each torrent's own speed and connection limits.
    pub torrent_limits: bool,
    /// Keep each torrent's own stop-at-ratio.
    pub seed_limits: bool,
    /// Keep Deluge's "move completed": the torrent's files move to that folder once it
    /// completes.
    pub move_completed: bool,
    /// Keep Deluge's sequential download: files in the torrent's order, each front to back.
    pub sequential: bool,
    /// Keep Deluge's "prioritize first and last pieces".
    pub first_last: bool,
    /// Keep Deluge's super-seeding.
    pub super_seed: bool,
    /// The client private trackers see for these torrents.
    pub identity: ImportIdentity,
    /// Tracker hosts to leave out of the identity rules.
    pub identity_skip_hosts: Vec<String>,
    /// Which of Deluge's own settings to take (into the space's settings, or the
    /// instance's for the default space).
    pub settings: DelugeSettingsPick,
}

impl Default for DelugeImportOptions {
    fn default() -> Self {
        Self {
            labels: true,
            label_renames: Vec::new(),
            queue_order: true,
            added_dates: true,
            totals: true,
            skipped_files: true,
            trackers: true,
            torrent_limits: true,
            seed_limits: true,
            move_completed: true,
            sequential: true,
            first_last: true,
            super_seed: true,
            identity: ImportIdentity::Deluge,
            identity_skip_hosts: Vec::new(),
            settings: DelugeSettingsPick::default(),
        }
    }
}

impl DelugeImportOptions {
    /// The label a torrent labelled `label` in Deluge gets here.
    #[must_use]
    pub fn label_for(&self, label: Option<&str>) -> Option<String> {
        let label = label.filter(|l| !l.is_empty())?;
        if !self.labels {
            return None;
        }
        match self
            .label_renames
            .iter()
            .find(|r| r.from.eq_ignore_ascii_case(label))
        {
            Some(r) => {
                r.to.as_deref()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(str::to_owned)
            }
            None => Some(label.to_owned()),
        }
    }
}

/// A Deluge label renamed (or dropped, with no `to`) on import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct LabelRename {
    pub from: String,
    pub to: Option<String>,
}

/// The client private trackers see for imported torrents.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
#[ts(export)]
pub enum ImportIdentity {
    /// Deluge, at the versions its daemon reports (or the newest release when it isn't
    /// running): what those trackers saw before.
    #[default]
    Deluge,
    /// No rules: apitorrent's own identity (or whatever rules already exist).
    None,
    /// Another identity.
    Other { identity: ClientIdentity },
}

/// Which of Deluge's settings an import takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
#[allow(clippy::struct_excessive_bools, reason = "one switch per setting")]
pub struct DelugeSettingsPick {
    pub download_rate: bool,
    pub upload_rate: bool,
    pub max_connections: bool,
    /// Active downloads, seeds and torrents in all.
    pub queue_slots: bool,
    /// Don't count slow torrents against the slots (the default space only; spaces have
    /// no such setting).
    pub ignore_slow: bool,
    /// Stop seeding at a ratio: the instance's seed limits for the default space; each
    /// torrent's own for another space.
    pub stop_at_ratio: bool,
    /// Encryption for peer connections (the default space only: it is the instance's).
    pub encryption: bool,
    /// DHT, local peer discovery, UPnP and NAT-PMP (the default space only).
    pub discovery: bool,
    /// The proxy (the default space only: it is the instance's).
    pub proxy: bool,
}

impl DelugeSettingsPick {
    /// Every setting.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            download_rate: true,
            upload_rate: true,
            max_connections: true,
            queue_slots: true,
            ignore_slow: true,
            stop_at_ratio: true,
            encryption: true,
            discovery: true,
            proxy: true,
        }
    }

    #[must_use]
    pub fn any(&self) -> bool {
        *self != Self::default()
    }
}

/// Deluge's own settings that an import can take, as Deluge has them.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DelugeSettingsView {
    /// Bytes per second; null for no limit.
    #[ts(type = "number | null")]
    pub download_rate: Option<u64>,
    #[ts(type = "number | null")]
    pub upload_rate: Option<u64>,
    pub max_connections: Option<u32>,
    pub max_active_downloading: Option<u32>,
    pub max_active_seeding: Option<u32>,
    pub max_active: Option<u32>,
    pub dont_count_slow: bool,
    pub stop_at_ratio: Option<StopAtRatio>,
    /// Deluge's encryption settings, as the nearest of apitorrent's.
    pub encryption: EncryptionPolicy,
    /// How Deluge's encryption maps, when it isn't one to one (incoming and outgoing set
    /// differently, say).
    pub encryption_note: Option<String>,
    /// DHT, local peer discovery, UPnP and NAT-PMP, as Deluge has them.
    pub discovery: DiscoverySettings,
    /// The settings changed in Deluge from Deluge's defaults.
    pub changed: DelugeSettingsPick,
    /// Deluge's proxy, as it would be here (without its password: `passwordSet` says
    /// whether it has one). `null` when Deluge uses no proxy, or one that can't come over
    /// (that is listed with what can't).
    pub proxy: Option<ProxySettings>,
}

/// Deluge's "stop seeding at ratio".
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct StopAtRatio {
    pub ratio: f64,
    /// Remove the torrent then, rather than pause it.
    pub remove: bool,
}

/// Which of a Deluge installation's torrents to import. Each list that isn't empty must
/// match (a torrent matches a list when it matches any entry); empty lists and `None`
/// don't filter.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
pub struct DelugeImportFilter {
    /// Labels to take; `""` is "no label". Compared without regard to case.
    pub labels: Vec<String>,
    /// Labels to leave out.
    pub exclude_labels: Vec<String>,
    /// Tracker hosts: `tracker.example.org`, or `example.org` for it and its subdomains.
    pub trackers: Vec<String>,
    /// Save paths (after `pathMap`): a torrent saved in one of these or below it.
    pub save_paths: Vec<String>,
    /// Only complete torrents (`true`) or only incomplete ones (`false`).
    pub complete: Option<bool>,
    /// Only torrents paused in Deluge (`true`) or only running ones (`false`).
    pub paused: Option<bool>,
    /// Words that must all appear in the name, ignoring case.
    pub name: Option<String>,
    /// Exactly these torrents, by info-hash (hex, as Deluge and apitorrent show them).
    pub ids: Vec<String>,
}

impl DelugeImportFilter {
    /// Whether it takes everything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// `POST /api/v1/import/deluge/preview`: what a Deluge installation holds, for choosing
/// what to import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DelugePreviewRequest {
    pub config_dir: String,
    #[serde(default)]
    pub path_map: Vec<PathMapping>,
    /// Preview for a space that doesn't exist yet: nothing is "already here" and every
    /// path fits.
    #[serde(default)]
    pub new_space: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DelugePreview {
    pub torrents: Vec<DelugePreviewTorrent>,
    /// Every label in use, then every tracker host, sorted: the filter's choices.
    pub labels: Vec<String>,
    pub trackers: Vec<String>,
    /// Deluge's default download folder (after `pathMap`).
    pub download_location: Option<String>,
    /// A space slug made from the config directory's name, for a new space.
    pub suggested_slug: String,
    pub warnings: Vec<String>,
    /// Deluge's own settings, which the import can take.
    pub settings: DelugeSettingsView,
    /// Label plugin options that don't carry over, in plain words.
    pub label_options: Vec<String>,
    /// The identity Deluge presents (its daemon's versions when it answers).
    pub deluge_identity: ClientIdentity,
    /// Tracker hosts an identity rule already covers: the import leaves them alone.
    pub covered_hosts: Vec<String>,
}

/// One torrent in a Deluge installation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
#[allow(clippy::struct_excessive_bools, reason = "facts about one torrent")]
pub struct DelugePreviewTorrent {
    /// The info-hash id, as apitorrent will know it.
    pub id: String,
    pub name: String,
    pub label: Option<String>,
    /// Hosts of its trackers.
    pub trackers: Vec<String>,
    /// Where its files are (after `pathMap`).
    pub save_path: String,
    #[ts(type = "number")]
    pub size: u64,
    /// Done, 0 to 1, from Deluge's resume data (`None`: Deluge never saved any).
    pub progress: Option<f64>,
    /// Not running in Deluge: paused there, or a leftover.
    pub paused: bool,
    /// A `.torrent` file left in Deluge's state folder that Deluge doesn't list, so it
    /// never runs there. Imported paused; `added_at` is when the file was written.
    #[serde(default)]
    pub leftover: bool,
    /// The space already has it; importing leaves it alone.
    pub present: bool,
    /// Why it can't be imported (outside the space's storage roots, unreadable).
    pub problem: Option<String>,
    /// A private torrent (its trackers may allow only known clients).
    pub private: bool,
    /// Its own speed and connection limits (zero / null: none).
    pub limits: TorrentLimits,
    /// Its own stop-at-ratio (Deluge copies the global one into every torrent).
    pub stop_at_ratio: Option<StopAtRatio>,
    /// Files Deluge doesn't download.
    pub skipped_files: u32,
    /// Files renamed in Deluge.
    pub renamed_files: u32,
    /// All-time totals, in bytes.
    #[ts(type = "number")]
    pub downloaded: u64,
    #[ts(type = "number")]
    pub uploaded: u64,
    #[ts(type = "number")]
    pub seeding_secs: u64,
    /// Deluge's queue position (0 first); null outside its queue.
    pub queue: Option<u32>,
    /// When it was added to Deluge, in milliseconds since the epoch.
    #[ts(type = "number | null")]
    pub added_at: Option<i64>,
    /// Deluge's tracker list differs from the `.torrent` file's.
    pub edited_trackers: bool,
    /// Where Deluge moves it when it completes (after `pathMap`); null when it doesn't.
    #[serde(default)]
    pub move_completed: Option<String>,
    /// Downloaded in order in Deluge.
    #[serde(default)]
    pub sequential: bool,
    /// First and last pieces first in Deluge.
    #[serde(default)]
    pub first_last: bool,
    /// Super-seeding in Deluge.
    #[serde(default)]
    pub super_seed: bool,
    /// Options set in Deluge that don't carry over, in plain words.
    pub not_carried: Vec<String>,
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
    /// How many torrents the filter left out.
    #[serde(default)]
    pub not_selected: u32,
    /// Tracker hosts that now get the Deluge client identity (the import's private
    /// torrents' trackers no identity rule covered yet).
    #[serde(default)]
    pub identity_hosts: Vec<String>,
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
///
/// A client that asks with `?chunks=1` may get a large snapshot in parts: the `snapshot`
/// carries the first torrents and `total`, then `snapshotPart`s follow until `total`
/// torrents have arrived. Patches come only after the last part.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(export)]
pub enum SyncMessage {
    #[serde(rename_all = "camelCase")]
    Snapshot {
        #[ts(type = "number")]
        revision: u64,
        torrents: Vec<TorrentSummary>,
        /// How many torrents the whole snapshot holds, when it comes in parts.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        total: Option<u32>,
    },
    /// More of the snapshot before it (only to a client that asked for parts).
    #[serde(rename_all = "camelCase")]
    SnapshotPart { torrents: Vec<TorrentSummary> },
    #[serde(rename_all = "camelCase")]
    Patch {
        #[ts(type = "number")]
        revision: u64,
        upserted: Vec<TorrentSummary>,
        removed: Vec<TorrentId>,
        /// The space of each removed torrent, in the same order. Sent only to a socket that
        /// watches every space (`?space=all`), where ids alone are ambiguous.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        #[ts(type = "Array<number>", optional)]
        removed_spaces: Vec<spaces::SpaceId>,
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
            removed_spaces: vec![],
        };
        assert_eq!(
            serde_json::to_string(&msg).unwrap(),
            r#"{"type":"patch","revision":3,"upserted":[],"removed":["ab"]}"#
        );
    }
}

/// `GET /api/v1/tracker-passkeys`: one tracker passkey found in the space's announce URLs. The
/// passkey itself is never sent; `id` names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct TrackerPasskey {
    /// Names this passkey in a replace request (a hash of it, not the passkey).
    pub id: String,
    /// Its first and last four characters, to tell it from another.
    pub masked: String,
    /// The tracker hosts it is used with, sorted.
    pub hosts: Vec<String>,
    /// How many of the space's torrents use it.
    pub torrents: u32,
}

/// `POST /api/v1/tracker-passkeys/replace`: put `new_passkey` in place of the passkey `id` in
/// every announce URL of the space's torrents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ReplacePasskeyRequest {
    pub id: String,
    pub new_passkey: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ReplacePasskeyReport {
    /// Torrents whose trackers now carry the new passkey.
    pub changed: u32,
    /// Torrents left as they were, and why (still fetching metadata, say).
    pub skipped: Vec<ImportSkip>,
}

/// `GET /api/v1/import/deluge/waiting`: imported torrents the client they came from may
/// still run, from one of its config directories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ImportSource {
    pub config_dir: String,
    pub torrents: u32,
}

/// `GET /api/v1/import/deluge/found`: a Deluge installation on the server, found by its
/// running daemon, its systemd unit, or where Deluge usually keeps its config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DelugeInstall {
    pub config_dir: String,
    /// Its systemd unit, else its user, else its folder.
    pub name: String,
    /// A slug for a space of its own.
    pub suggested_slug: String,
    /// A daemon is running with it.
    pub running: bool,
    pub units: Vec<String>,
    pub daemon_port: Option<u16>,
    /// `None` when it couldn't be read.
    pub torrents: Option<u32>,
    /// Why it can't be imported from, if it can't.
    pub problem: Option<String>,
    /// Its torrents already imported into this space.
    pub imported_here: u32,
    /// And into other spaces.
    pub imported_elsewhere: u32,
}

/// `POST /api/v1/import/deluge/take-over`: pause these imported torrents in Deluge and run
/// them here. No ids: every one still waiting in the space.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct TakeOverRequest {
    #[serde(default)]
    pub ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct TakeOverReport {
    /// Torrents now running here, paused in Deluge.
    pub taken_over: u32,
    /// Torrents left as they were, and why (Deluge couldn't be reached, say).
    pub skipped: Vec<ImportSkip>,
}

/// Work the server does in the background, so no request waits on it: a Deluge import, a
/// take-over. Started by a `POST …/jobs` that answers at once; `GET /api/v1/jobs/{id}`
/// reports its progress, then its outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Job {
    #[ts(type = "number")]
    pub id: u64,
    pub kind: JobKind,
    /// The space it works in.
    #[ts(type = "number")]
    pub space: spaces::SpaceId,
    /// Unix milliseconds.
    #[ts(type = "number")]
    pub started_at: i64,
    #[ts(type = "number | null")]
    pub finished_at: Option<i64>,
    pub progress: JobProgress,
    /// `None` while it runs.
    pub outcome: Option<JobOutcome>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum JobKind {
    DelugeImport,
    TakeOver,
}

/// How far a job has got.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct JobProgress {
    /// What it is doing now, in a few words ("Reading Deluge's state").
    pub phase: String,
    /// Steps done of `total`, across the whole job.
    pub done: u32,
    /// 0 while the size of the work isn't known yet.
    pub total: u32,
    /// The item being worked on (a torrent's name), if any.
    pub current: Option<String>,
}

/// How a job ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", tag = "kind")]
#[ts(export)]
pub enum JobOutcome {
    DelugeImport {
        report: ImportReport,
    },
    TakeOver {
        report: TakeOverReport,
    },
    /// It stopped with an error; whatever it finished before that stays done.
    Failed {
        message: String,
    },
}

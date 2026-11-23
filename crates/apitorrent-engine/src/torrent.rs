//! The per-torrent actor: owns the picker, the peer states and the piece buffers, and makes
//! every protocol decision for one torrent.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot, watch};

use apitorrent_metainfo::{InfoHashes, Sha1Hash, Sha256Hash, Torrent};
use apitorrent_wire::extended::{
    self, ExtensionHandshake, HolepunchError, HolepunchMessage, METADATA_PIECE_LEN,
    MetadataMessage, PexFlags, PexMessage, names,
};
use apitorrent_wire::{BlockRef, Decoder, Handshake, HashRequest, MAX_BLOCK_LEN, Message, PeerId};

use crate::announce::{Announcer, Job, Totals, TrackerStatus};
use crate::ban::BanReason;
use crate::bitfield::Bitfield;
use crate::hashreq::LayerShape;
use crate::identity::{ActiveIdentity, WireStyle};
use crate::layout::{BLOCK_LEN, Layout};
use crate::limits::{Slot, Throttle, TorrentLimits};
use crate::peer::{self, PeerCmd, PeerEvent};
use crate::picker::{PeerKey, Picker, Received};
use crate::reputation::Reputation;
use crate::session::Shared;
use crate::storage::{AdoptMode, Adopted, FileStorage, Placement, Storage, StoragePaths};
use crate::verify::{PieceHashes, Verdict};
use crate::webseed::{self, WebSeedEvent, WebSeedTorrent};

const TICK: Duration = Duration::from_secs(1);
/// At most this many PEX uTP hints are kept per torrent (each PEX message adds up to 50).
const MAX_UTP_HINTS: usize = 2000;
const CHOKE_EVERY: u32 = 10;
/// Pieces a super-seed offers each peer at once (BEP 16). libtorrent offers two, so a
/// peer that hasn't passed one on yet still has another to fetch.
const SUPERSEED_OFFERS: usize = 2;
const OPTIMISTIC_EVERY: u32 = 3;
/// How long to wait before reconnecting to a peer whose connection ended normally.
const RECONNECT_AFTER: Duration = Duration::from_secs(10);
/// How long to wait before retrying a peer we failed to reach.
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// How long to wait for a hash request to be answered before asking another peer.
const HASH_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// A web seed failing this many times in a row, or sending this many bad pieces, is
/// dropped.
const WEBSEED_MAX_FAILURES: u32 = 10;
const WEBSEED_MAX_BAD_PIECES: u32 = 3;

/// Our view of one web seed.
struct WebSeed {
    url: String,
    /// The piece it is fetching.
    piece: Option<u32>,
    failures: u32,
    bad_pieces: u32,
    disabled: bool,
}

/// A piece layer being fetched from peers (BEP 52).
struct LayerFetch {
    file: usize,
    shape: LayerShape,
    chunks: Vec<Option<Vec<Sha256Hash>>>,
    asked: HashMap<u32, (PeerKey, Instant)>,
    /// Peers that refused or answered wrongly; not asked again.
    refused: HashSet<PeerKey>,
}

/// Our extension ids, sent in the extension handshake. libtorrent's extensions get the ids
/// libtorrent gives them.
const OUR_UT_METADATA: u8 = 1;
const OUR_UT_PEX: u8 = 2;
const OUR_UPLOAD_ONLY: u8 = 3;
const OUR_UT_HOLEPUNCH: u8 = 4;
const OUR_LT_DONTHAVE: u8 = 7;
/// A peer's send budget, piece bytes being read or waiting to go out: about a second of its
/// upload rate, within these bounds.
const SEND_BUDGET_MIN: u64 = 512 * 1024;
const SEND_BUDGET_MAX: u64 = 4 * 1024 * 1024;
/// Most bytes read from disk in one go: queued requests for consecutive blocks of a piece
/// are read together.
const READ_RUN_MAX: u32 = 256 * 1024;
/// Requests we keep outstanding to a peer: at least the session's `request_queue`, and up
/// to this many seconds of its download rate (libtorrent's `request_queue_time`), within
/// what the peer takes.
const REQUEST_QUEUE_SECS: u64 = 3;
/// The most requests outstanding to one peer, and what we assume a peer takes when its
/// handshake doesn't say (libtorrent's defaults, `max_out_request_queue` and 250).
const MAX_REQUEST_QUEUE: usize = 500;
const DEFAULT_PEER_REQQ: usize = 250;
/// Most bookend pieces started from disk and not yet finished, per torrent. Each holds a
/// piece buffer until peers send the rest.
const MAX_PREFILLED: usize = 4;

/// What the outside world sees of a torrent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TorrentState {
    Checking,
    Downloading,
    Seeding,
    Paused,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TorrentStats {
    pub state: TorrentState,
    pub pieces_have: u32,
    pub pieces_total: u32,
    /// Pieces of the files the user wants (priority above 0), and how many of those we
    /// have. Progress means `wanted_have / wanted_total`.
    pub wanted_total: u32,
    pub wanted_have: u32,
    /// Every wanted piece is here. Stays accurate while paused.
    pub finished: bool,
    /// Payload bytes received and sent in this session.
    pub downloaded: u64,
    pub uploaded: u64,
    /// Bytes per second over the last tick.
    pub download_rate: u64,
    pub upload_rate: u64,
    pub peers: u32,
    pub seeds: u32,
    pub error: Option<String>,
    /// Each entry's own share, oldest first (see [`EntryParams`]).
    pub entries: Vec<EntryStats>,
}

/// Which entry of a torrent something belongs to. A torrent has one entry per space that
/// holds it, so most have exactly one.
pub type EntryKey = u64;

/// One entry of a torrent: a space's claim on the swarm. The swarm, the pieces and the
/// storage are shared; each entry has its own trackers, limits, pause state and transfer
/// counts. Every peer connection belongs to exactly one entry, whose limits it draws from and
/// whose counts it adds to:
///
/// - a peer a tracker returned belongs to the entry that announced to that tracker;
/// - DHT, PEX, LSD, web seed and incoming peers belong to the oldest running entry.
///
/// An entry reports to its trackers only its own peers' traffic, so data another entry
/// brought in is never claimed as downloaded (or uploaded) twice. An announce URL that
/// several running entries list is announced by the oldest of them only.
#[derive(Debug, Clone)]
pub struct EntryParams {
    pub key: EntryKey,
    /// Announce tiers; `None` uses the torrent's own.
    pub trackers: Option<Vec<Vec<String>>>,
    pub limits: TorrentLimits,
    /// The group whose shared limits apply too (see [`crate::Session::set_group`]).
    pub group: Option<String>,
    /// The space whose shared limits apply too (see [`crate::Session::set_space`]).
    pub space: Option<String>,
    pub paused: bool,
}

impl EntryParams {
    pub fn new(key: EntryKey) -> Self {
        Self {
            key,
            trackers: None,
            limits: TorrentLimits::default(),
            group: None,
            space: None,
            paused: false,
        }
    }
}

/// One entry's statistics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryStats {
    pub key: EntryKey,
    pub paused: bool,
    /// Payload bytes this entry's peers sent us and we sent them, in this session.
    pub downloaded: u64,
    pub uploaded: u64,
    pub download_rate: u64,
    pub upload_rate: u64,
    pub peers: u32,
    pub seeds: u32,
}

/// One connected peer, for display. The flags are the protocol's own state.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct PeerInfo {
    pub addr: SocketAddr,
    /// The client, from its peer id or its extension handshake.
    pub client: String,
    /// Fraction of the torrent it has, 0 to 1.
    pub progress: f64,
    /// Bytes per second over the last tick.
    pub download_rate: u64,
    pub upload_rate: u64,
    pub downloaded: u64,
    pub uploaded: u64,
    /// We connected to it (rather than it to us).
    pub outbound: bool,
    pub am_choking: bool,
    pub am_interested: bool,
    pub peer_choking: bool,
    pub peer_interested: bool,
    /// Suspected of sending bad data: only given whole pieces of its own.
    pub on_parole: bool,
    /// Connected over uTP rather than TCP.
    pub utp: bool,
    /// The payload is encrypted (MSE with RC4).
    pub encrypted: bool,
    /// The entry the connection belongs to.
    pub entry: EntryKey,
}

/// A piece a torrent verified and wrote, from [`crate::Session::piece_events`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PieceEvent {
    pub torrent: InfoHashes,
    pub piece: u32,
    pub origin: PieceOrigin,
}

/// A file whose every piece is now verified and written, from
/// [`crate::Session::file_events`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileEvent {
    pub torrent: InfoHashes,
    pub file: usize,
}

/// Where a verified piece's bytes came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PieceOrigin {
    /// From peers or web seeds.
    Downloaded,
    /// A bookend: `from_disk` of its bytes were read from files the content library
    /// supplied (see [`TorrentHandle::set_reused`]), the rest downloaded.
    Bookend { from_disk: u32 },
    /// Handed over by [`TorrentHandle::offer_piece`].
    Offered,
    /// Found on re-hashing a file put in place by [`TorrentHandle::adopt_file`].
    Adopted,
}

/// The answer to [`TorrentHandle::offer_piece`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Offered {
    /// It verified and was written: the torrent has the piece now.
    Taken,
    /// It doesn't match this torrent's hash.
    Mismatch,
    /// Not needed: had, not wanted, already in progress, the wrong size, or the torrent
    /// isn't running (checking, or gone).
    NotNeeded,
    /// It verified, but writing it failed.
    Failed(String),
}

/// Why [`TorrentHandle::adopt_file`] didn't replace a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdoptRefused {
    /// The torrent has (or is downloading) a piece touching the file, so the file holds
    /// verified bytes; or the file isn't wanted; or the torrent is still checking.
    InUse,
    /// The link, clone or copy failed.
    Io(String),
}

/// What [`TorrentHandle::adopt_file`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adoption {
    pub how: Adopted,
    /// Pieces touching the file that verified afterwards and weren't had before.
    pub gained: Vec<u32>,
}

/// What happens to a file that is deselected after some of it was downloaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnwantedPolicy {
    /// Move it to the torrent's held area, so selecting it again needs no download.
    #[default]
    Hold,
    /// Delete it, keeping only the boundary bytes the torrent's wanted files still need.
    Delete,
}

/// Parameters for adding a torrent. Start from [`AddTorrent::new`].
#[derive(Debug, Clone)]
pub struct AddTorrent {
    pub torrent: Arc<Torrent>,
    pub save_path: PathBuf,
    /// The torrent's held area. Defaults to `.apitorrent/held/<id>` in the save path.
    pub held_path: Option<PathBuf>,
    /// The torrent's part store. Defaults to `.apitorrent/<id>.parts` in the save path.
    pub parts_path: Option<PathBuf>,
    /// Pieces known to be on disk (resume data). `None` means check the files.
    pub have: Option<Bitfield>,
    /// Each file's placement (resume data). `None` derives them from the priorities:
    /// files with priority 0 live in the part store, the rest in the save path.
    pub placements: Option<Vec<Placement>>,
    pub unwanted: UnwantedPolicy,
    pub paused: bool,
    /// Peers to try straight away.
    pub peers: Vec<SocketAddr>,
    /// One priority per file; `None` downloads everything at the default priority.
    pub file_priorities: Option<Vec<u8>>,
    /// The first entry's key (see [`EntryParams`]). `paused`, `limits` and `group` are the
    /// first entry's too.
    pub entry: EntryKey,
    /// The first entry's announce tiers; `None` uses the torrent's own.
    pub trackers: Option<Vec<Vec<String>>>,
    /// The first entry's space (see [`crate::Session::set_space`]).
    pub space: Option<String>,
    /// More entries, after the first, oldest first.
    pub more_entries: Vec<EntryParams>,
    /// This torrent's own rate, connection and memory limits.
    pub limits: TorrentLimits,
    /// The group whose shared limits apply too (see [`crate::Session::set_group`]).
    pub group: Option<String>,
    /// Each file's path under the save path, overriding the torrent's own (portable)
    /// paths. Used for native naming. Must have one entry per file.
    pub file_paths: Option<Vec<Vec<String>>>,
    /// Files that are hard links to another torrent's copy (the content library). Writes
    /// that would change them give this torrent its own copy first.
    pub linked: Vec<usize>,
    /// Files whose bytes on disk came from the content library (see
    /// [`TorrentHandle::set_reused`]).
    pub reused: Vec<usize>,
    /// Files relocated earlier by [`TorrentHandle::relocate_file`] (resume data): one
    /// entry per file, `Some` where the file lives at that path instead of under the save
    /// path.
    pub file_locations: Option<Vec<Option<PathBuf>>>,
    /// The file order (see [`TorrentHandle::set_file_order`]).
    pub file_order: Option<Vec<usize>>,
    /// Files read while they download, each with the byte its reader needs next (see
    /// [`TorrentHandle::set_stream`]).
    pub streams: Vec<(usize, u64)>,
    /// Each file's first and last piece first (see [`TorrentHandle::set_first_last`]).
    pub first_last: bool,
    /// Super-seeding (see [`TorrentHandle::set_super_seed`]).
    pub super_seed: bool,
}

impl AddTorrent {
    pub fn new(torrent: Arc<Torrent>, save_path: impl Into<PathBuf>) -> Self {
        Self {
            torrent,
            save_path: save_path.into(),
            held_path: None,
            parts_path: None,
            have: None,
            placements: None,
            unwanted: UnwantedPolicy::default(),
            paused: false,
            peers: Vec::new(),
            file_priorities: None,
            entry: 0,
            trackers: None,
            space: None,
            more_entries: Vec::new(),
            limits: TorrentLimits::default(),
            group: None,
            file_paths: None,
            linked: Vec::new(),
            reused: Vec::new(),
            file_locations: None,
            file_order: None,
            streams: Vec::new(),
            first_last: false,
            super_seed: false,
        }
    }

    /// The first entry, from the fields that describe it.
    fn first_entry(&self) -> EntryParams {
        EntryParams {
            key: self.entry,
            trackers: self.trackers.clone(),
            limits: self.limits,
            group: self.group.clone(),
            space: self.space.clone(),
            paused: self.paused,
        }
    }
}

pub(crate) enum Command {
    AddPeers(Vec<SocketAddr>),
    /// The pieces we have right now, for resume data.
    Have(oneshot::Sender<Bitfield>),
    /// The trackers of one entry, or of every entry.
    Trackers(Option<EntryKey>, oneshot::Sender<Vec<TrackerStatus>>),
    Peers(oneshot::Sender<Vec<PeerInfo>>),
    /// One priority per file (0 = skip, up to [`crate::picker::MAX_PRIORITY`]).
    FilePriorities(Vec<u8>),
    FileOrder(Option<Vec<usize>>),
    Stream(usize, Option<u64>),
    FirstLast(bool),
    SuperSeed(bool),
    SetUnwantedPolicy(UnwantedPolicy),
    SetLimits(EntryKey, TorrentLimits),
    SetGroup(EntryKey, Option<String>),
    SetSpace(EntryKey, Option<String>),
    SetTrackers(EntryKey, Vec<Vec<String>>),
    AddEntry(EntryParams),
    RemoveEntry(EntryKey),
    PauseEntry(EntryKey),
    ResumeEntry(EntryKey),
    /// Delete the held copies of these files.
    ReleaseHeld(Vec<usize>),
    OfferPiece(u32, Vec<u8>, oneshot::Sender<Offered>),
    AdoptFile(
        usize,
        PathBuf,
        AdoptMode,
        oneshot::Sender<Result<Adoption, AdoptRefused>>,
    ),
    SetReused(Vec<usize>),
    /// Verify every piece on disk again, as on a first start.
    Recheck,
    Pause,
    Resume,
    Shutdown(oneshot::Sender<Bitfield>),
}

pub(crate) enum ActorMsg {
    Command(Command),
    Incoming {
        stream: peer::BoxIo,
        handshake: Handshake,
        link: peer::Link,
        addr: SocketAddr,
    },
    Checked(Bitfield),
    PieceWritten {
        piece: u32,
        result: Result<Verdict, String>,
        /// SHA-1 of each block, when asked for or when the piece failed.
        digests: Option<Vec<Sha1Hash>>,
    },
    /// Consecutive blocks of one piece, read in one go.
    BlocksRead {
        peer: PeerKey,
        blocks: Vec<BlockRef>,
        result: Result<Bytes, String>,
    },
    Announced {
        entry: EntryKey,
        job: Job,
        result: Result<apitorrent_tracker::AnnounceResponse, String>,
    },
    /// A batch of placement changes finished.
    Moved {
        files: Vec<usize>,
        result: Result<(), String>,
    },
    /// An offered piece was hashed (and written, if it verified).
    OfferWritten {
        piece: u32,
        result: Result<Verdict, String>,
        reply: oneshot::Sender<Offered>,
    },
    /// A file was adopted (or not), and the pieces touching it re-hashed.
    FileAdopted {
        file: usize,
        result: Result<(Adopted, Vec<u32>), String>,
        reply: oneshot::Sender<Result<Adoption, AdoptRefused>>,
    },
    /// A bookend piece's blocks were read from the library's files.
    Prefilled {
        piece: u32,
        result: Result<Vec<(u32, Vec<u8>)>, String>,
    },
}

/// Handle to a running torrent.
#[derive(Debug, Clone)]
pub struct TorrentHandle {
    pub(crate) info_hashes: InfoHashes,
    pub(crate) tx: mpsc::Sender<ActorMsg>,
    pub(crate) stats: watch::Receiver<TorrentStats>,
    /// The pieces the starting check (or the validated resume data) found.
    pub(crate) checked: watch::Receiver<Option<Bitfield>>,
    pub(crate) storage: Arc<dyn Storage>,
}

impl TorrentHandle {
    pub fn info_hashes(&self) -> InfoHashes {
        self.info_hashes
    }

    pub fn stats(&self) -> TorrentStats {
        self.stats.borrow().clone()
    }

    /// The pieces the torrent's starting check found (or its validated resume data), once
    /// the check is done: what was on disk before anything was downloaded.
    pub fn checked(&self) -> Option<Bitfield> {
        self.checked.borrow().clone()
    }

    /// A receiver that changes whenever the stats are refreshed.
    pub fn watch(&self) -> watch::Receiver<TorrentStats> {
        self.stats.clone()
    }

    pub async fn add_peers(&self, peers: Vec<SocketAddr>) {
        let _ = self
            .tx
            .send(ActorMsg::Command(Command::AddPeers(peers)))
            .await;
    }

    pub async fn pause(&self) {
        let _ = self.tx.send(ActorMsg::Command(Command::Pause)).await;
    }

    /// Verify every piece on disk again. The torrent shows as checking meanwhile, and
    /// carries on (or stays paused) with what is found.
    pub async fn recheck(&self) {
        let _ = self.tx.send(ActorMsg::Command(Command::Recheck)).await;
    }

    pub async fn resume(&self) {
        let _ = self.tx.send(ActorMsg::Command(Command::Resume)).await;
    }

    /// The pieces downloaded and verified so far.
    pub async fn have(&self) -> Option<Bitfield> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorMsg::Command(Command::Have(tx)))
            .await
            .ok()?;
        rx.await.ok()
    }

    /// The status of each tracker, of every entry.
    pub async fn trackers(&self) -> Vec<TrackerStatus> {
        self.trackers_of(None).await
    }

    /// The status of each tracker one entry announces to (or every entry's, with `None`).
    /// A URL an older running entry also lists shows that entry's status.
    pub async fn trackers_of(&self, entry: Option<EntryKey>) -> Vec<TrackerStatus> {
        let (tx, rx) = oneshot::channel();
        if self
            .tx
            .send(ActorMsg::Command(Command::Trackers(entry, tx)))
            .await
            .is_err()
        {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    /// Add an entry (see [`EntryParams`]). An entry with a key already present is ignored.
    pub async fn add_entry(&self, entry: EntryParams) {
        self.command(Command::AddEntry(entry)).await;
    }

    /// Remove an entry: its peers are disconnected and its trackers told `stopped`. The
    /// torrent keeps running for its other entries; without any, it stays idle.
    pub async fn remove_entry(&self, key: EntryKey) {
        self.command(Command::RemoveEntry(key)).await;
    }

    /// Pause one entry. The torrent runs while any entry does.
    pub async fn pause_entry(&self, key: EntryKey) {
        self.command(Command::PauseEntry(key)).await;
    }

    pub async fn resume_entry(&self, key: EntryKey) {
        self.command(Command::ResumeEntry(key)).await;
    }

    async fn command(&self, cmd: Command) {
        let _ = self.tx.send(ActorMsg::Command(cmd)).await;
    }

    /// Set a download priority per file: 0 skips the file, higher values are fetched first.
    /// A piece shared by several files takes the highest priority among them.
    pub async fn set_file_priorities(&self, priorities: Vec<u8>) {
        let _ = self
            .tx
            .send(ActorMsg::Command(Command::FilePriorities(priorities)))
            .await;
    }

    /// Prefer downloading files in this order (first first): among pieces of the same
    /// priority, the first unfinished file's go first. A preference, not a rule: peers
    /// without those pieces still supply later files. `None` turns it off.
    pub async fn set_file_order(&self, order: Option<Vec<usize>>) {
        self.command(Command::FileOrder(order)).await;
    }

    /// Read file `file` while it downloads, needing byte `from` next: its pieces come in
    /// read order (its start and end first, for a player's header and index), and the
    /// piece the reader waits for is finished first. Call again with a new `from` when
    /// the reader seeks. `None` stops.
    pub async fn set_stream(&self, file: usize, from: Option<u64>) {
        self.command(Command::Stream(file, from)).await;
    }

    /// Fetch each wanted file's first and last piece before the rest of its priority, so
    /// files can be previewed before they complete.
    pub async fn set_first_last(&self, on: bool) {
        self.command(Command::FirstLast(on)).await;
    }

    /// Super-seeding (BEP 16), for the first seed of a new torrent: once it has every
    /// piece, peers are shown one or two pieces at a time instead of everything, each one
    /// they lack and the rarest, and get another once they have one they were shown. Fewer
    /// bytes leave this seed before the swarm holds a full copy.
    /// It applies to peers that connect afterwards; turning it off shows every peer
    /// everything.
    pub async fn set_super_seed(&self, on: bool) {
        self.command(Command::SuperSeed(on)).await;
    }

    /// Move file `file` out of the save directory to `to` and keep seeding it from there
    /// (see `Storage::relocate`). The file must be in the save directory, and nothing
    /// may exist at `to`.
    pub async fn relocate_file(&self, file: usize, to: PathBuf) -> std::io::Result<()> {
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || storage.relocate(file, &to))
            .await
            .map_err(std::io::Error::other)?
    }

    /// Where each file is now: `None` for files kept only in the part store.
    pub fn file_paths(&self) -> Vec<Option<PathBuf>> {
        (0..self.storage.placements().len())
            .map(|f| self.storage.file_path(f))
            .collect()
    }

    /// Where each file's bytes are (for resume data). Waits for a move in progress.
    pub fn placements(&self) -> Vec<Placement> {
        self.storage.placements()
    }

    /// The files still linked to another torrent's copy (for resume data).
    pub fn linked(&self) -> Vec<usize> {
        self.storage
            .linked()
            .iter()
            .enumerate()
            .filter_map(|(i, &l)| l.then_some(i))
            .collect()
    }

    /// Mark files as shared with another torrent's copy (see [`Self::linked`]): writes that
    /// would change them give this torrent a private copy first.
    pub fn mark_linked(&self, files: &[usize]) {
        self.storage.mark_linked(files);
    }

    /// Replace file `file` on disk with no other I/O on it running (see
    /// `Storage::replace_file`). On success it is marked linked. Blocking: call it off the
    /// async runtime.
    pub fn replace_file(
        &self,
        file: usize,
        swap: &mut dyn FnMut(&std::path::Path) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        self.storage.replace_file(file, swap)
    }

    /// Change one entry's own limits.
    pub async fn set_limits(&self, entry: EntryKey, limits: TorrentLimits) {
        self.command(Command::SetLimits(entry, limits)).await;
    }

    /// Put an entry in a group (or none, with `None`). An unknown group name means no group.
    pub async fn set_group(&self, entry: EntryKey, group: Option<String>) {
        self.command(Command::SetGroup(entry, group)).await;
    }

    /// Put an entry in a space's limits (or none). An unknown name means none.
    pub async fn set_space(&self, entry: EntryKey, space: Option<String>) {
        self.command(Command::SetSpace(entry, space)).await;
    }

    /// Replace one entry's announce tiers. Trackers it keeps carry on where they were (no
    /// `stopped` and `started` again); trackers it drops are told `stopped`. A URL an older
    /// running entry lists is still announced by that entry only.
    pub async fn set_trackers(&self, entry: EntryKey, tiers: Vec<Vec<String>>) {
        self.command(Command::SetTrackers(entry, tiers)).await;
    }

    /// Choose what happens to files deselected from now on.
    pub async fn set_unwanted_policy(&self, policy: UnwantedPolicy) {
        let _ = self
            .tx
            .send(ActorMsg::Command(Command::SetUnwantedPolicy(policy)))
            .await;
    }

    /// Delete the held copies of these files (all held files when `None`). Their boundary
    /// bytes stay in the part store while wanted files need them.
    pub async fn release_held(&self, files: Option<Vec<usize>>) {
        let files = files.unwrap_or_else(|| {
            let p = self.storage.placements();
            (0..p.len()).filter(|&i| p[i] == Placement::Held).collect()
        });
        let _ = self
            .tx
            .send(ActorMsg::Command(Command::ReleaseHeld(files)))
            .await;
    }

    /// Offer a piece's bytes from elsewhere (another torrent with the same file). They are
    /// verified against this torrent's hash and written through its storage like a
    /// downloaded piece, so nothing unverified is ever written.
    pub async fn offer_piece(&self, piece: u32, data: Vec<u8>) -> Offered {
        let (tx, rx) = oneshot::channel();
        if self
            .tx
            .send(ActorMsg::Command(Command::OfferPiece(piece, data, tx)))
            .await
            .is_err()
        {
            return Offered::NotNeeded;
        }
        rx.await.unwrap_or(Offered::NotNeeded)
    }

    /// Replace file `file` with another file's bytes (see [`Storage::adopt`]), then
    /// re-hash the pieces touching it. Refused while any of those pieces is had or in
    /// progress: the file may hold verified bytes, or a write may be on its way.
    pub async fn adopt_file(
        &self,
        file: usize,
        from: PathBuf,
        mode: AdoptMode,
    ) -> Result<Adoption, AdoptRefused> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorMsg::Command(Command::AdoptFile(file, from, mode, tx)))
            .await
            .map_err(|_| AdoptRefused::InUse)?;
        rx.await.unwrap_or(Err(AdoptRefused::InUse))
    }

    /// Mark files whose bytes came from the content library. A piece straddling one of
    /// them and a file still to download (a bookend) takes the blocks lying wholly in the
    /// library's file from disk, and only the rest is downloaded. If the piece then fails,
    /// it is downloaded again entirely from peers.
    pub async fn set_reused(&self, files: Vec<usize>) {
        self.command(Command::SetReused(files)).await;
    }

    /// Delete the torrent's held copies and part store, and its downloaded files too when
    /// `save_files` is set. Call it after [`TorrentHandle::shutdown`].
    pub async fn delete_data(&self, save_files: bool) -> std::io::Result<()> {
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || storage.delete(save_files))
            .await
            .map_err(std::io::Error::other)?
    }

    /// The connected peers.
    pub async fn peers(&self) -> Vec<PeerInfo> {
        let (tx, rx) = oneshot::channel();
        if self
            .tx
            .send(ActorMsg::Command(Command::Peers(tx)))
            .await
            .is_err()
        {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    /// Stop the torrent, returning the pieces it had (for resume data).
    pub async fn shutdown(&self) -> Option<Bitfield> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorMsg::Command(Command::Shutdown(tx)))
            .await
            .ok()?;
        rx.await.ok()
    }

    /// Wait until every wanted piece is downloaded and verified.
    pub async fn wait_finished(&self) -> TorrentStats {
        let mut rx = self.stats.clone();
        let stats = rx
            .wait_for(|s| s.state == TorrentState::Seeding)
            .await
            .map(|s| s.clone());
        stats.unwrap_or_else(|_| self.stats())
    }
}

/// Our view of one connection. The four choke/interest flags are the protocol's own state.
#[allow(clippy::struct_excessive_bools)]
struct Peer {
    addr: SocketAddr,
    outbound: bool,
    /// The entry this connection belongs to.
    entry: EntryKey,
    tx: Option<mpsc::UnboundedSender<PeerCmd>>,
    peer_id: Option<PeerId>,
    fast: bool,
    /// The peer set the v2 bit in its handshake, so it can answer hash requests.
    v2: bool,
    extensions: Option<ExtensionHandshake>,
    /// Addresses we last told this peer about by PEX.
    pex_sent: HashSet<SocketAddr>,
    /// The transport and encryption, once connected.
    link: peer::Link,
    has: Bitfield,
    is_seed: bool,
    /// The peer said it downloads nothing more (BEP 21 `upload_only`).
    upload_only: bool,
    /// How many pieces the peer has that we still want.
    wanted: u32,
    am_choking: bool,
    am_interested: bool,
    peer_choking: bool,
    peer_interested: bool,
    /// Our outstanding requests to the peer. A set: a fast peer has hundreds, and each
    /// block that arrives is looked up.
    requests: HashSet<BlockRef>,
    /// The `reqq` we told the peer: the most requests it may have with us at once. Queued
    /// requests cost a few bytes each: they are read from disk only as the peer's send
    /// budget allows (see [`send_budget`]).
    our_reqq: u32,
    /// The peer's requests we took and haven't started reading, oldest first.
    upload_queue: VecDeque<BlockRef>,
    /// Blocks being read for the peer, and their bytes.
    reading: usize,
    reading_bytes: u64,
    /// Piece bytes handed to the connection and not yet written to the socket.
    unsent: u64,
    /// Requests the peer cancelled while their read was in flight: not sent when it ends.
    cancelled: HashSet<BlockRef>,
    downloaded: u64,
    uploaded: u64,
    rate_down: u64,
    rate_up: u64,
    last_down: u64,
    last_up: u64,
    optimistic: bool,
    /// Super-seeding: the pieces this peer was shown and hasn't had yet (see
    /// [`SUPERSEED_OFFERS`]).
    offered: Vec<u32>,
    /// Super-seeding: every piece this peer was ever shown, so none is shown twice.
    shown: HashSet<u32>,
    /// Units of the session's and group's connection counters, held while connected.
    _slots: Vec<Slot>,
    /// A half-open unit, held until the handshake completes.
    half_open: Option<Slot>,
}

impl Peer {
    fn new(
        addr: SocketAddr,
        entry: EntryKey,
        pieces: u32,
        outbound: bool,
        slots: Vec<Slot>,
    ) -> Self {
        Self {
            addr,
            outbound,
            entry,
            tx: None,
            peer_id: None,
            fast: false,
            v2: false,
            extensions: None,
            pex_sent: HashSet::new(),
            link: peer::Link::default(),
            has: Bitfield::new(pieces),
            is_seed: false,
            upload_only: false,
            wanted: 0,
            am_choking: true,
            am_interested: false,
            peer_choking: true,
            peer_interested: false,
            requests: HashSet::new(),
            our_reqq: WireStyle::APITORRENT.request_queue,
            upload_queue: VecDeque::new(),
            reading: 0,
            reading_bytes: 0,
            unsent: 0,
            cancelled: HashSet::new(),
            downloaded: 0,
            uploaded: 0,
            rate_down: 0,
            rate_up: 0,
            last_down: 0,
            last_up: 0,
            optimistic: false,
            offered: Vec::new(),
            shown: HashSet::new(),
            _slots: slots,
            half_open: None,
        }
    }

    fn send(&self, m: Message) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(PeerCmd::Send(m));
        }
    }

    fn close(&self) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(PeerCmd::Close);
        }
    }
}

/// An entry's running state (see [`EntryParams`]).
struct Entry {
    key: EntryKey,
    paused: bool,
    /// Every announce URL the entry lists, to tell which an older entry covers.
    urls: HashSet<String>,
    announcer: Announcer,
    throttle: Arc<Throttle>,
    limits: TorrentLimits,
    downloaded: u64,
    uploaded: u64,
    last_downloaded: u64,
    last_uploaded: u64,
}

impl Entry {
    fn new(params: EntryParams, torrent: &Torrent, keys: &[Sha1Hash], shared: &Shared) -> Self {
        let trackers = params.trackers.unwrap_or_else(|| torrent.trackers.clone());
        Self {
            key: params.key,
            paused: params.paused,
            urls: trackers.iter().flatten().cloned().collect(),
            announcer: Announcer::new(&trackers, keys, Instant::now()),
            throttle: Arc::new(Throttle::new(
                shared.download.clone(),
                shared.upload.clone(),
                params.space.as_deref().and_then(|s| shared.space(s)),
                params.group.as_deref().and_then(|g| shared.group(g)),
                params.limits,
            )),
            limits: params.limits,
            downloaded: 0,
            uploaded: 0,
            last_downloaded: 0,
            last_uploaded: 0,
        }
    }

    fn totals(&self) -> Totals {
        Totals {
            uploaded: self.uploaded,
            downloaded: self.downloaded,
        }
    }
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "independent per-torrent switches"
)]
pub(crate) struct TorrentActor {
    info_hashes: InfoHashes,
    short_hash: Sha1Hash,
    torrent: Arc<Torrent>,
    layout: Layout,
    hashes: Arc<PieceHashes>,
    storage: Arc<dyn Storage>,
    picker: Option<Picker>,
    shared: Arc<Shared>,
    rx: mpsc::Receiver<ActorMsg>,
    tx: mpsc::Sender<ActorMsg>,
    peer_tx: peer::EventTx,
    peer_rx: mpsc::Receiver<(PeerKey, PeerEvent)>,
    peers: HashMap<PeerKey, Peer>,
    buffers: HashMap<u32, Vec<u8>>,
    /// Addresses to dial, with the entry whose tracker named them (`None`: found by DHT,
    /// PEX or LSD, so the oldest running entry's).
    candidates: VecDeque<(SocketAddr, Option<EntryKey>)>,
    /// What PEX said about candidates' uTP support, until they are dialed.
    utp_hints: HashMap<SocketAddr, peer::UtpHint>,
    /// Holepunch-capable candidates (PEX flag `0x08`) and the peer that told us of each,
    /// which can introduce us if dialing fails (BEP 55).
    introducers: HashMap<SocketAddr, PeerKey>,
    /// Addresses we asked an introducer for, so each is asked once.
    punched: HashSet<SocketAddr>,
    /// Candidates we shouldn't dial before the given time.
    retry_at: HashMap<SocketAddr, Instant>,
    /// Addresses of seeds, and of peers that wanted nothing more when we had finished. They
    /// aren't dialled while we have everything we want.
    seed_addrs: HashSet<SocketAddr>,
    known: HashSet<SocketAddr>,
    stats: watch::Sender<TorrentStats>,
    checked: watch::Sender<Option<Bitfield>>,
    /// Oldest first. The torrent runs while any of them does.
    entries: Vec<Entry>,
    /// The wire info-hashes the torrent is announced under.
    keys: Vec<Sha1Hash>,
    error: Option<String>,
    downloaded: u64,
    uploaded: u64,
    last_downloaded: u64,
    last_uploaded: u64,
    ticks: u32,
    max_message: u32,
    file_priorities: Option<Vec<u8>>,
    file_order: Option<Vec<usize>>,
    streams: Vec<(usize, u64)>,
    first_last: bool,
    super_seed: bool,
    unwanted: UnwantedPolicy,
    /// Files whose placement change is in flight.
    moving: HashSet<usize>,
    /// Bytes this torrent has counted against the session's memory budget.
    memory_held: u64,
    /// Who sent each block of each piece in progress.
    block_from: HashMap<u32, Vec<Option<IpAddr>>>,
    reputation: Reputation,
    /// Piece layers still to fetch, by `pieces root`.
    layer_fetches: HashMap<Sha256Hash, LayerFetch>,
    webseeds: HashMap<PeerKey, WebSeed>,
    /// Which web seed supplied each piece being verified.
    webseed_pieces: HashMap<u32, PeerKey>,
    webseed_tx: mpsc::Sender<WebSeedEvent>,
    webseed_rx: mpsc::Receiver<WebSeedEvent>,
    /// Whether the download was already finished; finishing sends `completed` once.
    finished: bool,
    /// What peers were last told of our `upload_only` (BEP 21: nothing left we want).
    told_upload_only: bool,
    /// Pieces handed over by `offer_piece`, being hashed and written.
    offered: HashSet<u32>,
    /// Pieces held back while a file they touch is being adopted.
    adopting: HashSet<u32>,
    /// Files whose bytes came from the content library.
    reused: Vec<bool>,
    /// Bookend pieces (see [`TorrentHandle::set_reused`]) and their blocks lying wholly in
    /// reused files.
    bookends: Vec<(u32, Vec<u32>)>,
    /// The pieces of `bookends`.
    bookend_pieces: HashSet<u32>,
    /// Bookend pieces whose blocks are being read from disk.
    prefilling: HashSet<u32>,
    /// Bookend pieces started with blocks from disk, and how many bytes those were.
    prefilled: HashMap<u32, u32>,
    /// Bookend pieces that failed once: downloaded whole from now on.
    no_prefill: HashSet<u32>,
    /// A recheck asked for while files were moving or pieces were being taken in; it
    /// starts once they are done.
    recheck_pending: bool,
    /// The client this torrent presents as (see [`crate::identity`]): its peer id in
    /// announces and handshakes, its user agent and `v`.
    identity: ActiveIdentity,
    /// The generation of the session's identity rules `identity` was chosen under.
    identity_gen: u64,
}

impl TorrentActor {
    pub(crate) fn spawn(params: AddTorrent, shared: Arc<Shared>) -> (TorrentHandle, Vec<Sha1Hash>) {
        let torrent = params.torrent.clone();
        let info = &torrent.info;
        let info_hashes = info.info_hashes();
        let layout = Layout::from_info(info);
        let relative: Vec<Vec<String>> = params
            .file_paths
            .clone()
            .filter(|p| p.len() == info.files.len())
            .unwrap_or_else(|| info.files.iter().map(|f| f.path.clone()).collect());
        let id = info_hashes.id_string();
        let internal = params.save_path.join(".apitorrent");
        let paths = StoragePaths {
            save: params.save_path.clone(),
            held: params
                .held_path
                .clone()
                .unwrap_or_else(|| internal.join("held").join(&id)),
            parts: params
                .parts_path
                .clone()
                .unwrap_or_else(|| internal.join(format!("{id}.parts"))),
        };
        let placements = params.placements.clone().unwrap_or_else(|| {
            (0..info.files.len())
                .map(|i| match &params.file_priorities {
                    Some(p) if p.get(i) == Some(&0) => Placement::Parts,
                    _ => Placement::Save,
                })
                .collect()
        });
        let file_storage = FileStorage::new(&paths, layout.clone(), &relative, Some(&placements));
        file_storage.set_linked(&params.linked);
        if let Some(locations) = &params.file_locations {
            file_storage.set_locations(locations);
        }
        let storage: Arc<dyn Storage> = Arc::new(file_storage);
        let hashes = Arc::new(PieceHashes::new(&torrent));
        let (tx, rx) = mpsc::channel(1024);
        let (peer_tx, peer_rx) = mpsc::channel(4096);
        let initial = TorrentStats {
            state: if params.have.is_some() {
                TorrentState::Downloading
            } else {
                TorrentState::Checking
            },
            pieces_have: 0,
            pieces_total: layout.piece_count(),
            wanted_total: layout.piece_count(),
            wanted_have: 0,
            finished: false,
            downloaded: 0,
            uploaded: 0,
            download_rate: 0,
            upload_rate: 0,
            peers: 0,
            seeds: 0,
            error: None,
            entries: Vec::new(),
        };
        let (stats_tx, stats_rx) = watch::channel(initial);
        let (checked_tx, checked_rx) = watch::channel(None);
        let mut keys = Vec::new();
        if let Some(v1) = info_hashes.get_v1() {
            keys.push(v1);
        }
        if let Some(v2) = info_hashes.get_v2() {
            keys.push(v2.truncated());
        }
        let max_message = Decoder::max_len_for(u64::from(layout.piece_count()));
        let (webseed_tx, webseed_rx) = mpsc::channel(64);
        let mut entries: Vec<Entry> = Vec::new();
        for e in std::iter::once(params.first_entry()).chain(params.more_entries.clone()) {
            if entries.iter().all(|x| x.key != e.key) {
                entries.push(Entry::new(e, &torrent, &keys, &shared));
            }
        }
        let (identity, identity_gen) = ActiveIdentity::resolve(
            &shared,
            entries
                .iter()
                .flat_map(|e| e.urls.iter().map(String::as_str)),
        );
        let mut actor = TorrentActor {
            info_hashes,
            short_hash: info_hashes.primary_short(),
            torrent: torrent.clone(),
            layout,
            hashes,
            storage,
            picker: None,
            shared,
            rx,
            tx: tx.clone(),
            peer_tx,
            peer_rx,
            peers: HashMap::new(),
            buffers: HashMap::new(),
            candidates: VecDeque::new(),
            utp_hints: HashMap::new(),
            introducers: HashMap::new(),
            punched: HashSet::new(),
            retry_at: HashMap::new(),
            seed_addrs: HashSet::new(),
            known: HashSet::new(),
            stats: stats_tx,
            checked: checked_tx,
            entries,
            keys: keys.clone(),
            error: None,
            downloaded: 0,
            uploaded: 0,
            last_downloaded: 0,
            last_uploaded: 0,
            ticks: 0,
            max_message,
            file_priorities: params.file_priorities,
            file_order: params.file_order,
            streams: params.streams,
            first_last: params.first_last,
            super_seed: params.super_seed,
            unwanted: params.unwanted,
            moving: HashSet::new(),
            memory_held: 0,
            block_from: HashMap::new(),
            reputation: Reputation::default(),
            layer_fetches: HashMap::new(),
            webseeds: HashMap::new(),
            webseed_pieces: HashMap::new(),
            webseed_tx,
            webseed_rx,
            finished: false,
            told_upload_only: false,
            offered: HashSet::new(),
            adopting: HashSet::new(),
            reused: {
                let mut r = vec![false; info.files.len()];
                for &f in &params.reused {
                    if let Some(x) = r.get_mut(f) {
                        *x = true;
                    }
                }
                r
            },
            bookends: Vec::new(),
            bookend_pieces: HashSet::new(),
            prefilling: HashSet::new(),
            prefilled: HashMap::new(),
            no_prefill: HashSet::new(),
            recheck_pending: false,
            identity,
            identity_gen,
        };
        actor.set_bookends();
        actor.add_candidates(params.peers, None);
        match params.have {
            Some(have) => {
                let have = validate_resume(actor.storage.as_ref(), &actor.layout, have);
                actor.start(have);
                actor.publish_stats();
            }
            None => actor.check_files(),
        }
        let storage = actor.storage.clone();
        tokio::spawn(actor.run());
        (
            TorrentHandle {
                info_hashes,
                tx,
                stats: stats_rx,
                checked: checked_rx,
                storage,
            },
            keys,
        )
    }

    fn check_files(&self) {
        let storage = self.storage.clone();
        let hashes = self.hashes.clone();
        let layout = self.layout.clone();
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let have = check_existing(storage.as_ref(), &hashes, &layout);
            let _ = tx.blocking_send(ActorMsg::Checked(have));
        });
    }

    /// Forget which pieces we have and verify them all again from disk. Connections close
    /// (peers were told about pieces we may turn out not to have) and reopen once the check
    /// is done; a paused torrent stays paused. Clears a disk error, like resuming.
    fn recheck(&mut self) {
        if self.picker.is_none() && self.error.is_none() {
            return; // Checking already.
        }
        if !self.moving.is_empty() || !self.offered.is_empty() || !self.adopting.is_empty() {
            self.recheck_pending = true;
            return;
        }
        self.recheck_pending = false;
        self.error = None;
        self.shutdown_peers();
        if let Err(e) = self.storage.sync() {
            tracing::warn!(error = %e, "could not flush before rechecking");
        }
        self.picker = None;
        self.buffers.clear();
        self.block_from.clear();
        self.webseed_pieces.clear();
        for ws in self.webseeds.values_mut() {
            ws.piece = None;
        }
        self.prefilling.clear();
        self.prefilled.clear();
        self.sync_memory();
        self.publish_stats();
        self.check_files();
    }

    /// Start a recheck that waited for moves or offered pieces, once none are left.
    fn recheck_if_pending(&mut self) {
        if self.recheck_pending {
            self.recheck();
        }
    }

    fn start(&mut self, have: Bitfield) {
        self.checked.send_replace(Some(have.clone()));
        let seed = rand::random();
        let mut picker = Picker::new(self.layout.clone(), have, seed);
        self.plan_layer_fetches();
        if let Some(prios) = self.effective_priorities() {
            picker.set_priorities(&prios);
        }
        picker.set_file_order(self.file_order.clone());
        picker.set_first_last(self.first_last);
        for &(file, from) in &self.streams {
            picker.set_stream(file, Some(from));
        }
        self.picker = Some(picker);
        self.apply_holds();
        self.finished = self.picker.as_ref().is_some_and(Picker::is_finished);
        self.start_webseeds();
        if let Err(e) = self.storage.create_empty_files() {
            tracing::warn!(error = %e, "could not create empty files");
        }
    }

    fn set_file_priorities(&mut self, prios: &[u8]) {
        if prios.len() != self.layout.files().len() {
            tracing::warn!(
                given = prios.len(),
                files = self.layout.files().len(),
                "ignoring file priorities of the wrong length"
            );
            return;
        }
        self.file_priorities = Some(prios.to_vec());
        self.place_files();
        self.refresh_priorities();
        // Newly wanted pieces may need the seeds we let go once we had everything.
        if self.picker.as_ref().is_some_and(|p| !p.is_finished()) {
            for addr in &self.seed_addrs {
                self.retry_at.remove(addr);
            }
        }
        self.connect_more();
    }

    /// Piece priorities from the file priorities. `None` when everything is at the default.
    fn effective_priorities(&self) -> Option<Vec<u8>> {
        self.file_priorities
            .as_ref()
            .map(|p| piece_priorities(&self.layout, p))
    }

    /// Hold back pieces that can't be verified yet (their piece layer hasn't arrived), and
    /// release the ones that now can.
    fn apply_holds(&mut self) {
        let held: Vec<bool> = (0..self.layout.piece_count())
            .map(|p| self.held(p))
            .collect();
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        for (piece, held) in (0..).zip(held) {
            picker.set_held(piece, held);
        }
    }

    /// Whether the picker must leave `piece` alone: its hash isn't here yet, it is being
    /// offered or adopted, or it is a bookend waiting for its library blocks.
    fn held(&self, piece: u32) -> bool {
        !self.hashes.can_verify(piece)
            || self.offered.contains(&piece)
            || self.adopting.contains(&piece)
            || (self.bookend_pieces.contains(&piece) && !self.no_prefill.contains(&piece))
    }

    fn refresh_hold(&mut self, piece: u32) {
        let held = self.held(piece);
        if let Some(picker) = self.picker.as_mut() {
            picker.set_held(piece, held);
        }
    }

    /// Push the effective priorities into the picker and update every peer's interest.
    fn refresh_priorities(&mut self) {
        let prios = self.effective_priorities();
        if let Some(prios) = prios
            && let Some(picker) = self.picker.as_mut()
        {
            picker.set_priorities(&prios);
        }
        self.apply_holds();
        if let Some(picker) = self.picker.as_mut() {
            for peer in self.peers.values_mut() {
                peer.wanted = u32::try_from(
                    peer.has
                        .iter_ones()
                        .filter(|&p| picker.is_wanted(p))
                        .count(),
                )
                .expect("fits");
            }
            let keys: Vec<PeerKey> = self.peers.keys().copied().collect();
            for key in keys {
                self.update_interest(key);
            }
            self.publish_stats();
        }
    }

    // ----- web seeds (BEP 19) ------------------------------------------------------------

    fn start_webseeds(&mut self) {
        if self.torrent.web_seeds.is_empty() || !self.webseeds.is_empty() {
            return;
        }
        let info = &self.torrent.info;
        let shape = Arc::new(WebSeedTorrent {
            layout: self.layout.clone(),
            // URLs name the server's files as the torrent does, not our sanitised names.
            paths: info
                .paths(apitorrent_metainfo::sanitize::Naming::Verbatim)
                .unwrap_or_else(|_| info.files.iter().map(|f| f.path.clone()).collect()),
            name: info.name.clone(),
            single_file: info.is_single_file(),
        });
        let Some(throttle) = self.default_entry().map(|e| e.throttle.clone()) else {
            return;
        };
        for url in self.torrent.web_seeds.clone() {
            let key = self.shared.next_key();
            self.webseeds.insert(
                key,
                WebSeed {
                    url: url.clone(),
                    piece: None,
                    failures: 0,
                    bad_pieces: 0,
                    disabled: false,
                },
            );
            webseed::spawn(
                url,
                key,
                shape.clone(),
                self.shared.http(),
                throttle.clone(),
                self.webseed_tx.clone(),
            );
        }
    }

    fn on_webseed(&mut self, event: WebSeedEvent) {
        match event {
            WebSeedEvent::Want { key, reply } => {
                let _ = reply.send(self.webseed_pick(key));
            }
            WebSeedEvent::Piece { key, piece, data } => {
                if let Some(ws) = self.webseeds.get_mut(&key) {
                    ws.piece = None;
                    ws.failures = 0;
                }
                self.downloaded += data.len() as u64;
                if let Some(e) = self.default_entry_mut() {
                    e.downloaded += data.len() as u64;
                }
                let blocks = self.layout.blocks_in_piece(piece);
                let size = self.layout.piece_size(piece) as usize;
                let Some(picker) = self.picker.as_mut() else {
                    return;
                };
                let mut complete = false;
                for b in 0..blocks {
                    let block = BlockRef {
                        piece,
                        offset: b * BLOCK_LEN,
                        length: self.layout.block_size(piece, b),
                    };
                    if let Received::New { piece_complete, .. } = picker.block_received(key, &block)
                    {
                        let start = block.offset as usize;
                        let end = start + block.length as usize;
                        let buf = self.buffers.entry(piece).or_insert_with(|| vec![0u8; size]);
                        buf[start..end].copy_from_slice(&data[start..end]);
                        complete |= piece_complete;
                    }
                }
                if complete {
                    let full = self.buffers.remove(&piece).expect("buffer exists");
                    self.webseed_pieces.insert(piece, key);
                    self.verify_and_write(piece, full, false);
                }
            }
            WebSeedEvent::Failed { key, piece, error } => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.peer_gone(key);
                }
                if let Some(ws) = self.webseeds.get_mut(&key) {
                    ws.piece = None;
                    ws.failures += 1;
                    if ws.failures >= WEBSEED_MAX_FAILURES && !ws.disabled {
                        ws.disabled = true;
                        tracing::warn!(url = %ws.url, piece, %error, "giving up on a web seed");
                    }
                }
                self.sync_memory();
            }
        }
    }

    /// The next piece for a web seed to fetch: a whole piece of its own.
    fn webseed_pick(&mut self, key: PeerKey) -> Option<u32> {
        if !self.running() {
            return None;
        }
        let ws = self.webseeds.get(&key)?;
        if ws.disabled || ws.piece.is_some() {
            return None;
        }
        let memory_limit = self.memory_limit();
        let picker = self.picker.as_mut()?;
        let max_new = new_piece_room(
            &self.layout,
            picker.pieces_in_progress(),
            self.shared.memory.available(),
            memory_limit,
        )
        .min(1);
        let everything = Bitfield::full(self.layout.piece_count());
        let blocks = picker.pick_exclusive(
            key,
            &everything,
            self.layout.blocks_in_piece(0) as usize,
            max_new,
        );
        let piece = blocks.first()?.piece;
        if let Some(ws) = self.webseeds.get_mut(&key) {
            ws.piece = Some(piece);
        }
        self.sync_memory();
        Some(piece)
    }

    /// A piece from a web seed failed verification: count it against the seed.
    fn webseed_piece_failed(&mut self, piece: u32) {
        let Some(key) = self.webseed_pieces.remove(&piece) else {
            return;
        };
        if let Some(ws) = self.webseeds.get_mut(&key) {
            ws.bad_pieces += 1;
            if ws.bad_pieces >= WEBSEED_MAX_BAD_PIECES && !ws.disabled {
                ws.disabled = true;
                tracing::warn!(url = %ws.url, "web seed sent pieces that don't verify; dropped");
            }
        }
    }

    // ----- piece layers (BEP 52) ---------------------------------------------------------

    /// Find the files whose pieces can't be verified until their piece layer arrives.
    fn plan_layer_fetches(&mut self) {
        let info = &self.torrent.info;
        for (file, f) in info.files.iter().enumerate() {
            let Some(root) = f.pieces_root else { continue };
            if f.is_pad() || self.hashes.layer(&root).is_some() {
                continue;
            }
            let Some(shape) = LayerShape::new(root, f.length, info.piece_length) else {
                continue;
            };
            let first = u32::try_from(f.offset / info.piece_length).expect("fits");
            if self.hashes.can_verify(first) {
                continue; // hybrid: the v1 hashes are enough
            }
            self.layer_fetches.insert(
                root,
                LayerFetch {
                    file,
                    shape,
                    chunks: vec![None; shape.chunks() as usize],
                    asked: HashMap::new(),
                    refused: HashSet::new(),
                },
            );
        }
    }

    /// Ask v2-capable peers for the layer chunks nobody is fetching.
    fn request_layers(&mut self) {
        let now = Instant::now();
        let ready: Vec<PeerKey> = self
            .peers
            .iter()
            .filter(|(_, p)| p.tx.is_some() && p.v2)
            .map(|(k, _)| *k)
            .collect();
        if ready.is_empty() {
            return;
        }
        let mut next = 0usize;
        for fetch in self.layer_fetches.values_mut() {
            fetch
                .asked
                .retain(|_, (_, at)| now.duration_since(*at) < HASH_REQUEST_TIMEOUT);
            for chunk in 0..fetch.shape.chunks() {
                if fetch.chunks[chunk as usize].is_some() || fetch.asked.contains_key(&chunk) {
                    continue;
                }
                // Round-robin over the peers that haven't refused this layer.
                let candidates: Vec<PeerKey> = ready
                    .iter()
                    .copied()
                    .filter(|k| !fetch.refused.contains(k))
                    .collect();
                let Some(&key) = candidates.get(next % candidates.len().max(1)) else {
                    break;
                };
                next += 1;
                if let Some(peer) = self.peers.get(&key) {
                    peer.send(Message::HashRequest(fetch.shape.request(chunk)));
                    fetch.asked.insert(chunk, (key, now));
                }
            }
        }
    }

    fn on_hashes(&mut self, key: PeerKey, request: &HashRequest, hashes: &[u8]) {
        let Some(fetch) = self.layer_fetches.get_mut(&request.pieces_root) else {
            return;
        };
        let chunk = request.index / fetch.shape.chunk_len().max(1);
        if fetch.asked.get(&chunk).is_none_or(|(k, _)| *k != key) {
            return; // not something we asked this peer for
        }
        fetch.asked.remove(&chunk);
        let parsed: Vec<Sha256Hash> = hashes
            .as_chunks::<32>()
            .0
            .iter()
            .map(|c| Sha256Hash(*c))
            .collect();
        let Some(good) = fetch.shape.verify(request, &parsed) else {
            tracing::debug!(key, "piece layer hashes didn't prove out");
            fetch.refused.insert(key);
            return;
        };
        fetch.chunks[chunk as usize] = Some(good);
        if fetch.chunks.iter().any(Option::is_none) {
            return;
        }
        let fetch = self
            .layer_fetches
            .remove(&request.pieces_root)
            .expect("present");
        let mut layer: Vec<Sha256Hash> = fetch.chunks.into_iter().flatten().flatten().collect();
        layer.truncate(fetch.shape.pieces as usize);
        if let Err(e) = self.torrent.info.validate_piece_layer(fetch.file, &layer) {
            tracing::warn!(error = %e, "assembled piece layer is wrong; starting over");
            self.plan_layer_fetches();
            return;
        }
        let mut hashes = (*self.hashes).clone();
        hashes.add_layer(fetch.shape.root, layer.into());
        self.hashes = Arc::new(hashes);
        tracing::debug!(file = fetch.file, "piece layer fetched");
        self.refresh_priorities();
    }

    fn on_hash_reject(&mut self, key: PeerKey, request: &HashRequest) {
        if let Some(fetch) = self.layer_fetches.get_mut(&request.pieces_root) {
            let chunk = request.index / fetch.shape.chunk_len().max(1);
            if fetch.asked.get(&chunk).is_some_and(|(k, _)| *k == key) {
                fetch.asked.remove(&chunk);
            }
            fetch.refused.insert(key);
        }
        self.request_layers();
    }

    /// Answer a peer's hash request from the piece layers we have.
    fn on_hash_request(&self, key: PeerKey, request: HashRequest) {
        let Some(peer) = self.peers.get(&key) else {
            return;
        };
        let info = &self.torrent.info;
        let answer = info
            .files
            .iter()
            .find(|f| f.pieces_root == Some(request.pieces_root))
            .and_then(|f| LayerShape::new(request.pieces_root, f.length, info.piece_length))
            .and_then(|shape| {
                let layer = self.hashes.layer(&request.pieces_root)?;
                shape.answer(&request, layer)
            });
        match answer {
            Some(hashes) => {
                let mut bytes = Vec::with_capacity(hashes.len() * 32);
                for h in &hashes {
                    bytes.extend_from_slice(h.as_bytes());
                }
                peer.send(Message::Hashes {
                    request,
                    hashes: Bytes::from(bytes),
                });
            }
            None => peer.send(Message::HashReject(request)),
        }
    }

    /// Bring each file's placement in line with its priority: wanted files go to the save
    /// path, deselected ones to the held area or the part store.
    fn place_files(&mut self) {
        let Some(prios) = self.file_priorities.clone() else {
            return;
        };
        let current = self.storage.placements();
        let sizes = self.storage.file_sizes();
        let mut moves = Vec::new();
        for (i, f) in self.layout.files().iter().enumerate() {
            if f.pad || self.moving.contains(&i) {
                continue;
            }
            let target = match (prios[i], current[i]) {
                (0, Placement::Save) => {
                    let has_data = sizes[i].is_some_and(|n| n > 0);
                    if has_data && self.unwanted == UnwantedPolicy::Hold {
                        Placement::Held
                    } else {
                        Placement::Parts
                    }
                }
                (0, other) => other,
                (_, _) => Placement::Save,
            };
            if target != current[i] {
                moves.push((i, target));
            }
        }
        self.start_moves(moves);
    }

    /// Run placement changes on a disk thread. Pieces whose data is about to be deleted are
    /// dropped first, so nothing tries to read them meanwhile.
    fn start_moves(&mut self, moves: Vec<(usize, Placement)>) {
        if moves.is_empty() {
            return;
        }
        let prios = self.file_priorities.clone();
        let wanted_file = |file: usize| prios.as_ref().is_none_or(|p| p[file] > 0);
        let mut jobs = Vec::new();
        for (file, to) in moves {
            let mut keep = Vec::new();
            if to == Placement::Parts {
                for piece in self.layout.file_pieces(file) {
                    let have = self.picker.as_ref().is_some_and(|p| p.have().get(piece));
                    if !have || self.storage.in_parts(piece) {
                        continue;
                    }
                    let shared_with_wanted = self
                        .layout
                        .segments(
                            self.layout.piece_offset(piece),
                            self.layout.piece_size(piece) as usize,
                        )
                        .iter()
                        .any(|s| s.file != file && !s.pad && wanted_file(s.file));
                    if shared_with_wanted {
                        keep.push(piece);
                    } else if let Some(picker) = self.picker.as_mut() {
                        picker.mark_missing(piece);
                        self.piece_dropped(piece);
                    }
                }
            }
            self.moving.insert(file);
            jobs.push((file, to, keep));
        }
        let storage = self.storage.clone();
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let files: Vec<usize> = jobs.iter().map(|j| j.0).collect();
            let result = jobs
                .iter()
                .try_for_each(|(file, to, keep)| storage.set_placement(*file, *to, keep))
                .map_err(|e| format!("moving files failed: {e}"));
            let _ = tx.blocking_send(ActorMsg::Moved { files, result });
        });
    }

    fn on_moved(&mut self, files: &[usize], result: Result<(), String>) {
        for f in files {
            self.moving.remove(f);
        }
        if self.recheck_pending {
            // The check reads every file where it now is.
            if let Err(e) = result {
                tracing::error!(error = %e, "placement change failed");
            }
            self.recheck_if_pending();
            return;
        }
        // A piece that finished while its file was moving may have been written to the
        // old place. Drop any piece whose part-store bytes aren't there.
        let placements = self.storage.placements();
        let mut dropped = Vec::new();
        if let Some(picker) = self.picker.as_mut() {
            for &file in files {
                if placements[file] != Placement::Parts {
                    continue;
                }
                for piece in self.layout.file_pieces(file) {
                    if picker.have().get(piece) && !self.storage.in_parts(piece) {
                        picker.mark_missing(piece);
                        dropped.push(piece);
                    }
                }
            }
        }
        for piece in dropped {
            self.piece_dropped(piece);
        }
        if let Err(e) = result {
            tracing::error!(error = %e, "placement change failed");
            self.fail(e);
            return;
        }
        // Priorities may have changed again while the move ran.
        self.place_files();
        self.publish_stats();
    }

    async fn run(mut self) {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                msg = self.rx.recv() => {
                    let Some(msg) = msg else { break };
                    if let ActorMsg::Command(Command::Shutdown(reply)) = msg {
                        self.send_stopped();
                        self.shutdown_peers();
                        let _ = self.storage.sync();
                        let have = self.picker.as_ref().map_or_else(
                            || Bitfield::new(self.layout.piece_count()),
                            |p| p.have().clone(),
                        );
                        // Gone from the session before the reply, so the torrent can be
                        // added again (on other files) as soon as shutdown returns.
                        self.shared.unregister(&self.info_hashes);
                        let _ = reply.send(have);
                        break;
                    }
                    self.handle(msg);
                }
                Some((key, event)) = self.peer_rx.recv() => self.handle_peer(key, event),
                Some(event) = self.webseed_rx.recv() => self.on_webseed(event),
                _ = tick.tick() => self.on_tick(),
            }
        }
        self.shared.memory.sub(self.memory_held);
        self.shared.unregister(&self.info_hashes);
    }

    fn handle(&mut self, msg: ActorMsg) {
        match msg {
            ActorMsg::Command(cmd) => match cmd {
                Command::AddPeers(peers) => {
                    self.add_candidates(peers, None);
                    self.connect_more();
                }
                Command::Pause => {
                    let keys: Vec<EntryKey> = self.entries.iter().map(|e| e.key).collect();
                    for key in keys {
                        self.pause_entry(key);
                    }
                    self.publish_stats();
                }
                Command::Resume => {
                    // Resuming is also how the user retries after a disk error.
                    self.error = None;
                    for e in &mut self.entries {
                        e.paused = false;
                    }
                    self.announce();
                    self.connect_more();
                    self.publish_stats();
                }
                Command::PauseEntry(key) => {
                    self.pause_entry(key);
                    self.publish_stats();
                }
                Command::ResumeEntry(key) => {
                    self.error = None;
                    if let Some(e) = self.entry_mut(key) {
                        e.paused = false;
                    }
                    self.announce();
                    self.connect_more();
                    self.publish_stats();
                }
                Command::AddEntry(params) => {
                    if self.entry(params.key).is_none() {
                        let entry = Entry::new(params, &self.torrent, &self.keys, &self.shared);
                        self.entries.push(entry);
                        self.refresh_identity(true);
                        self.announce();
                        self.connect_more();
                        self.publish_stats();
                    }
                }
                Command::RemoveEntry(key) => {
                    self.pause_entry(key);
                    self.entries.retain(|e| e.key != key);
                    self.refresh_identity(true);
                    // Its trackers may now be the next entry's to announce to.
                    self.announce();
                    self.connect_more();
                    self.publish_stats();
                }
                Command::Have(reply) => {
                    // While checking, nothing is known: no answer, rather than an empty
                    // bitfield someone could save as "none of it is there".
                    if let Some(p) = &self.picker {
                        let _ = reply.send(p.have().clone());
                    }
                }
                Command::Recheck => self.recheck(),
                Command::FilePriorities(prios) => self.set_file_priorities(&prios),
                Command::FileOrder(order) => {
                    self.file_order = order;
                    if let Some(picker) = self.picker.as_mut() {
                        picker.set_file_order(self.file_order.clone());
                    }
                    self.fill_all();
                }
                Command::Stream(file, from) => {
                    if file < self.layout.files().len() {
                        self.streams.retain(|s| s.0 != file);
                        if let Some(from) = from {
                            self.streams.push((file, from));
                        }
                        if let Some(picker) = self.picker.as_mut() {
                            picker.set_stream(file, from);
                        }
                        self.fill_all();
                    }
                }
                Command::FirstLast(on) => {
                    self.first_last = on;
                    if let Some(picker) = self.picker.as_mut() {
                        picker.set_first_last(on);
                    }
                    self.fill_all();
                }
                Command::SuperSeed(on) => self.set_super_seed(on),
                Command::SetUnwantedPolicy(policy) => self.unwanted = policy,
                Command::SetLimits(key, limits) => {
                    if let Some(e) = self.entry_mut(key) {
                        e.throttle.torrent_down.set_rate(limits.download_rate);
                        e.throttle.torrent_up.set_rate(limits.upload_rate);
                        e.limits = limits;
                    }
                    self.connect_more();
                }
                Command::SetGroup(key, name) => {
                    let group = name.as_deref().and_then(|g| self.shared.group(g));
                    if let Some(e) = self.entry(key) {
                        e.throttle.set_group(group);
                    }
                }
                Command::SetSpace(key, name) => {
                    let space = name.as_deref().and_then(|s| self.shared.space(s));
                    if let Some(e) = self.entry(key) {
                        e.throttle.set_space(space);
                    }
                }
                Command::SetTrackers(key, tiers) => {
                    let now = Instant::now();
                    let stops = self.entry_mut(key).map(|e| {
                        e.urls = tiers.iter().flatten().cloned().collect();
                        let totals = e.totals();
                        e.announcer.retarget(&tiers, now, totals)
                    });
                    for job in stops.unwrap_or_default() {
                        self.spawn_announce(key, job, false);
                    }
                    self.refresh_identity(true);
                    self.announce();
                }
                Command::ReleaseHeld(files) => {
                    let placements = self.storage.placements();
                    let moves = files
                        .into_iter()
                        .filter(|&f| {
                            placements.get(f) == Some(&Placement::Held) && !self.moving.contains(&f)
                        })
                        .map(|f| (f, Placement::Parts))
                        .collect();
                    self.start_moves(moves);
                    self.publish_stats();
                }
                Command::Trackers(key, reply) => {
                    let _ = reply.send(self.tracker_status(key));
                }
                Command::OfferPiece(piece, data, reply) => self.offer_piece(piece, data, reply),
                Command::AdoptFile(file, from, mode, reply) => {
                    self.adopt_file(file, from, mode, reply);
                }
                Command::SetReused(files) => {
                    for f in files {
                        if let Some(r) = self.reused.get_mut(f) {
                            *r = true;
                        }
                    }
                    self.set_bookends();
                    self.plan_prefills();
                }
                Command::Peers(reply) => {
                    let _ = reply.send(self.peer_infos());
                }
                Command::Shutdown(_) => unreachable!("handled in run"),
            },
            ActorMsg::Incoming {
                stream,
                handshake,
                link,
                addr,
            } => self.accept(stream, handshake, link, addr),
            ActorMsg::Checked(have) => {
                self.start(have);
                self.publish_stats();
                self.connect_more();
            }
            ActorMsg::PieceWritten {
                piece,
                result,
                digests,
            } => self.on_piece_written(piece, result, digests.as_deref()),
            ActorMsg::BlocksRead {
                peer,
                blocks,
                result,
            } => self.on_blocks_read(peer, &blocks, result),
            ActorMsg::Announced { entry, job, result } => self.on_announced(entry, &job, result),
            ActorMsg::Moved { files, result } => self.on_moved(&files, result),
            ActorMsg::OfferWritten {
                piece,
                result,
                reply,
            } => {
                self.on_offer_written(piece, result, reply);
                if self.offered.is_empty() && self.adopting.is_empty() {
                    self.recheck_if_pending();
                }
            }
            ActorMsg::FileAdopted {
                file,
                result,
                reply,
            } => {
                self.on_file_adopted(file, result, reply);
                if self.offered.is_empty() && self.adopting.is_empty() {
                    self.recheck_if_pending();
                }
            }
            ActorMsg::Prefilled { piece, result } => self.on_prefilled(piece, result),
        }
    }

    // ----- trackers -----------------------------------------------------------------------

    /// Choose the identity again if the session's rules changed (or `force`, when the
    /// torrent's trackers did). On a change, trackers that heard the old identity's
    /// `started` are told `stopped` under it, and the new identity's `started` waits for
    /// that answer (see [`Announcer::stop`]). The new identity shares nothing a tracker
    /// could tie to the old one: a fresh peer id and key (apitorrent's own too), no
    /// `trackerid`, and stats counted from zero. Open peer connections keep the peer id
    /// they were made with; trackers never see those.
    fn refresh_identity(&mut self, force: bool) {
        if !force && self.shared.identity_gen() == self.identity_gen {
            return;
        }
        let (mut fresh, generation) = ActiveIdentity::resolve(
            &self.shared,
            self.entries
                .iter()
                .flat_map(|e| e.urls.iter().map(String::as_str)),
        );
        self.identity_gen = generation;
        if fresh.profile == self.identity.profile {
            return;
        }
        fresh.renew_native();
        tracing::info!(
            torrent = %self.short_hash,
            from = %self.identity.describe(),
            to = %fresh.describe(),
            "client identity changed"
        );
        let now = Instant::now();
        let mut stops = Vec::new();
        for e in &mut self.entries {
            let totals = e.totals();
            stops.extend(
                e.announcer
                    .stop(now, totals)
                    .into_iter()
                    .map(|j| (e.key, j)),
            );
        }
        // Spawned before the switch, so they go out under the old identity.
        for (key, job) in stops {
            self.spawn_announce(key, job, true);
        }
        self.identity = fresh;
    }

    fn announce(&mut self) {
        self.refresh_identity(false);
        if self.error.is_some() || self.picker.is_none() {
            return;
        }
        let now = Instant::now();
        let mut jobs = Vec::new();
        // URLs an older running entry lists are that entry's to announce to.
        let mut covered: HashSet<String> = HashSet::new();
        for e in &mut self.entries {
            if e.paused {
                continue;
            }
            let totals = e.totals();
            for job in e
                .announcer
                .due_where(now, totals, |url| !covered.contains(url))
            {
                jobs.push((e.key, job));
            }
            covered.extend(e.urls.iter().cloned());
        }
        for (key, job) in jobs {
            self.spawn_announce(key, job, true);
        }
    }

    /// Each tracker's status for one entry (or every entry). A URL an older running entry
    /// also lists shows that entry's status, since it is the one announcing there.
    fn tracker_status(&self, key: Option<EntryKey>) -> Vec<TrackerStatus> {
        let mut by_url: HashMap<String, TrackerStatus> = HashMap::new();
        let running = self.entries.iter().filter(|e| !e.paused);
        for e in running.chain(self.entries.iter().filter(|e| e.paused)) {
            for s in e.announcer.status() {
                by_url.entry(s.url.clone()).or_insert(s);
            }
        }
        let mut out: Vec<TrackerStatus> = Vec::new();
        for e in &self.entries {
            if key.is_some_and(|k| k != e.key) {
                continue;
            }
            for s in e.announcer.status() {
                let tier = s.tier;
                let shown = by_url.get(&s.url).cloned().unwrap_or(s);
                if !out.iter().any(|o| o.url == shown.url) {
                    out.push(TrackerStatus { tier, ..shown });
                }
            }
        }
        out
    }

    fn spawn_announce(&self, key: EntryKey, job: Job, report: bool) {
        let id = &self.identity;
        let req = apitorrent_tracker::AnnounceRequest {
            info_hash: job.info_hash,
            peer_id: id.peer_id,
            port: self.shared.listen_port(),
            uploaded: job.report.uploaded,
            downloaded: job.report.downloaded,
            left: self.bytes_left(),
            event: job.event,
            num_want: Some(id.num_want(job.event, self.finished)),
            key: id.key,
            tracker_id: job.tracker_id.clone(),
            style: id.announce_style(),
            user_agent: id.user_agent(),
            crypto: announce_crypto(self.shared.dialer.encryption()),
        };
        let client = self.shared.tracker.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = client
                .announce(&job.url, &req)
                .await
                .map_err(|e| e.to_string());
            if report {
                let _ = tx
                    .send(ActorMsg::Announced {
                        entry: key,
                        job,
                        result,
                    })
                    .await;
            }
        });
    }

    /// Best-effort `stopped` announces for every entry; their results are not waited for.
    fn send_stopped(&mut self) {
        let keys: Vec<EntryKey> = self.entries.iter().map(|e| e.key).collect();
        for key in keys {
            self.send_stopped_for(key);
        }
    }

    fn send_stopped_for(&mut self, key: EntryKey) {
        let Some(e) = self.entry_mut(key) else {
            return;
        };
        let totals = e.totals();
        let jobs = e.announcer.stop(Instant::now(), totals);
        // Reported, so a quick resume's `started` waits for the `stopped` to land.
        for job in jobs {
            self.spawn_announce(key, job, true);
        }
    }

    fn on_announced(
        &mut self,
        key: EntryKey,
        job: &Job,
        result: Result<apitorrent_tracker::AnnounceResponse, String>,
    ) {
        let now = Instant::now();
        if let Ok(resp) = &result {
            // The tracker knows where we are now, whatever became of the entry.
            self.shared.reach.announced(resp.external_ip, now);
        }
        let Some(entry) = self.entry_mut(key) else {
            return;
        };
        match result {
            Ok(resp) => {
                tracing::debug!(
                    url = %apitorrent_tracker::passkey::redact(&job.url),
                    peers = resp.peers.len(),
                    "announced"
                );
                entry.announcer.on_success(job, &resp, now);
                // A stopped entry's late answer brings no peers.
                if !entry.paused {
                    self.add_candidates(resp.peers, Some(key));
                    self.connect_more();
                }
            }
            Err(e) => {
                tracing::debug!(
                    url = %apitorrent_tracker::passkey::redact(&job.url),
                    error = %apitorrent_tracker::passkey::redact_text(&e),
                    "announce failed"
                );
                entry.announcer.on_failure(job, &e, now);
            }
        }
        // A failure may make the next tracker in the tier due right away.
        self.announce();
    }

    /// Bytes of wanted pieces we don't have yet.
    fn bytes_left(&self) -> u64 {
        let Some(picker) = &self.picker else {
            return self.torrent.info.content_length();
        };
        (0..self.layout.piece_count())
            .filter(|&p| picker.is_wanted(p))
            .map(|p| u64::from(self.layout.piece_size(p)))
            .sum()
    }

    // ----- connections ------------------------------------------------------------------

    /// Addresses to dial; `from` is the entry whose tracker named them.
    fn add_candidates(&mut self, peers: Vec<SocketAddr>, from: Option<EntryKey>) {
        for addr in peers {
            if self.known.insert(addr) {
                self.candidates.push_back((addr, from));
            }
        }
    }

    // ----- entries ----------------------------------------------------------------------

    fn entry(&self, key: EntryKey) -> Option<&Entry> {
        self.entries.iter().find(|e| e.key == key)
    }

    fn entry_mut(&mut self, key: EntryKey) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| e.key == key)
    }

    /// The oldest running entry: DHT, PEX, LSD, web seed and incoming peers are its.
    fn default_entry(&self) -> Option<&Entry> {
        self.entries.iter().find(|e| !e.paused)
    }

    fn default_entry_mut(&mut self) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| !e.paused)
    }

    /// Whether the torrent transfers at all: some entry runs and no disk error stopped it.
    fn running(&self) -> bool {
        self.error.is_none() && self.entries.iter().any(|e| !e.paused)
    }

    /// Stop one entry: tell its trackers, and close its connections. Outbound peers stay
    /// candidates for whichever entry runs next.
    fn pause_entry(&mut self, key: EntryKey) {
        let Some(e) = self.entry_mut(key) else {
            return;
        };
        if e.paused {
            return;
        }
        e.paused = true;
        self.send_stopped_for(key);
        let peers: Vec<PeerKey> = self
            .peers
            .iter()
            .filter(|(_, p)| p.entry == key)
            .map(|(k, _)| *k)
            .collect();
        self.close_peers(&peers);
    }

    /// The torrent's cap on piece data in memory: the tightest any entry sets.
    fn memory_limit(&self) -> Option<u64> {
        self.entries.iter().filter_map(|e| e.limits.memory).min()
    }

    fn max_peers(&self, entry: &Entry) -> usize {
        entry
            .limits
            .max_connections
            .unwrap_or(self.shared.config.max_peers_per_torrent)
    }

    /// Whether an entry runs and may have one more connection.
    fn has_room(&self, key: EntryKey) -> bool {
        self.entry(key).is_some_and(|e| {
            !e.paused && self.peers.values().filter(|p| p.entry == key).count() < self.max_peers(e)
        })
    }

    /// The entry a new connection would belong to: the one whose tracker named the peer if
    /// it runs, else the oldest running entry.
    fn entry_for(&self, from: Option<EntryKey>) -> Option<EntryKey> {
        from.filter(|&k| self.entry(k).is_some_and(|e| !e.paused))
            .or_else(|| self.default_entry().map(|e| e.key))
    }

    /// Take a unit of the session's, space's and group's connection counters, if all have
    /// room.
    fn connection_slots(&self, key: EntryKey) -> Option<Vec<Slot>> {
        let throttle = &self.entry(key)?.throttle;
        let mut slots = vec![self.shared.connections.try_acquire()?];
        for g in [throttle.space(), throttle.group()].into_iter().flatten() {
            slots.push(g.connections.try_acquire()?);
        }
        Some(slots)
    }

    fn connect_more(&mut self) {
        if !self.running() || self.picker.is_none() {
            return;
        }
        let now = Instant::now();
        let mut deferred = Vec::new();
        let mut full: HashSet<EntryKey> = HashSet::new();
        while let Some((addr, from)) = self.candidates.pop_front() {
            let Some(entry) = self.entry_for(from) else {
                self.candidates.push_front((addr, from));
                break;
            };
            if full.contains(&entry) || !self.has_room(entry) {
                full.insert(entry);
                deferred.push((addr, from));
                if self
                    .entries
                    .iter()
                    .all(|e| e.paused || full.contains(&e.key))
                {
                    break;
                }
                continue;
            }
            if self.peers.values().any(|p| p.addr == addr) {
                continue;
            }
            if self.shared.filter.is_blocked(addr.ip()) {
                self.retry_at.insert(addr, now + RETRY_AFTER);
                deferred.push((addr, from));
                continue;
            }
            // Finished what we want (every piece, or every selected one): seeds and
            // peers that want nothing more have nothing for us.
            let complete = self.picker.as_ref().is_some_and(Picker::is_finished);
            if self.retry_at.get(&addr).is_some_and(|&at| now < at)
                || (complete && self.seed_addrs.contains(&addr))
            {
                deferred.push((addr, from));
                continue;
            }
            // Room under every connection limit, and the attempt rate allows one more?
            let Some(slots) = self.connection_slots(entry) else {
                self.candidates.push_front((addr, from));
                break;
            };
            let Some(half_open) = self.shared.half_open.try_acquire() else {
                self.candidates.push_front((addr, from));
                break;
            };
            if !self.shared.connect_rate.try_take(1) {
                self.candidates.push_front((addr, from));
                break;
            }
            let throttle = self.entry(entry).expect("checked").throttle.clone();
            let key = self.shared.next_key();
            let mut peer = Peer::new(addr, entry, self.layout.piece_count(), true, slots);
            peer.half_open = Some(half_open);
            self.peers.insert(key, peer);
            let ours = self.our_handshake();
            let hint = self.utp_hints.remove(&addr).unwrap_or_default();
            tokio::spawn(peer::run_outbound(
                addr,
                hint,
                ours,
                key,
                self.max_message,
                self.peer_tx.clone(),
                throttle,
                self.shared.dialer.clone(),
            ));
        }
        self.candidates.extend(deferred);
    }

    fn our_handshake(&self) -> Handshake {
        let wire = self.identity.wire();
        peer::our_handshake(
            self.short_hash,
            self.identity.peer_id,
            wire.v2(
                self.info_hashes.get_v2().is_some(),
                self.info_hashes.get_v1().is_some(),
            ),
            wire.dht(self.shared.dht.is_some(), self.torrent.info.private),
        )
    }

    fn accept(
        &mut self,
        stream: peer::BoxIo,
        handshake: Handshake,
        link: peer::Link,
        addr: SocketAddr,
    ) {
        if !self.running() || self.picker.is_none() {
            return;
        }
        // Incoming peers are the oldest running entry's, or the next one with room.
        let Some(entry) = self
            .entries
            .iter()
            .map(|e| e.key)
            .find(|&k| self.has_room(k))
        else {
            return;
        };
        let Some(slots) = self.connection_slots(entry) else {
            return;
        };
        let throttle = self.entry(entry).expect("has room").throttle.clone();
        let key = self.shared.next_key();
        self.peers.insert(
            key,
            Peer::new(addr, entry, self.layout.piece_count(), false, slots),
        );
        // Answer with the hash the peer asked for: v1, or truncated v2 for a v2 swarm.
        let mut ours = self.our_handshake();
        ours.info_hash = handshake.info_hash;
        tokio::spawn(peer::run_inbound(
            stream,
            handshake,
            link,
            ours,
            key,
            self.max_message,
            self.peer_tx.clone(),
            throttle,
        ));
    }

    /// Close every connection. Outbound peers stay candidates, so resuming dials them
    /// again straight away.
    fn shutdown_peers(&mut self) {
        let keys: Vec<PeerKey> = self.peers.keys().copied().collect();
        self.close_peers(&keys);
    }

    /// Close these connections. Outbound peers go back to the candidates (no longer tied
    /// to an entry), so whichever entry runs next can dial them straight away.
    fn close_peers(&mut self, keys: &[PeerKey]) {
        for &key in keys {
            if let Some(p) = self.peers.get(&key) {
                p.close();
                if p.outbound {
                    self.retry_at.remove(&p.addr);
                    self.candidates.push_back((p.addr, None));
                }
            }
            self.drop_peer(key);
        }
    }

    fn drop_peer(&mut self, key: PeerKey) {
        let Some(peer) = self.peers.remove(&key) else {
            return;
        };
        if let Some(picker) = self.picker.as_mut() {
            picker.peer_gone(key);
            if peer.is_seed {
                picker.peer_seed_removed();
            } else {
                picker.peer_bitfield_lost(&peer.has);
            }
        }
    }

    // ----- peer events --------------------------------------------------------------------

    fn handle_peer(&mut self, key: PeerKey, event: PeerEvent) {
        match event {
            PeerEvent::Ready {
                reserved,
                peer_id,
                link,
                tx,
            } => {
                if let Some(p) = self.peers.get_mut(&key) {
                    p.link = link;
                }
                self.on_ready(key, reserved, peer_id, tx);
            }
            PeerEvent::Message(m) => {
                if let Err(reason) = self.on_message(key, m) {
                    tracing::debug!(key, %reason, "dropping peer");
                    if let Some(p) = self.peers.get(&key) {
                        p.close();
                    }
                    self.drop_peer(key);
                }
            }
            PeerEvent::Sent { bytes } => {
                if let Some(p) = self.peers.get_mut(&key) {
                    p.unsent = p.unsent.saturating_sub(bytes);
                    self.issue_reads(key);
                }
            }
            PeerEvent::Closed { reason } => {
                if let Some(p) = self.peers.get(&key) {
                    tracing::debug!(key, addr = %p.addr, %reason, "peer closed");
                    if p.outbound {
                        // Never reached: back off longer than after a normal close.
                        let wait = if p.tx.is_none() {
                            RETRY_AFTER
                        } else {
                            RECONNECT_AFTER
                        };
                        self.retry_at.insert(p.addr, Instant::now() + wait);
                        self.candidates.push_back((p.addr, Some(p.entry)));
                    }
                }
                let unreached = self
                    .peers
                    .get(&key)
                    .filter(|p| p.outbound && p.tx.is_none())
                    .map(|p| p.addr);
                self.drop_peer(key);
                if let Some(addr) = unreached {
                    self.ask_introducer(addr);
                }
                self.connect_more();
            }
        }
    }

    /// Whether connection `key`, just ready with `peer_id`, should stay when the same peer
    /// is already connected (a holepunch has both ends dial at once). One of an inbound
    /// and an outbound pair goes: the connection started by the end with the lower peer
    /// id stays, so both ends close the same one. A second connection the same way round
    /// goes. The other one is closed here when this one wins.
    fn keep_duplicate(&mut self, key: PeerKey, peer_id: PeerId) -> bool {
        let Some(this) = self.peers.get(&key) else {
            return false;
        };
        let (ip, outbound) = (this.addr.ip(), this.outbound);
        let Some((&other, other_outbound)) = self
            .peers
            .iter()
            .find(|(k, p)| {
                **k != key && p.tx.is_some() && p.peer_id == Some(peer_id) && p.addr.ip() == ip
            })
            .map(|(k, p)| (k, p.outbound))
        else {
            return true;
        };
        if outbound == other_outbound {
            return false;
        }
        let keep_ours = self.identity.peer_id.0 < peer_id.0;
        if outbound != keep_ours {
            tracing::debug!(key, "closing a second connection to the same peer");
            return false;
        }
        tracing::debug!(key = other, "closing a second connection to the same peer");
        if let Some(p) = self.peers.get(&other) {
            p.close();
        }
        self.drop_peer(other);
        true
    }

    fn on_ready(
        &mut self,
        key: PeerKey,
        reserved: apitorrent_wire::Reserved,
        peer_id: PeerId,
        tx: mpsc::UnboundedSender<PeerCmd>,
    ) {
        if self.picker.is_none() {
            let _ = tx.send(PeerCmd::Close);
            return;
        }
        if !self.keep_duplicate(key, peer_id) {
            // Gone now, so its close isn't taken for a failed dial.
            let _ = tx.send(PeerCmd::Close);
            self.drop_peer(key);
            return;
        }
        // Brought up to date first, so this peer hears the same as the others.
        self.sync_upload_only();
        let have = self.picker.as_ref().expect("checked").have().clone();
        let wire = self.identity.wire();
        let Some(peer) = self.peers.get_mut(&key) else {
            let _ = tx.send(PeerCmd::Close);
            return;
        };
        peer.tx = Some(tx);
        peer.half_open = None;
        peer.peer_id = Some(peer_id);
        peer.fast = reserved.fast();
        peer.v2 = reserved.v2();
        peer.our_reqq = wire.request_queue;
        // Tell DHT-capable peers where our DHT listens (BEP 5). Not for private torrents:
        // their peers come only from the tracker.
        if reserved.dht()
            && !self.torrent.info.private
            && let Some(port) = self.shared.dht_port()
        {
            peer.send(Message::Port(port));
        }
        if reserved.extension_protocol() {
            let mut ext = ExtensionHandshake {
                client: Some(self.identity.client_name(&self.shared.config.client_name)),
                request_queue: Some(wire.request_queue),
                metadata_size: Some(self.torrent.info.raw().len() as u64),
                upload_only: self.told_upload_only,
                listen_port: Some(self.shared.listen_port()),
                your_ip: Some(peer.addr.ip()),
                ..Default::default()
            };
            ext.extensions
                .insert(names::UT_METADATA.to_owned(), OUR_UT_METADATA);
            // No peer exchange for private torrents (BEP 27): peers come from the tracker.
            if !self.torrent.info.private {
                ext.extensions.insert(names::UT_PEX.to_owned(), OUR_UT_PEX);
            }
            if wire.libtorrent_extensions {
                ext.extensions
                    .insert(names::UPLOAD_ONLY.to_owned(), OUR_UPLOAD_ONLY);
                ext.extensions
                    .insert(names::LT_DONTHAVE.to_owned(), OUR_LT_DONTHAVE);
                ext.extensions
                    .insert(names::UT_HOLEPUNCH.to_owned(), OUR_UT_HOLEPUNCH);
            }
            peer.send(Message::Extended {
                id: 0,
                payload: Bytes::from(ext.encode()),
            });
        }
        let super_seeding = self.super_seed && have.is_full();
        if super_seeding {
            // Nothing yet: the pieces on offer follow as `have`s.
            if peer.fast {
                peer.send(Message::HaveNone);
            }
        } else if peer.fast && have.is_full() {
            peer.send(Message::HaveAll);
        } else if peer.fast && have.none_set() {
            peer.send(Message::HaveNone);
        } else if !have.none_set() {
            peer.send(Message::Bitfield(Bytes::copy_from_slice(have.as_bytes())));
        }
        if super_seeding {
            self.superseed_offer(key);
        }
        self.publish_stats();
    }

    fn on_message(&mut self, key: PeerKey, m: Message) -> Result<(), String> {
        let n = self.layout.piece_count();
        let Some(picker) = self.picker.as_mut() else {
            return Ok(());
        };
        let Some(peer) = self.peers.get_mut(&key) else {
            return Ok(());
        };
        match m {
            Message::KeepAlive | Message::Suggest(_) | Message::Unknown { .. } => {}
            Message::Bitfield(bytes) => {
                let has = Bitfield::from_bytes(&bytes, n).ok_or("invalid bitfield")?;
                set_peer_has(picker, peer, has);
                // Offers made before the bitfield came may be pieces it already had.
                let had = peer.has.clone();
                peer.offered.retain(|&p| !had.get(p));
                self.update_interest(key);
                self.superseed_offer(key);
            }
            Message::HaveAll => {
                if !peer.fast {
                    return Err("have_all without the fast extension".into());
                }
                set_peer_has(picker, peer, Bitfield::full(n));
                peer.offered.clear();
                self.update_interest(key);
            }
            Message::HaveNone => {
                if !peer.fast {
                    return Err("have_none without the fast extension".into());
                }
            }
            Message::Have(piece) => {
                if piece >= n {
                    return Err(format!("have for piece {piece} of {n}"));
                }
                if !peer.is_seed && peer.has.set(piece) {
                    picker.peer_has(piece);
                    if picker.is_wanted(piece) {
                        peer.wanted += 1;
                    }
                    if peer.has.is_full() {
                        picker.peer_bitfield_lost(&peer.has);
                        picker.peer_seed_added();
                        peer.is_seed = true;
                    }
                }
                self.update_interest(key);
                self.superseed_done(key, piece);
            }
            Message::Choke => {
                peer.peer_choking = true;
                if !peer.fast {
                    for b in peer.requests.drain() {
                        picker.request_failed(key, &b);
                    }
                }
            }
            Message::Unchoke => {
                peer.peer_choking = false;
                self.fill_requests(key);
            }
            Message::Interested => {
                peer.peer_interested = true;
                self.maybe_unchoke(key);
            }
            Message::NotInterested => {
                peer.peer_interested = false;
                // A super-seeded peer that wants nothing more has what it was shown (a
                // client that leaves out `have`s for pieces it thinks we have says so only
                // this way): show it more.
                let had_offers = !peer.offered.is_empty();
                if had_offers && self.super_seeding() {
                    if let Some(p) = self.peers.get_mut(&key) {
                        p.offered.clear();
                    }
                    self.superseed_offer(key);
                }
            }
            Message::Request(block) => self.on_request(key, block)?,
            Message::Cancel(block) => {
                // A queued request is dropped, and a read in flight isn't sent when it
                // ends; anything already handed to the connection is gone. With the Fast
                // extension every request gets an answer (BEP 6): here, a reject.
                if let Some(i) = peer.upload_queue.iter().position(|b| *b == block) {
                    peer.upload_queue.remove(i);
                    if peer.fast {
                        peer.send(Message::Reject(block));
                    }
                } else if peer.reading > 0 && peer.cancelled.len() < peer.reading {
                    peer.cancelled.insert(block);
                }
            }
            Message::Reject(block) => {
                if !peer.fast {
                    return Err("reject without the fast extension".into());
                }
                if peer.requests.remove(&block) {
                    picker.request_failed(key, &block);
                }
            }
            Message::AllowedFast(_) => {}
            Message::Piece {
                piece,
                offset,
                data,
            } => self.on_piece(key, piece, offset, &data),
            Message::Port(port) => {
                if let Some(dht) = &self.shared.dht
                    && port != 0
                    && !self.torrent.info.private
                {
                    dht.add_node(SocketAddr::new(peer.addr.ip(), port));
                }
            }
            Message::Extended { id: 0, payload } => {
                let ext = ExtensionHandshake::decode(&payload)
                    .map_err(|e| format!("bad extension handshake: {e}"))?;
                if let Some(ip) = ext.your_ip {
                    self.shared.reach.peer_says(peer.addr.ip(), ip);
                }
                peer.upload_only = ext.upload_only;
                peer.extensions = Some(ext);
                self.update_interest(key);
            }
            Message::Extended {
                id: OUR_UPLOAD_ONLY,
                payload,
            } => {
                peer.upload_only = extended::decode_upload_only(&payload)
                    .map_err(|e| format!("bad upload_only message: {e}"))?;
                self.update_interest(key);
            }
            Message::Extended {
                id: OUR_UT_HOLEPUNCH,
                payload,
            } => {
                let msg = HolepunchMessage::decode(&payload)
                    .map_err(|e| format!("bad ut_holepunch message: {e}"))?;
                self.on_holepunch(key, msg);
            }
            Message::Extended {
                id: OUR_LT_DONTHAVE,
                payload,
            } => {
                let piece = extended::decode_dont_have(&payload)
                    .map_err(|e| format!("bad lt_donthave message: {e}"))?;
                if piece >= n {
                    return Err(format!("lt_donthave for piece {piece} of {n}"));
                }
                self.peer_lost_piece(key, piece);
            }
            Message::Extended {
                id: OUR_UT_METADATA,
                payload,
            } => {
                let msg = MetadataMessage::decode(&payload)
                    .map_err(|e| format!("bad ut_metadata message: {e}"))?;
                self.on_metadata_message(key, &msg);
            }
            Message::Extended {
                id: OUR_UT_PEX,
                payload,
            } => {
                if !self.torrent.info.private {
                    let pex = PexMessage::decode(&payload)
                        .map_err(|e| format!("bad ut_pex message: {e}"))?;
                    let mut added = Vec::with_capacity(pex.added.len());
                    for (addr, flags) in pex.added {
                        // Kept only for peers we haven't dialed yet, and dropped at the dial.
                        if !self.known.contains(&addr) && self.utp_hints.len() < MAX_UTP_HINTS {
                            let hint = if flags.supports_utp() {
                                peer::UtpHint::Yes
                            } else {
                                peer::UtpHint::No
                            };
                            self.utp_hints.insert(addr, hint);
                        }
                        // The latest peer to name it is the one to ask.
                        if flags.supports_holepunch()
                            && (self.introducers.len() < MAX_UTP_HINTS
                                || self.introducers.contains_key(&addr))
                        {
                            self.introducers.insert(addr, key);
                        }
                        added.push(addr);
                    }
                    self.add_candidates(added, None);
                    self.connect_more();
                }
            }
            Message::Extended { .. } => {}
            Message::HashRequest(r) => self.on_hash_request(key, r),
            Message::Hashes { request, hashes } => self.on_hashes(key, &request, &hashes),
            Message::HashReject(r) => self.on_hash_reject(key, &r),
        }
        Ok(())
    }

    /// Answer a peer's request for a piece of the info dictionary (BEP 9).
    fn on_metadata_message(&mut self, key: PeerKey, msg: &MetadataMessage) {
        let &MetadataMessage::Request { piece } = msg else {
            // We have the metadata; data and rejects from others mean nothing to us.
            return;
        };
        let Some(peer) = self.peers.get(&key) else {
            return;
        };
        let Some(their_id) = peer
            .extensions
            .as_ref()
            .and_then(|e| e.id_of(names::UT_METADATA))
        else {
            return;
        };
        let raw = self.torrent.info.raw();
        let start = piece as usize * METADATA_PIECE_LEN;
        let reply = if start < raw.len() {
            let end = (start + METADATA_PIECE_LEN).min(raw.len());
            MetadataMessage::Data {
                piece,
                total_size: raw.len() as u64,
                data: Bytes::copy_from_slice(&raw[start..end]),
            }
        } else {
            MetadataMessage::Reject { piece }
        };
        peer.send(Message::Extended {
            id: their_id,
            payload: Bytes::from(reply.encode()),
        });
    }

    /// The address a peer accepts connections on: its own for outbound connections, or the
    /// listen port from its extension handshake for inbound ones.
    fn reachable_addr(peer: &Peer) -> Option<SocketAddr> {
        if peer.outbound {
            return Some(peer.addr);
        }
        let port = peer.extensions.as_ref()?.listen_port?;
        Some(SocketAddr::new(peer.addr.ip(), port))
    }

    /// Tell each peer that supports it which peers we're connected to (BEP 11): what's new
    /// since the last message, and what went away.
    fn send_pex(&mut self) {
        if self.torrent.info.private {
            return;
        }
        let offer_holepunch = self.identity.wire().libtorrent_extensions;
        // What we know first-hand about each peer we pass on (BEP 11 flags).
        let connected: HashMap<SocketAddr, PexFlags> = self
            .peers
            .values()
            .filter(|p| p.tx.is_some())
            .filter_map(|p| {
                let mut flags = 0;
                if p.link.encrypted {
                    flags |= 0x01;
                }
                if p.is_seed {
                    flags |= 0x02;
                }
                if p.link.utp {
                    flags |= 0x04;
                }
                if offer_holepunch
                    && p.extensions
                        .as_ref()
                        .is_some_and(|e| e.id_of(names::UT_HOLEPUNCH).is_some())
                {
                    // We can introduce others to it.
                    flags |= 0x08;
                }
                if p.outbound {
                    // We reached it, so it accepts incoming connections.
                    flags |= 0x10;
                }
                Some((Self::reachable_addr(p)?, PexFlags(flags)))
            })
            .collect();
        for peer in self.peers.values_mut() {
            let Some(id) = peer
                .extensions
                .as_ref()
                .and_then(|e| e.id_of(names::UT_PEX))
            else {
                continue;
            };
            let me = Self::reachable_addr(peer);
            let current: HashSet<SocketAddr> = connected
                .keys()
                .copied()
                .filter(|a| Some(*a) != me)
                .collect();
            let added: Vec<(SocketAddr, PexFlags)> = current
                .difference(&peer.pex_sent)
                .take(PexMessage::MAX_PEERS)
                .map(|a| (*a, connected[a]))
                .collect();
            let dropped: Vec<SocketAddr> = peer
                .pex_sent
                .difference(&current)
                .take(PexMessage::MAX_PEERS)
                .copied()
                .collect();
            if added.is_empty() && dropped.is_empty() {
                continue;
            }
            for (a, _) in &added {
                peer.pex_sent.insert(*a);
            }
            for a in &dropped {
                peer.pex_sent.remove(a);
            }
            let msg = PexMessage { added, dropped };
            peer.send(Message::Extended {
                id,
                payload: Bytes::from(msg.encode()),
            });
        }
    }

    /// Tell peers when our `upload_only` changes: we finished what we want, or more was
    /// selected, or super-seeding started or stopped (a super-seed still hands out
    /// pieces). libtorrent's extension message, to peers that take it.
    fn sync_upload_only(&mut self) {
        let Some(picker) = self.picker.as_ref() else {
            return;
        };
        let now = picker.is_finished() && !self.super_seed;
        if now == self.told_upload_only {
            return;
        }
        self.told_upload_only = now;
        if !self.identity.wire().libtorrent_extensions {
            return;
        }
        for peer in self.peers.values() {
            if let Some(id) = peer
                .extensions
                .as_ref()
                .and_then(|e| e.id_of(names::UPLOAD_ONLY))
            {
                peer.send(Message::Extended {
                    id,
                    payload: Bytes::from(extended::encode_upload_only(now)),
                });
            }
        }
    }

    /// We no longer have `piece` (its file was deselected and dropped): peers that were
    /// told we had it hear otherwise (libtorrent's `lt_donthave`), so they stop asking.
    fn piece_dropped(&self, piece: u32) {
        if !self.identity.wire().libtorrent_extensions {
            return;
        }
        for peer in self.peers.values() {
            if let Some(id) = peer
                .extensions
                .as_ref()
                .and_then(|e| e.id_of(names::LT_DONTHAVE))
            {
                peer.send(Message::Extended {
                    id,
                    payload: Bytes::from(extended::encode_dont_have(piece)),
                });
            }
        }
    }

    /// Whether we take part in holepunching (BEP 55) for this torrent: the identity offers
    /// it, and the torrent is public (a private torrent's peers come only from its tracker,
    /// BEP 27).
    fn holepunching(&self) -> bool {
        self.identity.wire().libtorrent_extensions && !self.torrent.info.private
    }

    /// The id peer `key` takes `ut_holepunch` messages on, if it offered the extension.
    fn holepunch_id(&self, key: PeerKey) -> Option<u8> {
        self.peers
            .get(&key)
            .filter(|p| p.tx.is_some())?
            .extensions
            .as_ref()?
            .id_of(names::UT_HOLEPUNCH)
    }

    fn send_holepunch(&self, key: PeerKey, msg: HolepunchMessage) {
        if let (Some(id), Some(peer)) = (self.holepunch_id(key), self.peers.get(&key)) {
            peer.send(Message::Extended {
                id,
                payload: Bytes::from(msg.encode()),
            });
        }
    }

    /// Dialing `addr` failed. If a peer told us of it as holepunch-capable and is still
    /// connected, ask it to introduce us, once (libtorrent does this after a failed
    /// connect too). Both ends then dial each other over uTP at once.
    fn ask_introducer(&mut self, addr: SocketAddr) {
        if !self.holepunching() || self.shared.dialer.utp.is_none() || self.punched.contains(&addr)
        {
            return;
        }
        let Some(&via) = self.introducers.get(&addr) else {
            return;
        };
        if self.holepunch_id(via).is_none() {
            return;
        }
        if self.punched.len() >= MAX_UTP_HINTS {
            self.punched.clear();
        }
        self.punched.insert(addr);
        tracing::debug!(%addr, "asking a peer to introduce us (holepunch)");
        self.send_holepunch(via, HolepunchMessage::Rendezvous(addr));
    }

    /// Where another peer should dial `peer` for a holepunch: over uTP its connection's
    /// own address (the NAT's mapping of its uTP socket), else its listen port.
    fn holepunch_addr(peer: &Peer) -> SocketAddr {
        if peer.link.utp {
            peer.addr
        } else {
            Self::reachable_addr(peer).unwrap_or(peer.addr)
        }
    }

    fn on_holepunch(&mut self, key: PeerKey, msg: HolepunchMessage) {
        // BEP 55: ignore holepunch messages from peers that didn't offer the extension.
        if !self.holepunching() || self.holepunch_id(key).is_none() {
            return;
        }
        match msg {
            HolepunchMessage::Rendezvous(target) => {
                let target = SocketAddr::new(target.ip().to_canonical(), target.port());
                let found = self.peers.iter().find(|(_, p)| {
                    p.tx.is_some() && (p.addr == target || Self::reachable_addr(p) == Some(target))
                });
                let reply = match found {
                    None => Err(HolepunchError::NotConnected),
                    Some((&k, _)) if k == key => Err(HolepunchError::NoSelf),
                    Some((&k, _)) if self.holepunch_id(k).is_none() => {
                        Err(HolepunchError::NoSupport)
                    }
                    Some((&k, p)) => Ok((k, Self::holepunch_addr(p))),
                };
                match reply {
                    Ok((other, other_addr)) => {
                        let asker = Self::holepunch_addr(&self.peers[&key]);
                        tracing::debug!(from = %asker, to = %other_addr, "introducing peers (holepunch)");
                        self.send_holepunch(other, HolepunchMessage::Connect(asker));
                        self.send_holepunch(key, HolepunchMessage::Connect(other_addr));
                    }
                    Err(e) => self.send_holepunch(key, HolepunchMessage::Error(target, e)),
                }
            }
            HolepunchMessage::Connect(addr) => {
                let addr = SocketAddr::new(addr.ip().to_canonical(), addr.port());
                if self.shared.dialer.utp.is_none()
                    || self.peers.values().any(|p| p.addr == addr)
                    || self.shared.filter.is_blocked(addr.ip())
                {
                    return;
                }
                tracing::debug!(%addr, "dialing an introduced peer over uTP (holepunch)");
                // Now: the other end is dialing us at the same moment.
                self.utp_hints.insert(addr, peer::UtpHint::Only);
                self.retry_at.remove(&addr);
                self.known.insert(addr);
                self.candidates.push_front((addr, None));
                self.connect_more();
            }
            HolepunchMessage::Error(addr, e) => {
                tracing::debug!(%addr, error = ?e, "holepunch refused");
            }
        }
    }

    /// Peer `key` says it no longer has `piece`.
    fn peer_lost_piece(&mut self, key: PeerKey, piece: u32) {
        let (Some(picker), Some(peer)) = (self.picker.as_mut(), self.peers.get_mut(&key)) else {
            return;
        };
        if peer.is_seed {
            // A seed no more: count its pieces one by one from here.
            picker.peer_seed_removed();
            peer.is_seed = false;
            picker.peer_bitfield(&peer.has);
        }
        if peer.has.clear(piece) {
            picker.peer_lost(piece);
            if picker.is_wanted(piece) {
                peer.wanted = peer.wanted.saturating_sub(1);
            }
        }
        let gone: Vec<BlockRef> = peer
            .requests
            .iter()
            .filter(|b| b.piece == piece)
            .copied()
            .collect();
        for block in gone {
            peer.requests.remove(&block);
            peer.send(Message::Cancel(block));
            picker.request_failed(key, &block);
        }
        self.update_interest(key);
    }

    fn update_interest(&mut self, key: PeerKey) {
        let Some(peer) = self.peers.get_mut(&key) else {
            return;
        };
        let interested = peer.wanted > 0;
        if interested != peer.am_interested {
            peer.am_interested = interested;
            peer.send(if interested {
                Message::Interested
            } else {
                Message::NotInterested
            });
        }
        if interested && !peer.peer_choking {
            self.fill_requests(key);
        }
        // Two seeds have nothing to give each other, nor do two peers that both want
        // nothing more (BEP 21's partial seeds included).
        if let (Some(picker), Some(peer)) = (self.picker.as_ref(), self.peers.get(&key))
            && (picker.have().is_full() || picker.is_finished())
            && (peer.is_seed || peer.upload_only)
        {
            // Kept as a candidate: if files are selected again, we'll want it back.
            self.seed_addrs.insert(peer.addr);
            peer.close();
        }
    }

    fn fill_requests(&mut self, key: PeerKey) {
        let memory_limit = self.memory_limit();
        let (Some(picker), Some(peer)) = (self.picker.as_mut(), self.peers.get_mut(&key)) else {
            return;
        };
        let depth = request_depth(
            peer.rate_down,
            self.shared.config.request_queue,
            peer.extensions.as_ref().and_then(|e| e.request_queue),
        );
        if peer.peer_choking || !peer.am_interested || peer.requests.len() >= depth {
            return;
        }
        let want = depth - peer.requests.len();
        let max_new = new_piece_room(
            &self.layout,
            picker.pieces_in_progress(),
            self.shared.memory.available(),
            memory_limit,
        );
        let blocks = if self.reputation.on_parole(peer.addr.ip()) {
            picker.pick_exclusive(key, &peer.has, want, max_new)
        } else {
            picker.pick(key, &peer.has, want, max_new)
        };
        for block in blocks {
            peer.send(Message::Request(block));
            peer.requests.insert(block);
        }
        self.sync_memory();
    }

    /// Bring this torrent's share of the memory budget in line with its pieces in progress.
    fn sync_memory(&mut self) {
        let pieces = self.picker.as_ref().map_or(0, Picker::pieces_in_progress) as u64;
        let want = pieces * self.layout.piece_length();
        if want > self.memory_held {
            self.shared.memory.add(want - self.memory_held);
        } else {
            self.shared.memory.sub(self.memory_held - want);
        }
        self.memory_held = want;
    }

    /// Ask every unchoked peer for more blocks: memory freed up, or pieces became wanted.
    fn fill_all(&mut self) {
        let keys: Vec<PeerKey> = self.peers.keys().copied().collect();
        for key in keys {
            self.fill_requests(key);
        }
    }

    fn on_piece(&mut self, key: PeerKey, piece: u32, offset: u32, data: &[u8]) {
        let Some(peer) = self.peers.get_mut(&key) else {
            return;
        };
        let block = BlockRef {
            piece,
            offset,
            length: u32::try_from(data.len()).unwrap_or(u32::MAX),
        };
        if !peer.requests.remove(&block) {
            // Unsolicited, or a late reply after a cancel. Ignore it.
            return;
        }
        let peer_ip = peer.addr.ip();
        let entry = peer.entry;
        peer.downloaded += data.len() as u64;
        self.downloaded += data.len() as u64;
        if let Some(e) = self.entry_mut(entry) {
            e.downloaded += data.len() as u64;
        }
        let picker = self.picker.as_mut().expect("running");
        match picker.block_received(key, &block) {
            Received::Unwanted => {}
            Received::New {
                cancel,
                piece_complete,
            } => {
                let size = self.layout.piece_size(piece) as usize;
                let buf = self.buffers.entry(piece).or_insert_with(|| vec![0u8; size]);
                let start = offset as usize;
                buf[start..start + data.len()].copy_from_slice(data);
                let blocks = self.layout.blocks_in_piece(piece) as usize;
                let from = self
                    .block_from
                    .entry(piece)
                    .or_insert_with(|| vec![None; blocks]);
                from[(offset / BLOCK_LEN) as usize] = Some(peer_ip);
                for other in cancel {
                    if let Some(p) = self.peers.get_mut(&other)
                        && p.requests.remove(&block)
                    {
                        p.send(Message::Cancel(block));
                    }
                }
                if piece_complete {
                    let data = self.buffers.remove(&piece).expect("buffer exists");
                    let want_digests = self.reputation.wants_digests(piece);
                    self.verify_and_write(piece, data, want_digests);
                }
            }
        }
        self.fill_requests(key);
    }

    fn verify_and_write(&self, piece: u32, data: Vec<u8>, want_digests: bool) {
        let hashes = self.hashes.clone();
        let storage = self.storage.clone();
        let offset = self.layout.piece_offset(piece);
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let verdict = hashes.verify(piece, &data);
            let digests = (want_digests || verdict != Verdict::Valid).then(|| {
                data.chunks(BLOCK_LEN as usize)
                    .map(Sha1Hash::digest)
                    .collect()
            });
            let result = if verdict == Verdict::Valid {
                storage
                    .write(offset, &data)
                    .map(|()| verdict)
                    .map_err(|e| format!("write failed: {e}"))
            } else {
                Ok(verdict)
            };
            let _ = tx.blocking_send(ActorMsg::PieceWritten {
                piece,
                result,
                digests,
            });
        });
    }

    /// Settle who is to blame (or credit) for a verified or failed piece.
    fn judge_piece(&mut self, piece: u32, valid: bool, digests: Option<&[Sha1Hash]>) {
        let senders = self.block_from.remove(&piece).unwrap_or_default();
        let bans = if valid {
            self.reputation.piece_verified(piece, &senders, digests)
        } else {
            self.reputation
                .piece_failed(piece, &senders, digests.unwrap_or_default())
        };
        for v in bans {
            self.ban(v.ip, v.reason, v.why);
        }
    }

    /// Ban an address session-wide and drop its connections here. Other torrents drop
    /// theirs on their next tick.
    fn ban(&mut self, ip: IpAddr, reason: BanReason, why: &str) {
        let length = self.shared.filter.ban(ip, reason);
        tracing::warn!(%ip, why, minutes = length.as_secs() / 60, "banned peer");
        self.reputation.forget(ip);
        self.drop_blocked_peers();
    }

    /// Close connections to addresses that are banned or blocklisted.
    fn drop_blocked_peers(&mut self) {
        let blocked: Vec<PeerKey> = self
            .peers
            .iter()
            .filter(|(_, p)| self.shared.filter.is_blocked(p.addr.ip()))
            .map(|(k, _)| *k)
            .collect();
        for key in blocked {
            if let Some(p) = self.peers.get(&key) {
                p.close();
            }
            self.drop_peer(key);
        }
    }

    fn on_piece_written(
        &mut self,
        piece: u32,
        result: Result<Verdict, String>,
        digests: Option<&[Sha1Hash]>,
    ) {
        let prefilled = self.prefilled.remove(&piece);
        if self.picker.is_none() {
            return;
        }
        // Disk errors say nothing about the peers.
        if let Ok(verdict) = &result {
            let valid = *verdict == Verdict::Valid;
            if prefilled.is_some() && !valid {
                // The library's bytes may be the ones that differ, so nobody is blamed, and
                // the piece is downloaded whole next time.
                tracing::debug!(piece, "a bookend piece failed; downloading it whole");
                self.block_from.remove(&piece);
                self.no_prefill.insert(piece);
                self.refresh_hold(piece);
            } else {
                self.judge_piece(piece, valid, digests);
            }
            if *verdict == Verdict::Valid {
                self.webseed_pieces.remove(&piece);
            } else {
                self.webseed_piece_failed(piece);
            }
        } else {
            self.block_from.remove(&piece);
            self.webseed_pieces.remove(&piece);
        }
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        match result {
            Ok(Verdict::Valid) => {
                // An offered copy may have landed first.
                let fresh = !picker.have().get(piece);
                picker.piece_verified(piece, true);
                if fresh {
                    let origin = prefilled.map_or(PieceOrigin::Downloaded, |from_disk| {
                        PieceOrigin::Bookend { from_disk }
                    });
                    self.piece_gained(piece, origin);
                }
            }
            Ok(Verdict::Invalid | Verdict::Unknown) => {
                tracing::warn!(piece, "piece failed verification");
                picker.piece_verified(piece, false);
            }
            Err(e) => {
                tracing::error!(piece, error = %e, "disk error");
                picker.piece_verified(piece, false);
                self.fail(e);
            }
        }
        // The piece's buffer is gone, which may let peers start new pieces.
        self.sync_memory();
        self.fill_all();
        self.publish_stats();
    }

    /// Tell peers and the session about a piece just had, and note when the download
    /// finishes.
    fn piece_gained(&mut self, piece: u32, origin: PieceOrigin) {
        for peer in self.peers.values_mut() {
            if peer.has.get(piece) || peer.is_seed {
                peer.wanted = peer.wanted.saturating_sub(1);
            } else {
                peer.send(Message::Have(piece));
            }
        }
        let keys: Vec<PeerKey> = self.peers.keys().copied().collect();
        for key in keys {
            self.update_interest(key);
        }
        let _ = self.shared.pieces.send(PieceEvent {
            torrent: self.info_hashes,
            piece,
            origin,
        });
        let completed = self
            .picker
            .as_mut()
            .map(Picker::take_completed_files)
            .unwrap_or_default();
        if !completed.is_empty() {
            // Written, but maybe only to the OS's cache: make the files whole on disk before
            // anyone moves them.
            let _ = self.storage.sync();
        }
        for file in completed {
            let _ = self.shared.files.send(FileEvent {
                torrent: self.info_hashes,
                file,
            });
        }
        if !self.finished && self.picker.as_ref().is_some_and(Picker::is_finished) {
            self.finished = true;
            let _ = self.storage.sync();
            tracing::info!(torrent = %self.info_hashes.id_string(), "download complete");
            let now = Instant::now();
            for e in &mut self.entries {
                e.announcer.completed(now);
            }
            self.announce();
        }
        self.sync_upload_only();
    }

    // ----- the content library ------------------------------------------------------------

    /// Whether `piece` is being handled outside the picker (offered, adopted or prefilled).
    fn busy_elsewhere(&self, piece: u32) -> bool {
        self.offered.contains(&piece)
            || self.adopting.contains(&piece)
            || self.prefilling.contains(&piece)
    }

    fn offer_piece(&mut self, piece: u32, data: Vec<u8>, reply: oneshot::Sender<Offered>) {
        let needed = self.error.is_none()
            && piece < self.layout.piece_count()
            && data.len() == self.layout.piece_size(piece) as usize
            && self.hashes.can_verify(piece)
            && !self.busy_elsewhere(piece)
            && self
                .picker
                .as_ref()
                .is_some_and(|p| p.is_wanted(piece) && !p.in_progress(piece));
        if !needed {
            let _ = reply.send(Offered::NotNeeded);
            return;
        }
        self.offered.insert(piece);
        if let Some(picker) = self.picker.as_mut() {
            picker.set_held(piece, true);
        }
        let hashes = self.hashes.clone();
        let storage = self.storage.clone();
        let offset = self.layout.piece_offset(piece);
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let verdict = hashes.verify(piece, &data);
            let result = if verdict == Verdict::Valid {
                storage
                    .write(offset, &data)
                    .map(|()| verdict)
                    .map_err(|e| format!("write failed: {e}"))
            } else {
                Ok(verdict)
            };
            let _ = tx.blocking_send(ActorMsg::OfferWritten {
                piece,
                result,
                reply,
            });
        });
    }

    fn on_offer_written(
        &mut self,
        piece: u32,
        result: Result<Verdict, String>,
        reply: oneshot::Sender<Offered>,
    ) {
        self.offered.remove(&piece);
        self.refresh_hold(piece);
        let Some(picker) = self.picker.as_mut() else {
            let _ = reply.send(Offered::NotNeeded);
            return;
        };
        let answer = match result {
            Ok(Verdict::Valid) => {
                if !picker.have().get(piece) {
                    picker.mark_have(piece);
                    self.piece_gained(piece, PieceOrigin::Offered);
                    self.publish_stats();
                }
                Offered::Taken
            }
            Ok(_) => Offered::Mismatch,
            Err(e) => {
                tracing::error!(piece, error = %e, "disk error");
                self.fail(e.clone());
                Offered::Failed(e)
            }
        };
        let _ = reply.send(answer);
    }

    fn adopt_file(
        &mut self,
        file: usize,
        from: PathBuf,
        mode: AdoptMode,
        reply: oneshot::Sender<Result<Adoption, AdoptRefused>>,
    ) {
        let files = self.layout.files();
        let wanted = self
            .file_priorities
            .as_ref()
            .is_none_or(|p| p.get(file).copied().unwrap_or(1) > 0);
        let usable = self.error.is_none()
            && file < files.len()
            && !files[file].pad
            && files[file].length > 0
            && wanted
            && !self.moving.contains(&file)
            && self.storage.placements().get(file) == Some(&Placement::Save);
        let pieces: Vec<u32> = if usable {
            self.layout.file_pieces(file).collect()
        } else {
            Vec::new()
        };
        let untouched = usable
            && self.picker.as_ref().is_some_and(|picker| {
                pieces.iter().all(|&p| {
                    !picker.have().get(p) && !picker.in_progress(p) && !self.busy_elsewhere(p)
                })
            });
        if !untouched {
            let _ = reply.send(Err(AdoptRefused::InUse));
            return;
        }
        let picker = self.picker.as_mut().expect("checked above");
        for &p in &pieces {
            self.adopting.insert(p);
            picker.set_held(p, true);
        }
        let storage = self.storage.clone();
        let hashes = self.hashes.clone();
        let layout = self.layout.clone();
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = storage
                .adopt(file, &from, mode)
                .map_err(|e| e.to_string())
                .map(|how| {
                    (
                        how,
                        verify_pieces(storage.as_ref(), &hashes, &layout, &pieces),
                    )
                });
            let _ = tx.blocking_send(ActorMsg::FileAdopted {
                file,
                result,
                reply,
            });
        });
    }

    fn on_file_adopted(
        &mut self,
        file: usize,
        result: Result<(Adopted, Vec<u32>), String>,
        reply: oneshot::Sender<Result<Adoption, AdoptRefused>>,
    ) {
        let pieces: Vec<u32> = self.layout.file_pieces(file).collect();
        for &p in &pieces {
            self.adopting.remove(&p);
            self.refresh_hold(p);
        }
        if self.picker.is_none() {
            let _ = reply.send(Err(AdoptRefused::InUse));
            return;
        }
        let answer = match result {
            Ok((how, valid)) => {
                let mut gained = Vec::new();
                for p in valid {
                    let picker = self.picker.as_mut().expect("running");
                    if !picker.have().get(p) {
                        picker.mark_have(p);
                        gained.push(p);
                        self.piece_gained(p, PieceOrigin::Adopted);
                    }
                }
                if let Some(r) = self.reused.get_mut(file) {
                    *r = true;
                }
                self.set_bookends();
                tracing::info!(
                    torrent = %self.info_hashes.id_string(),
                    file,
                    ?how,
                    pieces = gained.len(),
                    "adopted a file from the library"
                );
                self.publish_stats();
                Ok(Adoption { how, gained })
            }
            Err(e) => Err(AdoptRefused::Io(e)),
        };
        let _ = reply.send(answer);
    }

    /// Work out the bookend pieces again (the reused files changed), and hold them back
    /// from peers until their library blocks are read.
    fn set_bookends(&mut self) {
        self.bookends = self.find_bookends();
        self.bookend_pieces = self.bookends.iter().map(|(p, _)| *p).collect();
        self.apply_holds();
    }

    /// Bookend pieces: pieces touching a reused file and some file that isn't, with the
    /// blocks lying wholly in reused files (or pads).
    fn find_bookends(&self) -> Vec<(u32, Vec<u32>)> {
        let files = self.layout.files();
        let trusted = |f: usize| files[f].pad || files[f].length == 0 || self.reused[f];
        let mut pieces: Vec<u32> = (0..files.len())
            .filter(|&f| self.reused[f] && !files[f].pad)
            .flat_map(|f| self.layout.file_pieces(f))
            .collect();
        pieces.sort_unstable();
        pieces.dedup();
        let mut out = Vec::new();
        for piece in pieces {
            let size = self.layout.piece_size(piece) as usize;
            let segments = self.layout.segments(self.layout.piece_offset(piece), size);
            if segments.iter().all(|s| trusted(s.file)) {
                continue;
            }
            let blocks: Vec<u32> = (0..self.layout.blocks_in_piece(piece))
                .filter(|&b| {
                    let start = (b * BLOCK_LEN) as usize;
                    let end = start + self.layout.block_size(piece, b) as usize;
                    segments
                        .iter()
                        .filter(|s| s.buf_offset < end && s.buf_offset + s.len > start)
                        .all(|s| trusted(s.file))
                })
                .collect();
            if !blocks.is_empty() {
                out.push((piece, blocks));
            }
        }
        out
    }

    /// Start reading bookend pieces' library blocks, a few at a time, for pieces some peer
    /// (or web seed) can supply the rest of.
    fn plan_prefills(&mut self) {
        if self.bookends.is_empty() || !self.running() {
            return;
        }
        let Some(picker) = self.picker.as_ref() else {
            return;
        };
        let open = self.prefilled.len() + self.prefilling.len();
        let mut room = MAX_PREFILLED.saturating_sub(open);
        let mut memory = self.shared.memory.available();
        let mut picks = Vec::new();
        for (piece, blocks) in &self.bookends {
            if room == 0 {
                break;
            }
            let p = *piece;
            if !picker.is_wanted(p)
                || picker.in_progress(p)
                || !self.hashes.can_verify(p)
                || self.no_prefill.contains(&p)
                || self.busy_elsewhere(p)
                || (picker.availability(p) == 0 && self.webseeds.is_empty())
            {
                continue;
            }
            let size = u64::from(self.layout.piece_size(p));
            if memory < size {
                break;
            }
            memory -= size;
            room -= 1;
            picks.push((p, blocks.clone()));
        }
        for (piece, blocks) in picks {
            self.prefilling.insert(piece);
            let storage = self.storage.clone();
            let layout = self.layout.clone();
            let tx = self.tx.clone();
            tokio::task::spawn_blocking(move || {
                let base = layout.piece_offset(piece);
                let result = blocks
                    .into_iter()
                    .map(|b| {
                        let mut buf = vec![0u8; layout.block_size(piece, b) as usize];
                        storage
                            .read(base + u64::from(b * BLOCK_LEN), &mut buf)
                            .map(|()| (b, buf))
                    })
                    .collect::<std::io::Result<Vec<_>>>()
                    .map_err(|e| e.to_string());
                let _ = tx.blocking_send(ActorMsg::Prefilled { piece, result });
            });
        }
    }

    fn on_prefilled(&mut self, piece: u32, result: Result<Vec<(u32, Vec<u8>)>, String>) {
        self.prefilling.remove(&piece);
        let blocks = match result {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!(piece, error = %e, "could not read a bookend's library bytes");
                self.no_prefill.insert(piece);
                self.refresh_hold(piece);
                return;
            }
        };
        if self.offered.contains(&piece) || self.adopting.contains(&piece) {
            return;
        }
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        let indices: Vec<u32> = blocks.iter().map(|(b, _)| *b).collect();
        if !picker.is_wanted(piece) || picker.in_progress(piece) {
            return;
        }
        let complete = picker.prefill(piece, &indices);
        let mut buf = vec![0u8; self.layout.piece_size(piece) as usize];
        let mut bytes = 0u32;
        for (b, data) in blocks {
            let at = (b * BLOCK_LEN) as usize;
            buf[at..at + data.len()].copy_from_slice(&data);
            bytes += u32::try_from(data.len()).expect("a block fits u32");
        }
        self.prefilled.insert(piece, bytes);
        if complete {
            self.verify_and_write(piece, buf, false);
        } else {
            self.buffers.insert(piece, buf);
        }
        self.sync_memory();
        self.fill_all();
    }

    // ----- uploading ----------------------------------------------------------------------

    fn on_request(&mut self, key: PeerKey, block: BlockRef) -> Result<(), String> {
        let n = self.layout.piece_count();
        let picker = self.picker.as_ref().expect("running");
        let peer = self.peers.get_mut(&key).expect("peer exists");
        if block.piece >= n
            || block.length == 0
            || block.length > MAX_BLOCK_LEN
            || u64::from(block.offset) + u64::from(block.length)
                > u64::from(self.layout.piece_size(block.piece))
        {
            return Err(format!("invalid request {block:?}"));
        }
        // A super-seed serves only the pieces it showed this peer.
        let hidden =
            self.super_seed && picker.have().is_full() && !peer.offered.contains(&block.piece);
        if peer.am_choking
            || hidden
            || !picker.have().get(block.piece)
            || peer.upload_queue.len() + peer.reading >= peer.our_reqq as usize
        {
            if peer.fast {
                peer.send(Message::Reject(block));
            }
            return Ok(());
        }
        peer.upload_queue.push_back(block);
        self.issue_reads(key);
        Ok(())
    }

    /// Start reading queued requests for peer `key` while its send budget allows,
    /// consecutive blocks of a piece in one read.
    fn issue_reads(&mut self, key: PeerKey) {
        let Some(peer) = self.peers.get_mut(&key) else {
            return;
        };
        let budget = send_budget(peer.rate_up);
        while peer.reading_bytes + peer.unsent < budget
            && let Some(first) = peer.upload_queue.pop_front()
        {
            let mut blocks = vec![first];
            let mut len = first.length;
            while let Some(&next) = peer.upload_queue.front()
                && next.piece == first.piece
                && next.offset == first.offset + len
                && len + next.length <= READ_RUN_MAX
            {
                peer.upload_queue.pop_front();
                blocks.push(next);
                len += next.length;
            }
            peer.reading += blocks.len();
            peer.reading_bytes += u64::from(len);
            let storage = self.storage.clone();
            let offset = self.layout.piece_offset(first.piece) + u64::from(first.offset);
            let tx = self.tx.clone();
            tokio::task::spawn_blocking(move || {
                let mut buf = vec![0u8; len as usize];
                let result = storage
                    .read(offset, &mut buf)
                    .map(|()| Bytes::from(buf))
                    .map_err(|e| format!("read failed: {e}"));
                let _ = tx.blocking_send(ActorMsg::BlocksRead {
                    peer: key,
                    blocks,
                    result,
                });
            });
        }
    }

    fn on_blocks_read(&mut self, key: PeerKey, blocks: &[BlockRef], result: Result<Bytes, String>) {
        let Some(peer) = self.peers.get_mut(&key) else {
            return;
        };
        let total: u64 = blocks.iter().map(|b| u64::from(b.length)).sum();
        peer.reading = peer.reading.saturating_sub(blocks.len());
        peer.reading_bytes = peer.reading_bytes.saturating_sub(total);
        let data = match result {
            Ok(data) => data,
            Err(e) => {
                tracing::error!(error = %e, "disk read error");
                if peer.fast {
                    for &b in blocks {
                        peer.send(Message::Reject(b));
                    }
                }
                self.fail(e);
                return;
            }
        };
        let first = blocks.first().map_or(0, |b| b.offset);
        let mut sent = 0u64;
        for &block in blocks {
            // Cancelled, or choked since it was asked for (its reject went with the choke
            // if it was still queued; this one was already being read).
            if peer.cancelled.remove(&block) || peer.am_choking {
                if peer.fast {
                    peer.send(Message::Reject(block));
                }
                continue;
            }
            let start = (block.offset - first) as usize;
            peer.send(Message::Piece {
                piece: block.piece,
                offset: block.offset,
                data: data.slice(start..start + block.length as usize),
            });
            sent += u64::from(block.length);
        }
        if peer.reading == 0 {
            peer.cancelled.clear();
        }
        peer.unsent += sent;
        peer.uploaded += sent;
        let entry = peer.entry;
        self.uploaded += sent;
        if let Some(e) = self.entry_mut(entry) {
            e.uploaded += sent;
        }
        self.issue_reads(key);
    }

    /// Choke a peer. Its queued requests are dropped: with the Fast extension each is
    /// rejected (BEP 6); without it, choking cancels them.
    fn choke(peer: &mut Peer) {
        peer.am_choking = true;
        peer.send(Message::Choke);
        let queued = std::mem::take(&mut peer.upload_queue);
        if peer.fast {
            for block in queued {
                peer.send(Message::Reject(block));
            }
        }
    }

    // ----- choking ------------------------------------------------------------------------

    fn unchoked_count(&self) -> usize {
        self.peers.values().filter(|p| !p.am_choking).count()
    }

    /// Peers uploaded to at once: the fewest any entry allows, else the session's.
    fn upload_slots(&self) -> usize {
        self.entries
            .iter()
            .filter_map(|e| e.limits.upload_slots)
            .min()
            .unwrap_or(self.shared.config.upload_slots)
    }

    // ----- super-seeding (BEP 16) -----------------------------------------------------------

    fn super_seeding(&self) -> bool {
        self.super_seed && self.picker.as_ref().is_some_and(|p| p.have().is_full())
    }

    fn set_super_seed(&mut self, on: bool) {
        if self.super_seed == on {
            return;
        }
        let was = self.super_seeding();
        self.super_seed = on;
        if was && !on {
            // Show every peer everything it was kept from.
            let n = self.layout.piece_count();
            for peer in self
                .peers
                .values_mut()
                .filter(|p| p.tx.is_some() && !p.is_seed)
            {
                for piece in (0..n).filter(|&p| !peer.has.get(p) && !peer.shown.contains(&p)) {
                    peer.send(Message::Have(piece));
                }
                peer.offered.clear();
                peer.shown.clear();
            }
        }
    }

    /// Super-seeding: show peer `key` pieces it lacks until it has [`SUPERSEED_OFFERS`] on
    /// offer, the rarest first, preferring pieces not on offer to anyone else.
    fn superseed_offer(&mut self, key: PeerKey) {
        use rand::seq::SliceRandom;
        if !self.super_seeding() {
            return;
        }
        let picker = self.picker.as_ref().expect("super-seeding has a picker");
        let elsewhere: HashSet<u32> = self
            .peers
            .iter()
            .filter(|(k, _)| **k != key)
            .flat_map(|(_, p)| p.offered.iter().copied())
            .collect();
        let Some(peer) = self.peers.get(&key) else {
            return;
        };
        let want = SUPERSEED_OFFERS.saturating_sub(peer.offered.len());
        if peer.tx.is_none() || peer.is_seed || want == 0 {
            return;
        }
        let mut candidates: Vec<(bool, u32, u32)> = (0..self.layout.piece_count())
            .filter(|&p| !peer.has.get(p) && !peer.shown.contains(&p))
            .map(|p| (elsewhere.contains(&p), picker.availability(p), p))
            .collect();
        candidates.shuffle(&mut rand::rng());
        candidates.sort_by_key(|&(on_offer, availability, _)| (on_offer, availability));
        let peer = self.peers.get_mut(&key).expect("checked above");
        for (_, _, piece) in candidates.into_iter().take(want) {
            peer.offered.push(piece);
            peer.shown.insert(piece);
            peer.send(Message::Have(piece));
        }
    }

    /// Super-seeding: peer `key` now has `piece`. If it was one shown to it, it is shown
    /// another (libtorrent's default, non-strict super-seeding: strict mode waits until a
    /// third peer has the piece, which stalls a swarm of one).
    fn superseed_done(&mut self, key: PeerKey, piece: u32) {
        if !self.super_seeding() {
            return;
        }
        let Some(peer) = self.peers.get_mut(&key) else {
            return;
        };
        if let Some(i) = peer.offered.iter().position(|&p| p == piece) {
            peer.offered.swap_remove(i);
            self.superseed_offer(key);
        }
    }

    /// Unchoke a newly interested peer straight away if a slot is free, instead of waiting
    /// for the next choke round.
    fn maybe_unchoke(&mut self, key: PeerKey) {
        let slots = self.upload_slots();
        if self.unchoked_count() < slots
            && let Some(peer) = self.peers.get_mut(&key)
            && peer.am_choking
        {
            peer.am_choking = false;
            peer.send(Message::Unchoke);
        }
    }

    /// Tit-for-tat: unchoke the interested peers that give us the most (or, when seeding,
    /// that take the most), plus one optimistic unchoke that rotates.
    fn run_choker(&mut self) {
        let slots = self.upload_slots().max(1);
        let seeding = self.picker.as_ref().is_some_and(|p| p.have().is_full());
        let mut ranked: Vec<(PeerKey, u64)> = self
            .peers
            .iter()
            .filter(|(_, p)| p.peer_interested && p.tx.is_some())
            .map(|(k, p)| (*k, if seeding { p.rate_up } else { p.rate_down }))
            .collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let regular: HashSet<PeerKey> = ranked
            .iter()
            .take(slots.saturating_sub(1))
            .map(|(k, _)| *k)
            .collect();
        let rotate = self.ticks.is_multiple_of(CHOKE_EVERY * OPTIMISTIC_EVERY);
        let mut optimistic = self
            .peers
            .iter()
            .find(|(k, p)| p.optimistic && !regular.contains(k) && p.peer_interested)
            .map(|(k, _)| *k);
        if rotate || optimistic.is_none() {
            let choked: Vec<PeerKey> = ranked
                .iter()
                .map(|(k, _)| *k)
                .filter(|k| !regular.contains(k))
                .collect();
            optimistic = if choked.is_empty() {
                None
            } else {
                Some(choked[rand::random_range(0..choked.len())])
            };
        }
        for (key, peer) in &mut self.peers {
            let unchoke = regular.contains(key) || Some(*key) == optimistic;
            peer.optimistic = Some(*key) == optimistic;
            if unchoke && peer.am_choking {
                peer.am_choking = false;
                peer.send(Message::Unchoke);
            } else if !unchoke && !peer.am_choking {
                Self::choke(peer);
            }
        }
    }

    // ----- periodic work -------------------------------------------------------------------

    fn on_tick(&mut self) {
        self.ticks = self.ticks.wrapping_add(1);
        // Finishing, more files selected, super-seeding on or off.
        self.sync_upload_only();
        for peer in self.peers.values_mut() {
            peer.rate_down = peer.downloaded - peer.last_down;
            peer.rate_up = peer.uploaded - peer.last_up;
            peer.last_down = peer.downloaded;
            peer.last_up = peer.uploaded;
        }
        if self.ticks.is_multiple_of(CHOKE_EVERY) {
            self.run_choker();
        }
        let pex_every = u32::try_from(self.shared.config.pex_interval.as_secs())
            .unwrap_or(u32::MAX)
            .max(1);
        if self.ticks.is_multiple_of(pex_every) {
            self.send_pex();
        }
        if !self.layer_fetches.is_empty() {
            self.request_layers();
        }
        // Another torrent may have banned an address, or freed memory.
        self.drop_blocked_peers();
        self.plan_prefills();
        self.fill_all();
        self.announce();
        self.connect_more();
        self.publish_stats();
        self.last_downloaded = self.downloaded;
        self.last_uploaded = self.uploaded;
        for e in &mut self.entries {
            e.last_downloaded = e.downloaded;
            e.last_uploaded = e.uploaded;
        }
    }

    /// Stops the torrent after a storage error: every further transfer would be wasted (or,
    /// for uploads, fail too). It stays stopped until the user resumes it.
    fn fail(&mut self, error: String) {
        if self.error.is_some() {
            return;
        }
        self.error = Some(error);
        self.send_stopped();
        self.shutdown_peers();
        self.publish_stats();
    }

    fn peer_infos(&self) -> Vec<PeerInfo> {
        let total = f64::from(self.layout.piece_count().max(1));
        let mut out: Vec<PeerInfo> = self
            .peers
            .values()
            .filter(|p| p.tx.is_some())
            .map(|p| {
                let client = p
                    .extensions
                    .as_ref()
                    .and_then(|e| e.client.clone())
                    .or_else(|| p.peer_id.map(|id| id.client()))
                    .unwrap_or_default();
                let have = if p.is_seed {
                    total
                } else {
                    f64::from(p.has.count_ones())
                };
                PeerInfo {
                    addr: p.addr,
                    client,
                    progress: have / total,
                    download_rate: p.rate_down,
                    upload_rate: p.rate_up,
                    downloaded: p.downloaded,
                    uploaded: p.uploaded,
                    outbound: p.outbound,
                    am_choking: p.am_choking,
                    am_interested: p.am_interested,
                    peer_choking: p.peer_choking,
                    peer_interested: p.peer_interested,
                    on_parole: self.reputation.on_parole(p.addr.ip()),
                    utp: p.link.utp,
                    encrypted: p.link.encrypted,
                    entry: p.entry,
                }
            })
            .collect();
        out.sort_by(|a, b| {
            b.download_rate
                .cmp(&a.download_rate)
                .then(a.addr.cmp(&b.addr))
        });
        out
    }

    fn publish_stats(&self) {
        let (state, pieces_have) = match &self.picker {
            None if self.error.is_some() => (TorrentState::Error, 0),
            None => (TorrentState::Checking, 0),
            Some(p) => {
                let state = if self.error.is_some() {
                    TorrentState::Error
                } else if !self.running() {
                    TorrentState::Paused
                } else if p.is_finished() {
                    TorrentState::Seeding
                } else {
                    TorrentState::Downloading
                };
                (state, p.have().count_ones())
            }
        };
        let ready = self.peers.values().filter(|p| p.tx.is_some());
        let (peers, seeds) = ready.fold((0, 0), |(n, s), p| (n + 1, s + u32::from(p.is_seed)));
        let entries = self
            .entries
            .iter()
            .map(|e| {
                let mine = self
                    .peers
                    .values()
                    .filter(|p| p.tx.is_some() && p.entry == e.key);
                let (peers, seeds) =
                    mine.fold((0, 0), |(n, s), p| (n + 1, s + u32::from(p.is_seed)));
                EntryStats {
                    key: e.key,
                    paused: e.paused,
                    downloaded: e.downloaded,
                    uploaded: e.uploaded,
                    download_rate: e.downloaded - e.last_downloaded,
                    upload_rate: e.uploaded - e.last_uploaded,
                    peers,
                    seeds,
                }
            })
            .collect();
        let (wanted_total, wanted_have) = self
            .picker
            .as_ref()
            .map_or((self.layout.piece_count(), 0), Picker::wanted);
        let stats = TorrentStats {
            state,
            pieces_have: self
                .picker
                .as_ref()
                .map_or(pieces_have, |p| p.have().count_ones()),
            pieces_total: self.layout.piece_count(),
            wanted_total,
            wanted_have,
            finished: self.picker.is_some() && wanted_have == wanted_total,
            downloaded: self.downloaded,
            uploaded: self.uploaded,
            download_rate: self.downloaded - self.last_downloaded,
            upload_rate: self.uploaded - self.last_uploaded,
            peers,
            seeds,
            error: self.error.clone(),
            entries,
        };
        self.stats.send_if_modified(|old| {
            if *old == stats {
                false
            } else {
                *old = stats;
                true
            }
        });
    }
}

/// How many new pieces a torrent may start, given the session's free memory and the
/// torrent's own limit. A torrent with nothing in progress may always start one, so no
/// torrent starves while others hold the budget.
/// Our encryption policy, as an announce reports it.
pub(crate) fn announce_crypto(
    policy: apitorrent_mse::Policy,
) -> apitorrent_tracker::AnnounceCrypto {
    use apitorrent_mse::Policy;
    use apitorrent_tracker::AnnounceCrypto;
    match policy {
        Policy::Disabled => AnnounceCrypto::Disabled,
        Policy::Enabled(_) => AnnounceCrypto::Enabled,
        Policy::Required => AnnounceCrypto::Required,
    }
}

/// How many piece bytes may be read or waiting to go out for a peer uploading at `rate`
/// bytes a second: about a second's worth, within [`SEND_BUDGET_MIN`] and
/// [`SEND_BUDGET_MAX`].
fn send_budget(rate: u64) -> u64 {
    rate.clamp(SEND_BUDGET_MIN, SEND_BUDGET_MAX)
}

/// How many requests to keep outstanding to a peer sending `rate` bytes a second: enough
/// for [`REQUEST_QUEUE_SECS`] of it, at least `floor`, and no more than the peer takes
/// (`reqq`, or [`DEFAULT_PEER_REQQ`] when it doesn't say) or [`MAX_REQUEST_QUEUE`].
fn request_depth(rate: u64, floor: usize, peer_reqq: Option<u32>) -> usize {
    let cap = peer_reqq
        .map_or(DEFAULT_PEER_REQQ, |q| q as usize)
        .clamp(1, MAX_REQUEST_QUEUE);
    let for_rate =
        usize::try_from(rate * REQUEST_QUEUE_SECS / u64::from(BLOCK_LEN)).unwrap_or(usize::MAX);
    // Not `clamp`: the floor may be above the peer's cap, and the cap wins.
    for_rate.max(floor).min(cap)
}

fn new_piece_room(
    layout: &Layout,
    in_progress: usize,
    session_available: u64,
    torrent_limit: Option<u64>,
) -> usize {
    let piece = layout.piece_length().max(1);
    let session = session_available / piece;
    let torrent = torrent_limit.map_or(u64::MAX, |limit| {
        limit.saturating_sub(in_progress as u64 * piece) / piece
    });
    let room = usize::try_from(session.min(torrent)).unwrap_or(usize::MAX);
    if in_progress == 0 { room.max(1) } else { room }
}

/// Each piece gets the highest priority of the files it overlaps. Pad files don't count.
fn piece_priorities(layout: &Layout, file_prios: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; layout.piece_count() as usize];
    for (i, f) in layout.files().iter().enumerate() {
        if f.pad {
            continue;
        }
        let prio = file_prios[i];
        for p in layout.file_pieces(i) {
            out[p as usize] = out[p as usize].max(prio);
        }
    }
    out
}

/// Record the peer's full set of pieces (from a bitfield or have-all).
fn set_peer_has(picker: &mut Picker, peer: &mut Peer, has: Bitfield) {
    if peer.is_seed {
        picker.peer_seed_removed();
    } else {
        picker.peer_bitfield_lost(&peer.has);
    }
    peer.is_seed = has.is_full();
    if peer.is_seed {
        picker.peer_seed_added();
    } else {
        picker.peer_bitfield(&has);
    }
    peer.wanted =
        u32::try_from(has.iter_ones().filter(|&p| picker.is_wanted(p)).count()).expect("fits");
    peer.has = has;
}

/// Drop pieces from resume data whose files are missing or too short. That catches files
/// deleted or truncated while we weren't running, without hashing anything.
fn validate_resume(storage: &dyn Storage, layout: &Layout, mut have: Bitfield) -> Bitfield {
    if have.len() != layout.piece_count() {
        return Bitfield::new(layout.piece_count());
    }
    let sizes = storage.file_sizes();
    let placements = storage.placements();
    let pieces: Vec<u32> = have.iter_ones().collect();
    for piece in pieces {
        let ok = layout
            .segments(
                layout.piece_offset(piece),
                layout.piece_size(piece) as usize,
            )
            .iter()
            .all(|s| {
                s.pad
                    || if placements[s.file] == Placement::Parts {
                        storage.in_parts(piece)
                    } else {
                        sizes[s.file].is_some_and(|len| len >= s.file_offset + s.len as u64)
                    }
            });
        if !ok {
            have.clear(piece);
        }
    }
    have
}

/// Hash whatever is already on disk and return the pieces that verify.
/// The pieces among `pieces` that read back and verify.
fn verify_pieces(
    storage: &dyn Storage,
    hashes: &PieceHashes,
    layout: &Layout,
    pieces: &[u32],
) -> Vec<u32> {
    let mut buf = Vec::new();
    pieces
        .iter()
        .copied()
        .filter(|&p| {
            buf.resize(layout.piece_size(p) as usize, 0);
            storage.read(layout.piece_offset(p), &mut buf).is_ok()
                && hashes.verify(p, &buf) == Verdict::Valid
        })
        .collect()
}

fn check_existing(storage: &dyn Storage, hashes: &PieceHashes, layout: &Layout) -> Bitfield {
    let mut have = Bitfield::new(layout.piece_count());
    let sizes = storage.file_sizes();
    let placements = storage.placements();
    let files = layout.files();
    let mut buf = Vec::new();
    for piece in 0..layout.piece_count() {
        let offset = layout.piece_offset(piece);
        let size = layout.piece_size(piece) as usize;
        // Skip the read when any file under the piece is missing or too short.
        let present = layout.segments(offset, size).iter().all(|s| {
            s.pad
                || files[s.file].length == 0
                || if placements[s.file] == Placement::Parts {
                    storage.in_parts(piece)
                } else {
                    sizes[s.file].is_some_and(|len| len >= s.file_offset + s.len as u64)
                }
        });
        if !present {
            continue;
        }
        buf.resize(size, 0);
        if storage.read(offset, &mut buf).is_ok() && hashes.verify(piece, &buf) == Verdict::Valid {
            have.set(piece);
        }
    }
    have
}

#[cfg(test)]
mod tests {
    use super::*;
    use apitorrent_metainfo::{FileSource, TorrentBuilder, Version};

    fn layout(piece_kib: u64) -> Layout {
        let bytes = TorrentBuilder::new("t", Version::V1)
            .piece_length(piece_kib * 1024)
            .file(FileSource::from_bytes(&[], vec![0; 1 << 20]))
            .build()
            .unwrap();
        Layout::from_info(&Torrent::from_bytes(&bytes).unwrap().info)
    }

    #[test]
    fn send_budget_follows_the_rate() {
        assert_eq!(send_budget(0), SEND_BUDGET_MIN);
        assert_eq!(send_budget(2 * 1024 * 1024), 2 * 1024 * 1024);
        assert_eq!(send_budget(u64::MAX), SEND_BUDGET_MAX);
    }

    #[test]
    fn request_depth_follows_the_rate_within_the_peers_queue() {
        let block = u64::from(BLOCK_LEN);
        // Slow or new peers get the floor.
        assert_eq!(request_depth(0, 64, Some(2000)), 64);
        // Three seconds of a 10 MiB/s peer: 1920 blocks, capped at 500.
        assert_eq!(request_depth(640 * block, 64, Some(2000)), 500);
        assert_eq!(request_depth(100 * block, 64, Some(2000)), 300);
        // Never more than the peer takes, even below the floor.
        assert_eq!(request_depth(100 * block, 64, Some(250)), 250);
        assert_eq!(request_depth(0, 64, Some(16)), 16);
        assert_eq!(request_depth(100 * block, 64, None), DEFAULT_PEER_REQQ);
        assert_eq!(request_depth(0, 64, Some(0)), 1);
    }

    #[test]
    fn new_piece_room_respects_both_budgets() {
        let l = layout(64);
        let piece = 64 * 1024;
        assert_eq!(new_piece_room(&l, 0, 10 * piece, None), 10);
        assert_eq!(new_piece_room(&l, 3, 10 * piece, Some(5 * piece)), 2);
        assert_eq!(new_piece_room(&l, 6, 10 * piece, Some(5 * piece)), 0);
        assert_eq!(new_piece_room(&l, 2, piece - 1, None), 0);
        assert_eq!(new_piece_room(&l, 0, 0, Some(0)), 1, "never starved");
    }
}

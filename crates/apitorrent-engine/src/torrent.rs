//! The per-torrent actor: owns the picker, the peer states and the piece buffers, and makes
//! every protocol decision for one torrent.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};

use apitorrent_metainfo::{InfoHashes, Sha1Hash, Torrent};
use apitorrent_wire::extended::{ExtensionHandshake, names};
use apitorrent_wire::{BlockRef, Decoder, Handshake, MAX_BLOCK_LEN, Message, PeerId};

use crate::announce::{Announcer, Job, TrackerStatus};
use crate::bitfield::Bitfield;
use crate::layout::Layout;
use crate::peer::{self, PeerCmd, PeerEvent};
use crate::picker::{PeerKey, Picker, Received};
use crate::session::Shared;
use crate::storage::{FileStorage, Placement, Storage, StoragePaths};
use crate::verify::{PieceHashes, Verdict};

const TICK: Duration = Duration::from_secs(1);
const CHOKE_EVERY: u32 = 10;
const OPTIMISTIC_EVERY: u32 = 3;
/// How long to wait before reconnecting to a peer whose connection ended normally.
const RECONNECT_AFTER: Duration = Duration::from_secs(10);
/// How long to wait before retrying a peer we failed to reach.
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// Most outstanding block reads we'll do for one peer.
const MAX_PEER_UPLOAD_QUEUE: u32 = 256;

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
    /// Payload bytes received and sent in this session.
    pub downloaded: u64,
    pub uploaded: u64,
    /// Bytes per second over the last tick.
    pub download_rate: u64,
    pub upload_rate: u64,
    pub peers: u32,
    pub seeds: u32,
    pub error: Option<String>,
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
    /// This torrent's own cap on piece data in memory, below the session's.
    pub memory_limit: Option<u64>,
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
            memory_limit: None,
        }
    }
}

pub(crate) enum Command {
    AddPeers(Vec<SocketAddr>),
    /// The pieces we have right now, for resume data.
    Have(oneshot::Sender<Bitfield>),
    Trackers(oneshot::Sender<Vec<TrackerStatus>>),
    /// One priority per file (0 = skip, up to [`crate::picker::MAX_PRIORITY`]).
    FilePriorities(Vec<u8>),
    SetUnwantedPolicy(UnwantedPolicy),
    /// Delete the held copies of these files.
    ReleaseHeld(Vec<usize>),
    Pause,
    Resume,
    Shutdown(oneshot::Sender<Bitfield>),
}

pub(crate) enum ActorMsg {
    Command(Command),
    Incoming {
        stream: TcpStream,
        handshake: Handshake,
        addr: SocketAddr,
    },
    Checked(Bitfield),
    PieceWritten {
        piece: u32,
        result: Result<Verdict, String>,
    },
    BlockRead {
        peer: PeerKey,
        block: BlockRef,
        result: Result<Bytes, String>,
    },
    Announced {
        job: Job,
        result: Result<apitorrent_tracker::AnnounceResponse, String>,
    },
    /// A batch of placement changes finished.
    Moved {
        files: Vec<usize>,
        result: Result<(), String>,
    },
}

/// Handle to a running torrent.
#[derive(Debug, Clone)]
pub struct TorrentHandle {
    pub(crate) info_hashes: InfoHashes,
    pub(crate) tx: mpsc::Sender<ActorMsg>,
    pub(crate) stats: watch::Receiver<TorrentStats>,
    pub(crate) storage: Arc<dyn Storage>,
}

impl TorrentHandle {
    pub fn info_hashes(&self) -> InfoHashes {
        self.info_hashes
    }

    pub fn stats(&self) -> TorrentStats {
        self.stats.borrow().clone()
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

    /// The status of each tracker.
    pub async fn trackers(&self) -> Vec<TrackerStatus> {
        let (tx, rx) = oneshot::channel();
        if self
            .tx
            .send(ActorMsg::Command(Command::Trackers(tx)))
            .await
            .is_err()
        {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    /// Set a download priority per file: 0 skips the file, higher values are fetched first.
    /// A piece shared by several files takes the highest priority among them.
    pub async fn set_file_priorities(&self, priorities: Vec<u8>) {
        let _ = self
            .tx
            .send(ActorMsg::Command(Command::FilePriorities(priorities)))
            .await;
    }

    /// Where each file's bytes are (for resume data). Waits for a move in progress.
    pub fn placements(&self) -> Vec<Placement> {
        self.storage.placements()
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

    /// Delete the torrent's held copies and part store, and its downloaded files too when
    /// `save_files` is set. Call it after [`TorrentHandle::shutdown`].
    pub async fn delete_data(&self, save_files: bool) -> std::io::Result<()> {
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || storage.delete(save_files))
            .await
            .map_err(std::io::Error::other)?
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
    tx: Option<mpsc::UnboundedSender<PeerCmd>>,
    peer_id: Option<PeerId>,
    fast: bool,
    extensions: Option<ExtensionHandshake>,
    has: Bitfield,
    is_seed: bool,
    /// How many pieces the peer has that we still want.
    wanted: u32,
    am_choking: bool,
    am_interested: bool,
    peer_choking: bool,
    peer_interested: bool,
    /// Our outstanding requests to the peer.
    requests: Vec<BlockRef>,
    /// Block reads in flight for the peer's requests.
    uploads: usize,
    downloaded: u64,
    uploaded: u64,
    rate_down: u64,
    rate_up: u64,
    last_down: u64,
    last_up: u64,
    optimistic: bool,
}

impl Peer {
    fn new(addr: SocketAddr, pieces: u32, outbound: bool) -> Self {
        Self {
            addr,
            outbound,
            tx: None,
            peer_id: None,
            fast: false,
            extensions: None,
            has: Bitfield::new(pieces),
            is_seed: false,
            wanted: 0,
            am_choking: true,
            am_interested: false,
            peer_choking: true,
            peer_interested: false,
            requests: Vec::new(),
            uploads: 0,
            downloaded: 0,
            uploaded: 0,
            rate_down: 0,
            rate_up: 0,
            last_down: 0,
            last_up: 0,
            optimistic: false,
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
    candidates: VecDeque<SocketAddr>,
    /// Candidates we shouldn't dial before the given time.
    retry_at: HashMap<SocketAddr, Instant>,
    /// Addresses we know to be seeds. They aren't dialled while we have every piece.
    seed_addrs: HashSet<SocketAddr>,
    known: HashSet<SocketAddr>,
    stats: watch::Sender<TorrentStats>,
    paused: bool,
    error: Option<String>,
    downloaded: u64,
    uploaded: u64,
    last_downloaded: u64,
    last_uploaded: u64,
    ticks: u32,
    max_message: u32,
    file_priorities: Option<Vec<u8>>,
    unwanted: UnwantedPolicy,
    /// Files whose placement change is in flight.
    moving: HashSet<usize>,
    memory_limit: Option<u64>,
    /// Bytes this torrent has counted against the session's memory budget.
    memory_held: u64,
    announcer: Announcer,
    /// Whether the download was already finished; finishing sends `completed` once.
    finished: bool,
}

impl TorrentActor {
    pub(crate) fn spawn(params: AddTorrent, shared: Arc<Shared>) -> (TorrentHandle, Vec<Sha1Hash>) {
        let torrent = params.torrent;
        let info = &torrent.info;
        let info_hashes = info.info_hashes();
        let layout = Layout::from_info(info);
        let relative: Vec<Vec<String>> = info.files.iter().map(|f| f.path.clone()).collect();
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
        let storage: Arc<dyn Storage> = Arc::new(FileStorage::new(
            &paths,
            layout.clone(),
            &relative,
            Some(&placements),
        ));
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
            downloaded: 0,
            uploaded: 0,
            download_rate: 0,
            upload_rate: 0,
            peers: 0,
            seeds: 0,
            error: None,
        };
        let (stats_tx, stats_rx) = watch::channel(initial);
        let mut keys = Vec::new();
        if let Some(v1) = info_hashes.get_v1() {
            keys.push(v1);
        }
        if let Some(v2) = info_hashes.get_v2() {
            keys.push(v2.truncated());
        }
        let max_message = Decoder::max_len_for(u64::from(layout.piece_count()));
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
            retry_at: HashMap::new(),
            seed_addrs: HashSet::new(),
            known: HashSet::new(),
            stats: stats_tx,
            paused: params.paused,
            error: None,
            downloaded: 0,
            uploaded: 0,
            last_downloaded: 0,
            last_uploaded: 0,
            ticks: 0,
            max_message,
            file_priorities: params.file_priorities,
            unwanted: params.unwanted,
            moving: HashSet::new(),
            memory_limit: params.memory_limit,
            memory_held: 0,
            announcer: Announcer::new(&torrent.trackers, &keys, Instant::now()),
            finished: false,
        };
        actor.add_candidates(params.peers);
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

    fn start(&mut self, have: Bitfield) {
        let seed = rand::random();
        let mut picker = Picker::new(self.layout.clone(), have, seed);
        if let Some(prios) = &self.file_priorities {
            picker.set_priorities(&piece_priorities(&self.layout, prios));
        }
        self.finished = picker.is_finished();
        self.picker = Some(picker);
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
        if let Some(picker) = self.picker.as_mut() {
            picker.set_priorities(&piece_priorities(&self.layout, prios));
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
        // Newly wanted pieces may need the seeds we let go once we had everything.
        if self.picker.as_ref().is_some_and(|p| !p.have().is_full()) {
            for addr in &self.seed_addrs {
                self.retry_at.remove(addr);
            }
        }
        self.connect_more();
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
        // A piece that finished while its file was moving may have been written to the
        // old place. Drop any piece whose part-store bytes aren't there.
        let placements = self.storage.placements();
        if let Some(picker) = self.picker.as_mut() {
            for &file in files {
                if placements[file] != Placement::Parts {
                    continue;
                }
                for piece in self.layout.file_pieces(file) {
                    if picker.have().get(piece) && !self.storage.in_parts(piece) {
                        picker.mark_missing(piece);
                    }
                }
            }
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
                        let _ = reply.send(have);
                        break;
                    }
                    self.handle(msg);
                }
                Some((key, event)) = self.peer_rx.recv() => self.handle_peer(key, event),
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
                    self.add_candidates(peers);
                    self.connect_more();
                }
                Command::Pause => {
                    self.paused = true;
                    self.send_stopped();
                    self.shutdown_peers();
                    self.publish_stats();
                }
                Command::Resume => {
                    // Resuming is also how the user retries after a disk error.
                    self.error = None;
                    self.paused = false;
                    self.announce();
                    self.connect_more();
                    self.publish_stats();
                }
                Command::Have(reply) => {
                    let have = self.picker.as_ref().map_or_else(
                        || Bitfield::new(self.layout.piece_count()),
                        |p| p.have().clone(),
                    );
                    let _ = reply.send(have);
                }
                Command::FilePriorities(prios) => self.set_file_priorities(&prios),
                Command::SetUnwantedPolicy(policy) => self.unwanted = policy,
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
                Command::Trackers(reply) => {
                    let _ = reply.send(self.announcer.status());
                }
                Command::Shutdown(_) => unreachable!("handled in run"),
            },
            ActorMsg::Incoming {
                stream,
                handshake,
                addr,
            } => self.accept(stream, handshake, addr),
            ActorMsg::Checked(have) => {
                self.start(have);
                self.publish_stats();
                self.connect_more();
            }
            ActorMsg::PieceWritten { piece, result } => self.on_piece_written(piece, result),
            ActorMsg::BlockRead {
                peer,
                block,
                result,
            } => self.on_block_read(peer, block, result),
            ActorMsg::Announced { job, result } => self.on_announced(&job, result),
            ActorMsg::Moved { files, result } => self.on_moved(&files, result),
        }
    }

    // ----- trackers -----------------------------------------------------------------------

    fn announce(&mut self) {
        if self.paused || self.picker.is_none() {
            return;
        }
        for job in self.announcer.due(Instant::now()) {
            self.spawn_announce(job, true);
        }
    }

    fn spawn_announce(&self, job: Job, report: bool) {
        let req = apitorrent_tracker::AnnounceRequest {
            info_hash: job.info_hash,
            peer_id: self.shared.peer_id,
            port: self.shared.listen_port(),
            uploaded: self.uploaded,
            downloaded: self.downloaded,
            left: self.bytes_left(),
            event: job.event,
            num_want: Some(if self.finished { 0 } else { 50 }),
            key: self.shared.announce_key,
            tracker_id: job.tracker_id.clone(),
        };
        let client = self.shared.tracker.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = client
                .announce(&job.url, &req)
                .await
                .map_err(|e| e.to_string());
            if report {
                let _ = tx.send(ActorMsg::Announced { job, result }).await;
            }
        });
    }

    /// Best-effort `stopped` announces; their results are not waited for.
    fn send_stopped(&mut self) {
        for job in self.announcer.stop(Instant::now()) {
            self.spawn_announce(job, false);
        }
    }

    fn on_announced(
        &mut self,
        job: &Job,
        result: Result<apitorrent_tracker::AnnounceResponse, String>,
    ) {
        let now = Instant::now();
        match result {
            Ok(resp) => {
                tracing::debug!(url = %job.url, peers = resp.peers.len(), "announced");
                self.announcer.on_success(job, &resp, now);
                self.add_candidates(resp.peers);
                self.connect_more();
            }
            Err(e) => {
                tracing::debug!(url = %job.url, error = %e, "announce failed");
                self.announcer.on_failure(job, &e, now);
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

    fn add_candidates(&mut self, peers: Vec<SocketAddr>) {
        for addr in peers {
            if self.known.insert(addr) {
                self.candidates.push_back(addr);
            }
        }
    }

    fn connect_more(&mut self) {
        if self.paused || self.picker.is_none() {
            return;
        }
        let max = self.shared.config.max_peers_per_torrent;
        let now = Instant::now();
        let mut deferred = Vec::new();
        while self.peers.len() < max {
            let Some(addr) = self.candidates.pop_front() else {
                break;
            };
            if self.peers.values().any(|p| p.addr == addr) {
                continue;
            }
            let complete = self.picker.as_ref().is_some_and(|p| p.have().is_full());
            if self.retry_at.get(&addr).is_some_and(|&at| now < at)
                || (complete && self.seed_addrs.contains(&addr))
            {
                deferred.push(addr);
                continue;
            }
            let key = self.shared.next_key();
            self.peers
                .insert(key, Peer::new(addr, self.layout.piece_count(), true));
            let ours = self.our_handshake();
            tokio::spawn(peer::run_outbound(
                addr,
                ours,
                key,
                self.max_message,
                self.peer_tx.clone(),
            ));
        }
        self.candidates.extend(deferred);
    }

    fn our_handshake(&self) -> Handshake {
        peer::our_handshake(
            self.short_hash,
            self.shared.peer_id,
            self.info_hashes.get_v2().is_some(),
        )
    }

    fn accept(&mut self, stream: TcpStream, handshake: Handshake, addr: SocketAddr) {
        if self.paused
            || self.picker.is_none()
            || self.peers.len() >= self.shared.config.max_peers_per_torrent
        {
            return;
        }
        let key = self.shared.next_key();
        self.peers
            .insert(key, Peer::new(addr, self.layout.piece_count(), false));
        // Answer with the hash the peer asked for: v1, or truncated v2 for a v2 swarm.
        let mut ours = self.our_handshake();
        ours.info_hash = handshake.info_hash;
        tokio::spawn(peer::run_inbound(
            stream,
            handshake,
            ours,
            key,
            self.max_message,
            self.peer_tx.clone(),
        ));
    }

    /// Close every connection. Outbound peers stay candidates, so resuming dials them
    /// again straight away.
    fn shutdown_peers(&mut self) {
        let keys: Vec<PeerKey> = self.peers.keys().copied().collect();
        for key in keys {
            if let Some(p) = self.peers.get(&key) {
                p.close();
                if p.outbound {
                    self.retry_at.remove(&p.addr);
                    self.candidates.push_back(p.addr);
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
                tx,
            } => self.on_ready(key, reserved, peer_id, tx),
            PeerEvent::Message(m) => {
                if let Err(reason) = self.on_message(key, m) {
                    tracing::debug!(key, %reason, "dropping peer");
                    if let Some(p) = self.peers.get(&key) {
                        p.close();
                    }
                    self.drop_peer(key);
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
                        self.candidates.push_back(p.addr);
                    }
                }
                self.drop_peer(key);
                self.connect_more();
            }
        }
    }

    fn on_ready(
        &mut self,
        key: PeerKey,
        reserved: apitorrent_wire::Reserved,
        peer_id: PeerId,
        tx: mpsc::UnboundedSender<PeerCmd>,
    ) {
        let Some(picker) = self.picker.as_ref() else {
            let _ = tx.send(PeerCmd::Close);
            return;
        };
        let have = picker.have().clone();
        let Some(peer) = self.peers.get_mut(&key) else {
            let _ = tx.send(PeerCmd::Close);
            return;
        };
        peer.tx = Some(tx);
        peer.peer_id = Some(peer_id);
        peer.fast = reserved.fast();
        if reserved.extension_protocol() {
            let mut ext = ExtensionHandshake {
                client: Some(self.shared.config.client_name.clone()),
                request_queue: Some(MAX_PEER_UPLOAD_QUEUE),
                metadata_size: Some(self.torrent.info.raw().len() as u64),
                upload_only: have.is_full(),
                listen_port: Some(self.shared.listen_port()),
                your_ip: Some(peer.addr.ip()),
                ..Default::default()
            };
            ext.extensions.insert(names::UT_METADATA.to_owned(), 1);
            peer.send(Message::Extended {
                id: 0,
                payload: Bytes::from(ext.encode()),
            });
        }
        if peer.fast && have.is_full() {
            peer.send(Message::HaveAll);
        } else if peer.fast && have.none_set() {
            peer.send(Message::HaveNone);
        } else if !have.none_set() {
            peer.send(Message::Bitfield(Bytes::copy_from_slice(have.as_bytes())));
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
            Message::KeepAlive
            | Message::Port(_)
            | Message::Suggest(_)
            | Message::Unknown { .. } => {}
            Message::Bitfield(bytes) => {
                let has = Bitfield::from_bytes(&bytes, n).ok_or("invalid bitfield")?;
                set_peer_has(picker, peer, has);
                self.update_interest(key);
            }
            Message::HaveAll => {
                if !peer.fast {
                    return Err("have_all without the fast extension".into());
                }
                set_peer_has(picker, peer, Bitfield::full(n));
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
            }
            Message::Choke => {
                peer.peer_choking = true;
                if !peer.fast {
                    for b in peer.requests.drain(..) {
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
            }
            Message::Request(block) => self.on_request(key, block)?,
            Message::Cancel(_) => {}
            Message::Reject(block) => {
                if !peer.fast {
                    return Err("reject without the fast extension".into());
                }
                if let Some(i) = peer.requests.iter().position(|b| *b == block) {
                    peer.requests.swap_remove(i);
                    picker.request_failed(key, &block);
                }
            }
            Message::AllowedFast(_) => {}
            Message::Piece {
                piece,
                offset,
                data,
            } => self.on_piece(key, piece, offset, &data),
            Message::Extended { id, payload } => {
                if id == 0 {
                    let ext = ExtensionHandshake::decode(&payload)
                        .map_err(|e| format!("bad extension handshake: {e}"))?;
                    peer.extensions = Some(ext);
                }
            }
            Message::HashRequest(_) | Message::Hashes { .. } | Message::HashReject(_) => {}
        }
        Ok(())
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
        // Two seeds have nothing to give each other.
        if let (Some(picker), Some(peer)) = (self.picker.as_ref(), self.peers.get(&key))
            && picker.have().is_full()
            && peer.is_seed
        {
            // Kept as a candidate: if files are selected again, we'll want it back.
            self.seed_addrs.insert(peer.addr);
            peer.close();
        }
    }

    fn fill_requests(&mut self, key: PeerKey) {
        let depth = self.shared.config.request_queue;
        let (Some(picker), Some(peer)) = (self.picker.as_mut(), self.peers.get_mut(&key)) else {
            return;
        };
        if peer.peer_choking || !peer.am_interested || peer.requests.len() >= depth {
            return;
        }
        let want = depth - peer.requests.len();
        let max_new = new_piece_room(
            &self.layout,
            picker.pieces_in_progress(),
            self.shared.memory.available(),
            self.memory_limit,
        );
        for block in picker.pick(key, &peer.has, want, max_new) {
            peer.send(Message::Request(block));
            peer.requests.push(block);
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
        let Some(i) = peer.requests.iter().position(|b| *b == block) else {
            // Unsolicited, or a late reply after a cancel. Ignore it.
            return;
        };
        peer.requests.swap_remove(i);
        peer.downloaded += data.len() as u64;
        self.downloaded += data.len() as u64;
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
                for other in cancel {
                    if let Some(p) = self.peers.get_mut(&other)
                        && let Some(j) = p.requests.iter().position(|b| *b == block)
                    {
                        p.requests.swap_remove(j);
                        p.send(Message::Cancel(block));
                    }
                }
                if piece_complete {
                    let data = self.buffers.remove(&piece).expect("buffer exists");
                    self.verify_and_write(piece, data);
                }
            }
        }
        self.fill_requests(key);
    }

    fn verify_and_write(&self, piece: u32, data: Vec<u8>) {
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
            let _ = tx.blocking_send(ActorMsg::PieceWritten { piece, result });
        });
    }

    fn on_piece_written(&mut self, piece: u32, result: Result<Verdict, String>) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        match result {
            Ok(Verdict::Valid) => {
                picker.piece_verified(piece, true);
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
                if !self.finished && self.picker.as_ref().is_some_and(Picker::is_finished) {
                    self.finished = true;
                    let _ = self.storage.sync();
                    tracing::info!(torrent = %self.info_hashes.id_string(), "download complete");
                    self.announcer.completed(Instant::now());
                    self.announce();
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
        if peer.am_choking
            || !picker.have().get(block.piece)
            || peer.uploads >= MAX_PEER_UPLOAD_QUEUE as usize
        {
            if peer.fast {
                peer.send(Message::Reject(block));
            }
            return Ok(());
        }
        peer.uploads += 1;
        let storage = self.storage.clone();
        let offset = self.layout.piece_offset(block.piece) + u64::from(block.offset);
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; block.length as usize];
            let result = storage
                .read(offset, &mut buf)
                .map(|()| Bytes::from(buf))
                .map_err(|e| format!("read failed: {e}"));
            let _ = tx.blocking_send(ActorMsg::BlockRead {
                peer: key,
                block,
                result,
            });
        });
        Ok(())
    }

    fn on_block_read(&mut self, key: PeerKey, block: BlockRef, result: Result<Bytes, String>) {
        let Some(peer) = self.peers.get_mut(&key) else {
            return;
        };
        peer.uploads = peer.uploads.saturating_sub(1);
        match result {
            Ok(data) => {
                if peer.am_choking {
                    return;
                }
                peer.uploaded += data.len() as u64;
                self.uploaded += data.len() as u64;
                peer.send(Message::Piece {
                    piece: block.piece,
                    offset: block.offset,
                    data,
                });
            }
            Err(e) => {
                tracing::error!(error = %e, "disk read error");
                if peer.fast {
                    peer.send(Message::Reject(block));
                }
                self.fail(e);
            }
        }
    }

    // ----- choking ------------------------------------------------------------------------

    fn unchoked_count(&self) -> usize {
        self.peers.values().filter(|p| !p.am_choking).count()
    }

    /// Unchoke a newly interested peer straight away if a slot is free, instead of waiting
    /// for the next choke round.
    fn maybe_unchoke(&mut self, key: PeerKey) {
        let slots = self.shared.config.upload_slots;
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
        let slots = self.shared.config.upload_slots.max(1);
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
                peer.am_choking = true;
                peer.send(Message::Choke);
            }
        }
    }

    // ----- periodic work -------------------------------------------------------------------

    fn on_tick(&mut self) {
        self.ticks = self.ticks.wrapping_add(1);
        for peer in self.peers.values_mut() {
            peer.rate_down = peer.downloaded - peer.last_down;
            peer.rate_up = peer.uploaded - peer.last_up;
            peer.last_down = peer.downloaded;
            peer.last_up = peer.uploaded;
        }
        if self.ticks.is_multiple_of(CHOKE_EVERY) {
            self.run_choker();
        }
        // Another torrent may have freed memory since our peers last asked.
        self.fill_all();
        self.announce();
        self.connect_more();
        self.publish_stats();
        self.last_downloaded = self.downloaded;
        self.last_uploaded = self.uploaded;
    }

    /// Stops the torrent after a storage error: every further transfer would be wasted (or,
    /// for uploads, fail too). It stays stopped until the user resumes it.
    fn fail(&mut self, error: String) {
        if self.error.is_some() {
            return;
        }
        self.error = Some(error);
        if !self.paused {
            self.paused = true;
            self.send_stopped();
            self.shutdown_peers();
        }
        self.publish_stats();
    }

    fn publish_stats(&self) {
        let (state, pieces_have) = match &self.picker {
            None if self.error.is_some() => (TorrentState::Error, 0),
            None => (TorrentState::Checking, 0),
            Some(p) => {
                let state = if self.error.is_some() {
                    TorrentState::Error
                } else if self.paused {
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
        let stats = TorrentStats {
            state,
            pieces_have: self
                .picker
                .as_ref()
                .map_or(pieces_have, |p| p.have().count_ones()),
            pieces_total: self.layout.piece_count(),
            downloaded: self.downloaded,
            uploaded: self.uploaded,
            download_rate: self.downloaded - self.last_downloaded,
            upload_rate: self.uploaded - self.last_uploaded,
            peers,
            seeds,
            error: self.error.clone(),
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

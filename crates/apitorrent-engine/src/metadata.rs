//! Fetching a torrent's info dictionary from peers, for magnet links (BEP 9).
//!
//! A fetch finds peers (from the magnet link, its trackers, and whatever is added later),
//! asks each for the metadata size in the extension handshake, requests the 16 KiB pieces
//! with `ut_metadata`, and checks the assembled dictionary against the info-hash. Peers
//! that took part in a dictionary that didn't match are dropped. The result is the raw
//! info dictionary plus the peers found, for the torrent proper to start with.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot, watch};

use apitorrent_metainfo::{InfoHashes, Sha1Hash, Sha256Hash};
use apitorrent_tracker::{AnnounceEvent, AnnounceRequest};
use apitorrent_wire::Message;
use apitorrent_wire::extended::{ExtensionHandshake, METADATA_PIECE_LEN, MetadataMessage, names};

use crate::limits::Throttle;
use crate::peer::{self, PeerCmd, PeerEvent};
use crate::picker::PeerKey;
use crate::session::Shared;

/// The largest info dictionary we accept. Real ones are far smaller (a few MiB for
/// torrents with tens of thousands of files).
pub const MAX_METADATA_SIZE: u64 = 32 * 1024 * 1024;
/// How many peers a fetch talks to at once.
const MAX_PEERS: usize = 30;
/// How long to wait for a requested piece before asking someone else.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// How often to ask the trackers for more peers while still fetching.
const ANNOUNCE_EVERY: Duration = Duration::from_secs(60);
/// How often to ask the trackers and the DHT again while no peer has been found.
const RETRY_WHEN_ALONE: Duration = Duration::from_secs(10);
/// Our extension id for `ut_metadata` in handshakes we send.
const OUR_UT_METADATA: u8 = 1;

/// Progress of a fetch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchStatus {
    /// Peers connected now.
    pub peers: u32,
    /// Pieces of the dictionary received, and how many there are (0 until a peer says).
    pub pieces_have: u32,
    pub pieces_total: u32,
    /// The last reason the fetch had to start over, if it did.
    pub last_error: Option<String>,
}

/// The fetched metadata.
#[derive(Debug, Clone)]
pub struct Fetched {
    /// The raw bencoded info dictionary, verified against the info-hash.
    pub info: Bytes,
    /// Peers seen during the fetch, to start the torrent with.
    pub peers: Vec<SocketAddr>,
}

enum Cmd {
    AddPeers(Vec<SocketAddr>),
}

/// A fetch in progress. Dropping it cancels the fetch.
#[derive(Debug)]
pub struct MetadataFetch {
    cmd: mpsc::UnboundedSender<Cmd>,
    status: watch::Receiver<FetchStatus>,
    done: Option<oneshot::Receiver<Fetched>>,
}

impl MetadataFetch {
    pub fn status(&self) -> FetchStatus {
        self.status.borrow().clone()
    }

    /// A receiver that changes whenever the status does.
    pub fn watch(&self) -> watch::Receiver<FetchStatus> {
        self.status.clone()
    }

    pub fn add_peers(&self, peers: Vec<SocketAddr>) {
        let _ = self.cmd.send(Cmd::AddPeers(peers));
    }

    /// Wait for the metadata. Returns `None` if the fetch stopped (the session ended).
    pub async fn wait(&mut self) -> Option<Fetched> {
        let rx = self.done.as_mut()?;
        let result = rx.await.ok();
        self.done = None;
        result
    }
}

struct FetchPeer {
    addr: SocketAddr,
    tx: Option<mpsc::UnboundedSender<PeerCmd>>,
    /// Their extension id for `ut_metadata`, once known.
    ut_metadata: Option<u8>,
    /// The piece we asked them for, and when.
    asked: Option<(u32, Instant)>,
    /// Pieces they said they don't have.
    rejected: HashSet<u32>,
}

struct Fetcher {
    hashes: InfoHashes,
    shared: Arc<Shared>,
    throttle: Arc<Throttle>,
    trackers: Vec<String>,
    events: mpsc::Receiver<(PeerKey, PeerEvent)>,
    events_tx: peer::EventTx,
    cmd: mpsc::UnboundedReceiver<Cmd>,
    status: watch::Sender<FetchStatus>,
    peers: HashMap<PeerKey, FetchPeer>,
    candidates: VecDeque<SocketAddr>,
    known: HashSet<SocketAddr>,
    failed: HashSet<SocketAddr>,
    size: Option<u64>,
    pieces: Vec<Option<(Bytes, SocketAddr)>>,
    last_announce: Option<Instant>,
    last_error: Option<String>,
}

pub(crate) fn start(
    shared: Arc<Shared>,
    hashes: InfoHashes,
    trackers: Vec<String>,
    peers: Vec<SocketAddr>,
) -> MetadataFetch {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (status_tx, status_rx) = watch::channel(FetchStatus::default());
    let (done_tx, done_rx) = oneshot::channel();
    let (events_tx, events_rx) = mpsc::channel(1024);
    let throttle = Arc::new(Throttle::new(
        shared.download.clone(),
        shared.upload.clone(),
        None,
        crate::limits::TorrentLimits::default(),
    ));
    let mut f = Fetcher {
        hashes,
        shared,
        throttle,
        trackers,
        events: events_rx,
        events_tx,
        cmd: cmd_rx,
        status: status_tx,
        peers: HashMap::new(),
        candidates: VecDeque::new(),
        known: HashSet::new(),
        failed: HashSet::new(),
        size: None,
        pieces: Vec::new(),
        last_announce: None,
        last_error: None,
    };
    f.add(peers);
    tokio::spawn(async move {
        if let Some(fetched) = f.run().await {
            let _ = done_tx.send(fetched);
        }
    });
    MetadataFetch {
        cmd: cmd_tx,
        status: status_rx,
        done: Some(done_rx),
    }
}

impl Fetcher {
    /// The info-hash used on the wire: v1, or the truncated v2 hash for a v2-only swarm.
    fn wire_hash(&self) -> Sha1Hash {
        self.hashes.primary_short()
    }

    fn add(&mut self, peers: Vec<SocketAddr>) {
        for p in peers {
            if self.known.insert(p) {
                self.candidates.push_back(p);
            }
        }
    }

    async fn run(mut self) -> Option<Fetched> {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let (announced_tx, mut announced_rx) = mpsc::unbounded_channel::<Vec<SocketAddr>>();
        loop {
            self.connect_more();
            // Ask again sooner while nobody has been found.
            let every = if self.peers.is_empty() {
                RETRY_WHEN_ALONE
            } else {
                ANNOUNCE_EVERY
            };
            if self.last_announce.is_none_or(|t| t.elapsed() >= every) {
                self.last_announce = Some(Instant::now());
                self.announce(&announced_tx);
            }
            tokio::select! {
                cmd = self.cmd.recv() => match cmd {
                    Some(Cmd::AddPeers(p)) => self.add(p),
                    // The handle was dropped: the fetch is cancelled.
                    None => break,
                },
                Some(peers) = announced_rx.recv() => self.add(peers),
                Some((key, event)) = self.events.recv() => {
                    if let Some(info) = self.on_event(key, event) {
                        let peers = self.peers.values().map(|p| p.addr).chain(self.candidates.iter().copied()).collect();
                        self.close_all();
                        return Some(Fetched { info, peers });
                    }
                }
                _ = tick.tick() => self.request_more(),
            }
            self.publish();
        }
        self.close_all();
        None
    }

    fn announce(&self, out: &mpsc::UnboundedSender<Vec<SocketAddr>>) {
        // The DHT finds peers for magnet links without trackers.
        if let Some(dht) = self.shared.dht.clone() {
            let mut stream = dht.get_peers(*self.wire_hash().as_bytes());
            let out = out.clone();
            tokio::spawn(async move {
                use futures_util::StreamExt;
                while let Some(peer) = stream.next().await {
                    tracing::debug!(%peer, "metadata fetch: peer from the DHT");
                    if out.send(vec![peer]).is_err() {
                        return;
                    }
                }
            });
        }
        for url in self.trackers.clone() {
            let req = AnnounceRequest {
                info_hash: self.wire_hash(),
                peer_id: self.shared.peer_id,
                port: self.shared.listen_port(),
                uploaded: 0,
                downloaded: 0,
                // Unknown until we have the metadata; any non-zero value marks us as a
                // leecher, so the tracker returns seeds.
                left: 1,
                event: AnnounceEvent::Started,
                num_want: Some(50),
                key: self.shared.announce_key,
                tracker_id: None,
            };
            let tracker = self.shared.tracker.clone();
            let out = out.clone();
            tokio::spawn(async move {
                match tracker.announce(&url, &req).await {
                    Ok(resp) => {
                        let _ = out.send(resp.peers);
                    }
                    Err(e) => tracing::debug!(%url, error = %e, "metadata announce failed"),
                }
            });
        }
    }

    fn connect_more(&mut self) {
        while self.peers.len() < MAX_PEERS {
            let Some(addr) = self.candidates.pop_front() else {
                break;
            };
            if self.failed.contains(&addr) || self.shared.filter.is_blocked(addr.ip()) {
                continue;
            }
            let key = self.shared.next_key();
            self.peers.insert(
                key,
                FetchPeer {
                    addr,
                    tx: None,
                    ut_metadata: None,
                    asked: None,
                    rejected: HashSet::new(),
                },
            );
            let ours = peer::our_handshake(
                self.wire_hash(),
                self.shared.peer_id,
                self.hashes.get_v2().is_some(),
                self.shared.dht.is_some(),
            );
            tokio::spawn(peer::run_outbound(
                addr,
                ours,
                key,
                2 * 1024 * 1024,
                self.events_tx.clone(),
                self.throttle.clone(),
                self.shared.dialer.clone(),
            ));
        }
    }

    /// Handle one peer event. Returns the verified dictionary when it is complete.
    fn on_event(&mut self, key: PeerKey, event: PeerEvent) -> Option<Bytes> {
        match event {
            PeerEvent::Ready { reserved, tx, .. } => {
                let Some(p) = self.peers.get_mut(&key) else {
                    let _ = tx.send(PeerCmd::Close);
                    return None;
                };
                if !reserved.extension_protocol() {
                    let _ = tx.send(PeerCmd::Close);
                    self.peers.remove(&key);
                    return None;
                }
                let mut ext = ExtensionHandshake {
                    client: Some(self.shared.config.client_name.clone()),
                    listen_port: Some(self.shared.listen_port()),
                    ..Default::default()
                };
                ext.extensions
                    .insert(names::UT_METADATA.to_owned(), OUR_UT_METADATA);
                let _ = tx.send(PeerCmd::Send(Message::Extended {
                    id: 0,
                    payload: Bytes::from(ext.encode()),
                }));
                if reserved.fast() {
                    let _ = tx.send(PeerCmd::Send(Message::HaveNone));
                }
                p.tx = Some(tx);
            }
            PeerEvent::Message(Message::Extended { id: 0, payload }) => {
                let Ok(ext) = ExtensionHandshake::decode(&payload) else {
                    self.drop_peer(key, false);
                    return None;
                };
                let (Some(id), Some(size)) = (ext.id_of(names::UT_METADATA), ext.metadata_size)
                else {
                    // Can't help us.
                    self.drop_peer(key, false);
                    return None;
                };
                if size == 0 || size > MAX_METADATA_SIZE {
                    self.drop_peer(key, true);
                    return None;
                }
                if self.size.is_none() {
                    self.size = Some(size);
                    let count =
                        usize::try_from(size.div_ceil(METADATA_PIECE_LEN as u64)).expect("bounded");
                    self.pieces = vec![None; count];
                } else if self.size != Some(size) {
                    // Disagrees with the size we're assembling. One of them is wrong; the
                    // hash check will tell. Don't use this peer for now.
                    self.drop_peer(key, false);
                    return None;
                }
                if let Some(p) = self.peers.get_mut(&key) {
                    p.ut_metadata = Some(id);
                }
                self.request_more();
            }
            PeerEvent::Message(Message::Extended {
                id: OUR_UT_METADATA,
                payload,
            }) => {
                return self.on_metadata(key, &payload);
            }
            PeerEvent::Message(_) => {}
            PeerEvent::Closed { reason } => {
                tracing::debug!(key, %reason, "metadata fetch: peer closed");
                if let Some(p) = self.peers.remove(&key)
                    && p.tx.is_none()
                {
                    self.failed.insert(p.addr);
                }
            }
        }
        None
    }

    fn on_metadata(&mut self, key: PeerKey, payload: &Bytes) -> Option<Bytes> {
        tracing::debug!(
            key,
            len = payload.len(),
            "metadata fetch: ut_metadata message"
        );
        let Ok(msg) = MetadataMessage::decode(payload) else {
            self.drop_peer(key, true);
            return None;
        };
        let addr = self.peers.get(&key).map(|p| p.addr)?;
        match msg {
            MetadataMessage::Request { piece } => {
                // We have nothing to give yet.
                self.send_metadata(key, &MetadataMessage::Reject { piece });
            }
            MetadataMessage::Reject { piece } => {
                if let Some(p) = self.peers.get_mut(&key) {
                    p.rejected.insert(piece);
                    p.asked = None;
                }
            }
            MetadataMessage::Data {
                piece,
                total_size,
                data,
            } => {
                let expected = self.piece_len(piece);
                if Some(total_size) != self.size || expected != Some(data.len()) {
                    self.drop_peer(key, true);
                    return None;
                }
                if let Some(p) = self.peers.get_mut(&key) {
                    p.asked = None;
                }
                self.pieces[piece as usize] = Some((data, addr));
                if self.pieces.iter().all(Option::is_some) {
                    return self.finish();
                }
            }
        }
        self.request_more();
        None
    }

    fn piece_len(&self, piece: u32) -> Option<usize> {
        let size = usize::try_from(self.size?).ok()?;
        let start = piece as usize * METADATA_PIECE_LEN;
        (start < size).then(|| METADATA_PIECE_LEN.min(size - start))
    }

    /// All pieces are in: check the hash. On a mismatch, drop everyone who contributed
    /// and start over.
    fn finish(&mut self) -> Option<Bytes> {
        let mut info = Vec::with_capacity(usize::try_from(self.size.unwrap_or(0)).unwrap_or(0));
        for (data, _) in self.pieces.iter().flatten() {
            info.extend_from_slice(data);
        }
        let ok = match (self.hashes.get_v1(), self.hashes.get_v2()) {
            (Some(v1), _) => Sha1Hash::digest(&info) == v1,
            (None, Some(v2)) => Sha256Hash::digest(&info) == v2,
            (None, None) => false,
        };
        if ok {
            return Some(Bytes::from(info));
        }
        let senders: HashSet<SocketAddr> = self.pieces.iter().flatten().map(|(_, a)| *a).collect();
        tracing::warn!(
            senders = senders.len(),
            "fetched metadata didn't match the info-hash"
        );
        self.last_error = Some("metadata from peers didn't match the info-hash; retrying".into());
        let keys: Vec<PeerKey> = self
            .peers
            .iter()
            .filter(|(_, p)| senders.contains(&p.addr))
            .map(|(k, _)| *k)
            .collect();
        for k in keys {
            self.drop_peer(k, true);
        }
        self.size = None;
        self.pieces.clear();
        None
    }

    /// Ask idle peers for pieces nobody has delivered yet.
    fn request_more(&mut self) {
        if self.size.is_none() {
            return;
        }
        let now = Instant::now();
        // Requests that timed out free their piece.
        for p in self.peers.values_mut() {
            if p.asked
                .is_some_and(|(_, at)| now.duration_since(at) > REQUEST_TIMEOUT)
            {
                p.asked = None;
            }
        }
        let outstanding: HashSet<u32> = self
            .peers
            .values()
            .filter_map(|p| p.asked.map(|a| a.0))
            .collect();
        let mut missing = (0..u32::try_from(self.pieces.len()).expect("bounded"))
            .filter(|&i| self.pieces[i as usize].is_none() && !outstanding.contains(&i))
            .collect::<VecDeque<u32>>();
        let keys: Vec<PeerKey> = self.peers.keys().copied().collect();
        for key in keys {
            let Some(p) = self.peers.get(&key) else {
                continue;
            };
            if p.asked.is_some() || p.ut_metadata.is_none() || p.tx.is_none() {
                continue;
            }
            let Some(pos) = missing.iter().position(|i| !p.rejected.contains(i)) else {
                continue;
            };
            let piece = missing.remove(pos).expect("found");
            tracing::debug!(key, piece, "metadata fetch: requesting");
            self.send_metadata(key, &MetadataMessage::Request { piece });
            if let Some(p) = self.peers.get_mut(&key) {
                p.asked = Some((piece, now));
            }
        }
    }

    fn send_metadata(&self, key: PeerKey, msg: &MetadataMessage) {
        if let Some(p) = self.peers.get(&key)
            && let (Some(tx), Some(id)) = (&p.tx, p.ut_metadata)
        {
            let _ = tx.send(PeerCmd::Send(Message::Extended {
                id,
                payload: Bytes::from(msg.encode()),
            }));
        }
    }

    fn drop_peer(&mut self, key: PeerKey, bad: bool) {
        if let Some(p) = self.peers.remove(&key) {
            tracing::debug!(key, addr = %p.addr, bad, "metadata fetch: dropping peer");
            if let Some(tx) = &p.tx {
                let _ = tx.send(PeerCmd::Close);
            }
            if bad {
                self.failed.insert(p.addr);
            }
        }
    }

    fn close_all(&mut self) {
        for p in self.peers.values() {
            if let Some(tx) = &p.tx {
                let _ = tx.send(PeerCmd::Close);
            }
        }
        self.peers.clear();
    }

    fn publish(&self) {
        let status = FetchStatus {
            peers: u32::try_from(self.peers.values().filter(|p| p.tx.is_some()).count())
                .unwrap_or(u32::MAX),
            pieces_have: u32::try_from(self.pieces.iter().filter(|p| p.is_some()).count())
                .unwrap_or(u32::MAX),
            pieces_total: u32::try_from(self.pieces.len()).unwrap_or(u32::MAX),
            last_error: self.last_error.clone(),
        };
        self.status.send_if_modified(|old| {
            if *old == status {
                false
            } else {
                *old = status;
                true
            }
        });
    }
}

/// Build a `.torrent` file from a fetched info dictionary and a magnet link's trackers
/// and web seeds, ready for [`apitorrent_metainfo::Torrent::from_bytes`].
pub fn torrent_file(info: &[u8], trackers: &[String], web_seeds: &[String]) -> Vec<u8> {
    use apitorrent_bencode::OwnedValue;
    let mut root = OwnedValue::dict().insert("info", OwnedValue::Raw(info.to_vec()));
    if let Some(first) = trackers.first() {
        root = root.insert("announce", first.as_str());
        // Magnet links have no tiers: one tracker per tier keeps them all in use.
        let tiers: Vec<OwnedValue> = trackers
            .iter()
            .map(|t| OwnedValue::List(vec![OwnedValue::from(t.as_str())]))
            .collect();
        root = root.insert("announce-list", OwnedValue::List(tiers));
    }
    if !web_seeds.is_empty() {
        let seeds: Vec<OwnedValue> = web_seeds
            .iter()
            .map(|s| OwnedValue::from(s.as_str()))
            .collect();
        root = root.insert("url-list", OwnedValue::List(seeds));
    }
    root.to_bytes()
}

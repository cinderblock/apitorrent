//! The session: our identity on the network, the listen socket, and the set of torrents.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use apitorrent_metainfo::{InfoHashes, Sha1Hash};
use apitorrent_tracker::TrackerClient;
use apitorrent_wire::PeerId;

use crate::ban::{BanInfo, BanPolicy, BanReason, Blocklist, IpFilter};
use crate::limits::{Bucket, Counter, Group, GroupLimits};
use crate::peer;
use crate::torrent::{ActorMsg, AddTorrent, TorrentActor, TorrentHandle};

#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Where to accept peer connections. Port 0 picks a free port.
    pub listen: SocketAddr,
    pub max_peers_per_torrent: usize,
    /// Outstanding block requests per peer.
    pub request_queue: usize,
    /// Peers we upload to at once, per torrent (one of them is the optimistic unchoke).
    pub upload_slots: usize,
    /// Sent to peers in the extension handshake.
    pub client_name: String,
    /// Most bytes of piece data held in memory at once, across all torrents: pieces being
    /// downloaded plus pieces waiting to be hashed and written. When it is reached, torrents
    /// finish the pieces they have started before starting new ones.
    pub memory_limit: u64,
    /// How long bans for bad data last.
    pub ban_policy: BanPolicy,
    /// Session-wide rates in bytes per second, protocol overhead included. 0 is unlimited.
    pub download_rate: u64,
    pub upload_rate: u64,
    /// Open peer connections across all torrents. 0 is unlimited.
    pub max_connections: usize,
    /// Outgoing connection attempts in progress at once. 0 is unlimited.
    pub max_half_open: usize,
    /// New outgoing connection attempts per second. 0 is unlimited.
    pub connect_rate: u64,
    /// How often to send peer exchange messages. BEP 11 asks for at most once a minute.
    pub pex_interval: std::time::Duration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 6881)),
            max_peers_per_torrent: 60,
            request_queue: 64,
            upload_slots: 8,
            client_name: concat!("apitorrent ", env!("CARGO_PKG_VERSION")).to_owned(),
            memory_limit: 256 * 1024 * 1024,
            ban_policy: BanPolicy::default(),
            download_rate: 0,
            upload_rate: 0,
            max_connections: 500,
            max_half_open: 64,
            connect_rate: 30,
            pex_interval: std::time::Duration::from_secs(60),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AddError {
    #[error("the torrent is already in the session")]
    Duplicate,
}

/// State shared between the session and its torrents.
#[derive(Debug)]
pub(crate) struct Shared {
    pub(crate) config: SessionConfig,
    pub(crate) peer_id: PeerId,
    pub(crate) tracker: TrackerClient,
    /// Sent with every announce so trackers can recognise us across IP changes.
    pub(crate) announce_key: u32,
    pub(crate) memory: MemoryBudget,
    pub(crate) filter: IpFilter,
    pub(crate) download: Arc<Bucket>,
    pub(crate) upload: Arc<Bucket>,
    pub(crate) connections: Arc<Counter>,
    pub(crate) half_open: Arc<Counter>,
    pub(crate) connect_rate: Bucket,
    groups: Mutex<HashMap<String, Arc<Group>>>,
    listen_port: u16,
    next_key: AtomicU64,
    torrents: Mutex<HashMap<Sha1Hash, mpsc::Sender<ActorMsg>>>,
}

/// The session's memory budget for piece data. Torrents reserve and release whole pieces;
/// the budget only says how much room is left, it never blocks.
#[derive(Debug)]
pub(crate) struct MemoryBudget {
    limit: u64,
    used: AtomicU64,
}

impl MemoryBudget {
    pub(crate) fn new(limit: u64) -> Self {
        Self {
            limit,
            used: AtomicU64::new(0),
        }
    }

    pub(crate) fn available(&self) -> u64 {
        self.limit.saturating_sub(self.used())
    }

    pub(crate) fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    pub(crate) fn add(&self, bytes: u64) {
        self.used.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn sub(&self, bytes: u64) {
        self.used.fetch_sub(bytes, Ordering::Relaxed);
    }
}

impl Shared {
    pub(crate) fn next_key(&self) -> u64 {
        self.next_key.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn listen_port(&self) -> u16 {
        self.listen_port
    }

    pub(crate) fn group(&self, name: &str) -> Option<Arc<Group>> {
        self.groups
            .lock()
            .expect("groups poisoned")
            .get(name)
            .cloned()
    }

    pub(crate) fn unregister(&self, hashes: &InfoHashes) {
        let mut map = self.torrents.lock().expect("torrent map poisoned");
        if let Some(v1) = hashes.get_v1() {
            map.remove(&v1);
        }
        if let Some(v2) = hashes.get_v2() {
            map.remove(&v2.truncated());
        }
    }
}

/// A running engine.
#[derive(Debug, Clone)]
pub struct Session {
    shared: Arc<Shared>,
    listen_addr: SocketAddr,
}

impl Session {
    /// Bind the listen socket and start accepting peers.
    pub async fn start(config: SessionConfig) -> io::Result<Self> {
        let listener = TcpListener::bind(config.listen).await?;
        let listen_addr = listener.local_addr()?;
        let version = version_digits();
        let peer_id = PeerId::azureus(*b"AT", version, rand::random());
        let tracker = TrackerClient::new().map_err(io::Error::other)?;
        let memory = MemoryBudget::new(config.memory_limit);
        let filter = IpFilter::new(config.ban_policy);
        let shared = Arc::new(Shared {
            download: Arc::new(Bucket::new(config.download_rate)),
            upload: Arc::new(Bucket::new(config.upload_rate)),
            connections: Counter::new(config.max_connections),
            half_open: Counter::new(config.max_half_open),
            connect_rate: Bucket::new(config.connect_rate),
            groups: Mutex::new(HashMap::new()),
            config,
            memory,
            filter,
            peer_id,
            tracker,
            announce_key: rand::random(),
            listen_port: listen_addr.port(),
            next_key: AtomicU64::new(1),
            torrents: Mutex::new(HashMap::new()),
        });
        tokio::spawn(accept_loop(listener, shared.clone()));
        Ok(Self {
            shared,
            listen_addr,
        })
    }

    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    pub fn peer_id(&self) -> PeerId {
        self.shared.peer_id
    }

    /// Bytes of piece data held in memory right now, across all torrents.
    pub fn memory_used(&self) -> u64 {
        self.shared.memory.used()
    }

    /// Ban an address by hand. Its connections close within a second.
    pub fn ban(&self, ip: std::net::IpAddr) -> std::time::Duration {
        self.shared.filter.ban(ip, BanReason::Manual)
    }

    pub fn unban(&self, ip: std::net::IpAddr) -> bool {
        self.shared.filter.unban(ip)
    }

    /// Addresses banned right now.
    pub fn bans(&self) -> Vec<BanInfo> {
        self.shared.filter.bans()
    }

    /// Replace the IP blocklist. Connections from listed addresses close within a second.
    pub fn set_blocklist(&self, list: Blocklist) {
        self.shared.filter.set_blocklist(list);
    }

    /// Change the session-wide rates (bytes per second, 0 unlimited).
    pub fn set_rate_limits(&self, download: u64, upload: u64) {
        self.shared.download.set_rate(download);
        self.shared.upload.set_rate(upload);
    }

    /// Change the connection limits (0 unlimited). Existing connections are kept; new
    /// ones wait for room.
    pub fn set_connection_limits(
        &self,
        max_connections: usize,
        max_half_open: usize,
        connect_rate: u64,
    ) {
        self.shared.connections.set_limit(max_connections);
        self.shared.half_open.set_limit(max_half_open);
        self.shared.connect_rate.set_rate(connect_rate);
    }

    /// Open peer connections right now, across all torrents.
    pub fn connections(&self) -> usize {
        self.shared.connections.used()
    }

    /// Create a group, or change its limits. Torrents join with
    /// [`TorrentHandle::set_group`] or [`AddTorrent::group`].
    pub fn set_group(&self, name: &str, limits: GroupLimits) {
        let mut groups = self.shared.groups.lock().expect("groups poisoned");
        match groups.get(name) {
            Some(g) => g.set(limits),
            None => {
                groups.insert(name.to_owned(), Group::new(limits));
            }
        }
    }

    /// Remove a group. Its torrents keep its limits until they leave it.
    pub fn remove_group(&self, name: &str) -> bool {
        self.shared
            .groups
            .lock()
            .expect("groups poisoned")
            .remove(name)
            .is_some()
    }

    pub fn groups(&self) -> Vec<(String, GroupLimits)> {
        let mut out: Vec<(String, GroupLimits)> = self
            .shared
            .groups
            .lock()
            .expect("groups poisoned")
            .iter()
            .map(|(n, g)| (n.clone(), g.limits()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Start fetching the info dictionary for a magnet link (BEP 9). The peers in the link
    /// (`x.pe`) and its trackers are used; more can be added to the returned fetch.
    pub fn fetch_metadata(
        &self,
        magnet: &apitorrent_metainfo::Magnet,
    ) -> crate::metadata::MetadataFetch {
        let peers = magnet
            .peers
            .iter()
            .filter_map(|p| p.parse::<SocketAddr>().ok())
            .collect();
        crate::metadata::start(
            self.shared.clone(),
            magnet.info_hashes,
            magnet.trackers.clone(),
            peers,
        )
    }

    /// Whether connections to and from `ip` are refused.
    pub fn is_blocked(&self, ip: std::net::IpAddr) -> bool {
        self.shared.filter.is_blocked(ip)
    }

    pub fn add_torrent(&self, params: AddTorrent) -> Result<TorrentHandle, AddError> {
        let hashes = params.torrent.info.info_hashes();
        {
            let map = self.shared.torrents.lock().expect("torrent map poisoned");
            let taken = hashes.get_v1().is_some_and(|h| map.contains_key(&h))
                || hashes
                    .get_v2()
                    .is_some_and(|h| map.contains_key(&h.truncated()));
            if taken {
                return Err(AddError::Duplicate);
            }
        }
        let (handle, keys) = TorrentActor::spawn(params, self.shared.clone());
        let mut map = self.shared.torrents.lock().expect("torrent map poisoned");
        for key in keys {
            map.insert(key, handle.tx.clone());
        }
        Ok(handle)
    }
}

/// The crate version as four peer-id digits: `0.1.0` → `0100`.
fn version_digits() -> [u8; 4] {
    let mut out = [b'0'; 4];
    let parts = [
        env!("CARGO_PKG_VERSION_MAJOR"),
        env!("CARGO_PKG_VERSION_MINOR"),
        env!("CARGO_PKG_VERSION_PATCH"),
    ];
    for (slot, part) in out.iter_mut().zip(parts) {
        let n: u32 = part.parse().unwrap_or(0);
        *slot = char::from_digit(n.min(35), 36).map_or(b'0', |c| {
            u8::try_from(c.to_ascii_uppercase()).unwrap_or(b'0')
        });
    }
    out
}

async fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                if shared.filter.is_blocked(addr.ip()) {
                    tracing::debug!(%addr, "refused a blocked peer");
                    continue;
                }
                tokio::spawn(route_incoming(stream, addr, shared.clone()));
            }
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

/// Read an incoming handshake and hand the connection to the torrent it names.
async fn route_incoming(mut stream: TcpStream, addr: SocketAddr, shared: Arc<Shared>) {
    let _ = stream.set_nodelay(true);
    let handshake = match peer::read_handshake(&mut stream).await {
        Ok(h) => h,
        Err(reason) => {
            tracing::debug!(%addr, %reason, "incoming connection rejected");
            return;
        }
    };
    let tx = shared
        .torrents
        .lock()
        .expect("torrent map poisoned")
        .get(&handshake.info_hash)
        .cloned();
    if let Some(tx) = tx {
        let _ = tx
            .send(ActorMsg::Incoming {
                stream,
                handshake,
                addr,
            })
            .await;
    }
}

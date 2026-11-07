//! The session: our identity on the network, the listen socket, and the set of torrents.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::net::TcpListener;
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
    /// Local service discovery (BEP 14). Off unless set: listening on the LSD port makes
    /// some systems ask the user about firewall access.
    pub lsd: Option<crate::lsd::LsdConfig>,
    /// The Mainline DHT (BEP 5). Off unless set, for the same reason. Its UDP port is
    /// separate from the peer port (peers learn it from our PORT messages), which leaves
    /// the peer port's UDP side free for uTP.
    pub dht: Option<apitorrent_dht::DhtConfig>,
    /// Accept and make uTP (BEP 29) connections, on the UDP side of the peer port.
    pub utp: bool,
    /// Try uTP before TCP for outgoing connections. Either way the other is the fallback.
    /// uTP backs off when it sees queues building, so it keeps torrent traffic from
    /// slowing the rest of the network.
    pub prefer_utp: bool,
    /// Message stream encryption for peer connections, both directions.
    pub encryption: apitorrent_mse::Policy,
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
            lsd: None,
            dht: None,
            utp: true,
            prefer_utp: true,
            encryption: apitorrent_mse::Policy::default(),
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
    /// For web seeds: verified TLS, no proxies from the environment.
    pub(crate) http: apitorrent_tracker::reqwest::Client,
    pub(crate) dht: Option<apitorrent_dht::Dht>,
    pub(crate) dialer: Arc<peer::Dialer>,
    pub(crate) download: Arc<Bucket>,
    pub(crate) upload: Arc<Bucket>,
    pub(crate) connections: Arc<Counter>,
    pub(crate) half_open: Arc<Counter>,
    pub(crate) connect_rate: Bucket,
    groups: Mutex<HashMap<String, Arc<Group>>>,
    listen_port: u16,
    next_key: AtomicU64,
    torrents: Mutex<HashMap<Sha1Hash, mpsc::Sender<ActorMsg>>>,
    /// The wire info-hashes in `torrents`, by `HASH('req2', info_hash)`: how an incoming
    /// encrypted connection names its torrent.
    skeys: Mutex<apitorrent_mse::SkeyTable>,
    /// Wire info-hashes of torrents that aren't private: the ones LSD and DHT may announce
    /// and find peers for.
    public: Mutex<HashSet<Sha1Hash>>,
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
        let mut public = self.public.lock().expect("public set poisoned");
        let mut skeys = self.skeys.lock().expect("skey table poisoned");
        if let Some(v1) = hashes.get_v1() {
            map.remove(&v1);
            public.remove(&v1);
            skeys.remove(&v1);
        }
        if let Some(v2) = hashes.get_v2() {
            map.remove(&v2.truncated());
            public.remove(&v2.truncated());
            skeys.remove(&v2.truncated());
        }
    }

    /// The UDP port our DHT listens on, for PORT messages.
    pub(crate) fn dht_port(&self) -> Option<u16> {
        let dht = self.dht.as_ref()?;
        dht.local_addr_v4()
            .or_else(|| dht.local_addr_v6())
            .map(|a| a.port())
    }

    pub(crate) fn public_hashes(&self) -> HashSet<Sha1Hash> {
        self.public.lock().expect("public set poisoned").clone()
    }

    /// Peers found by a discovery service (LSD, DHT) for a wire info-hash. Only public
    /// torrents get them.
    pub(crate) async fn discovered(&self, hash: &Sha1Hash, peers: Vec<SocketAddr>) {
        if !self
            .public
            .lock()
            .expect("public set poisoned")
            .contains(hash)
        {
            return;
        }
        let tx = self
            .torrents
            .lock()
            .expect("torrent map poisoned")
            .get(hash)
            .cloned();
        if let Some(tx) = tx {
            let _ = tx
                .send(ActorMsg::Command(crate::torrent::Command::AddPeers(peers)))
                .await;
        }
    }
}

/// A running engine.
#[derive(Debug, Clone)]
pub struct Session {
    shared: Arc<Shared>,
    listen_addr: SocketAddr,
    lsd_addr: Option<SocketAddr>,
    utp_addr: Option<SocketAddr>,
}

impl Session {
    /// Bind the listen socket and start accepting peers.
    pub async fn start(config: SessionConfig) -> io::Result<Self> {
        let (listener, utp) = bind_peer_port(&config).await?;
        let listen_addr = listener.local_addr()?;
        let version = version_digits();
        let peer_id = PeerId::azureus(*b"AT", version, rand::random());
        let tracker = TrackerClient::new().map_err(io::Error::other)?;
        let http = apitorrent_tracker::strict_http_client(
            &config.client_name,
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(15),
        )
        .map_err(io::Error::other)?;
        let dht = match config.dht.clone() {
            Some(cfg) => Some(
                apitorrent_dht::Dht::start(cfg)
                    .await
                    .map_err(io::Error::other)?,
            ),
            None => None,
        };
        let dialer = Arc::new(peer::Dialer {
            utp: utp.clone(),
            prefer_utp: config.prefer_utp,
            encryption: config.encryption,
        });
        let memory = MemoryBudget::new(config.memory_limit);
        let filter = IpFilter::new(config.ban_policy);
        let shared = Arc::new(Shared {
            download: Arc::new(Bucket::new(config.download_rate)),
            upload: Arc::new(Bucket::new(config.upload_rate)),
            connections: Counter::new(config.max_connections),
            half_open: Counter::new(config.max_half_open),
            connect_rate: Bucket::new(config.connect_rate),
            groups: Mutex::new(HashMap::new()),
            public: Mutex::new(HashSet::new()),
            config,
            memory,
            filter,
            http,
            dht,
            dialer,
            peer_id,
            tracker,
            announce_key: rand::random(),
            listen_port: listen_addr.port(),
            next_key: AtomicU64::new(1),
            torrents: Mutex::new(HashMap::new()),
            skeys: Mutex::new(apitorrent_mse::SkeyTable::new()),
        });
        tokio::spawn(accept_loop(listener, shared.clone()));
        let utp_addr = utp.as_ref().map(apitorrent_utp::UtpSocket::local_addr);
        if let Some(socket) = utp {
            tokio::spawn(utp_accept_loop(socket, shared.clone()));
        }
        if shared.dht.is_some() {
            tokio::spawn(dht_announcer(Arc::downgrade(&shared)));
        }
        let lsd_addr = match shared.config.lsd.clone() {
            Some(cfg) => Some(crate::lsd::start(&shared, cfg)?),
            None => None,
        };
        Ok(Self {
            shared,
            listen_addr,
            lsd_addr,
            utp_addr,
        })
    }

    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    /// The uTP socket's address, when uTP is on.
    pub fn utp_addr(&self) -> Option<SocketAddr> {
        self.utp_addr
    }

    /// The LSD socket's address, when LSD is on.
    pub fn lsd_addr(&self) -> Option<SocketAddr> {
        self.lsd_addr
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

    /// The session-wide memory budget for piece data.
    pub fn memory_limit(&self) -> u64 {
        self.shared.config.memory_limit
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

    /// The DHT's state, to save and pass back in [`SessionConfig::dht`] next time.
    pub async fn dht_state(&self) -> Option<Vec<u8>> {
        self.shared.dht.as_ref()?.export_state().await.ok()
    }

    /// The DHT's IPv4 socket address, when it runs.
    pub fn dht_addr(&self) -> Option<SocketAddr> {
        let dht = self.shared.dht.as_ref()?;
        dht.local_addr_v4().or_else(|| dht.local_addr_v6())
    }

    /// The DHT's routing table and store, when it runs.
    pub fn dht_stats(&self) -> Option<apitorrent_dht::DhtStats> {
        self.shared.dht.as_ref().map(apitorrent_dht::Dht::stats)
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
        let private = params.torrent.info.private;
        let (handle, keys) = TorrentActor::spawn(params, self.shared.clone());
        let mut map = self.shared.torrents.lock().expect("torrent map poisoned");
        let mut public = self.shared.public.lock().expect("public set poisoned");
        let mut skeys = self.shared.skeys.lock().expect("skey table poisoned");
        for key in keys {
            map.insert(key, handle.tx.clone());
            skeys.insert(key);
            if !private {
                public.insert(key);
            }
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
                let _ = stream.set_nodelay(true);
                tokio::spawn(route_incoming(
                    Box::new(stream),
                    false,
                    addr,
                    shared.clone(),
                ));
            }
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

/// Bind the peer port: TCP, and the same port on UDP for uTP when it is on. When the
/// configured port is 0, keep trying free TCP ports until one is also free on UDP, so the
/// two always match (peers reach uTP on the port they learn for TCP). A fixed port whose
/// UDP side is taken runs without uTP.
async fn bind_peer_port(
    config: &SessionConfig,
) -> io::Result<(TcpListener, Option<apitorrent_utp::UtpSocket>)> {
    const ATTEMPTS: usize = 20;
    let utp_config = apitorrent_utp::Config {
        connect_timeout: UTP_CONNECT_TIMEOUT,
        ..apitorrent_utp::Config::default()
    };
    let mut attempt = 0;
    loop {
        attempt += 1;
        let listener = TcpListener::bind(config.listen).await?;
        if !config.utp {
            return Ok((listener, None));
        }
        let addr = listener.local_addr()?;
        match apitorrent_utp::UtpSocket::bind_with_config(addr, utp_config).await {
            Ok(socket) => return Ok((listener, Some(socket))),
            Err(e) if config.listen.port() == 0 && attempt < ATTEMPTS => {
                tracing::debug!(%addr, error = %e, "UDP side taken, trying another port");
            }
            Err(e) => {
                tracing::warn!(%addr, error = %e, "uTP is off: could not bind");
                return Ok((listener, None));
            }
        }
    }
}

/// How long an outgoing uTP connection keeps retrying its SYN before we fall back to TCP.
/// With uTP's 1 s initial timeout doubling, that is three tries.
const UTP_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

async fn utp_accept_loop(socket: apitorrent_utp::UtpSocket, shared: Arc<Shared>) {
    loop {
        match socket.accept().await {
            Ok((stream, addr)) => {
                if shared.filter.is_blocked(addr.ip()) {
                    tracing::debug!(%addr, "refused a blocked peer");
                    continue;
                }
                tokio::spawn(route_incoming(Box::new(stream), true, addr, shared.clone()));
            }
            // The socket only fails like this once it has shut down.
            Err(e) => {
                tracing::debug!(error = %e, "uTP accept stopped");
                return;
            }
        }
    }
}

/// Finds a torrent for an incoming encrypted connection.
struct SkeyIndex(Arc<Shared>);

impl apitorrent_mse::SkeyLookup for SkeyIndex {
    fn lookup(&self, req2: &[u8; 20]) -> Option<Sha1Hash> {
        self.0
            .skeys
            .lock()
            .expect("skey table poisoned")
            .lookup(req2)
    }
}

/// Run the encryption handshake if the peer starts one, read its BitTorrent handshake,
/// and hand the connection to the torrent it names.
async fn route_incoming(stream: peer::BoxIo, utp: bool, addr: SocketAddr, shared: Arc<Shared>) {
    let policy = shared.config.encryption;
    let (mut stream, link, named): (peer::BoxIo, peer::Link, Option<Sha1Hash>) = if policy
        .accepts_mse()
    {
        let config = apitorrent_mse::MseConfig::new(policy);
        match apitorrent_mse::MseStream::accept(stream, SkeyIndex(shared.clone()), config).await {
            Ok(s) => {
                let link = peer::Link {
                    utp,
                    encrypted: s.encryption().is_rc4(),
                };
                let named = s.info_hash();
                (Box::new(s), link, named)
            }
            Err(e) => {
                tracing::debug!(%addr, error = %e, "incoming connection rejected");
                return;
            }
        }
    } else {
        let link = peer::Link {
            utp,
            encrypted: false,
        };
        (stream, link, None)
    };
    let handshake = match peer::read_handshake(&mut stream).await {
        Ok(h) => h,
        Err(reason) => {
            tracing::debug!(%addr, %reason, "incoming connection rejected");
            return;
        }
    };
    // An encrypted connection already named its torrent; the handshake must agree.
    if named.is_some_and(|h| h != handshake.info_hash) {
        tracing::debug!(%addr, "incoming connection rejected: handshake names another torrent");
        return;
    }
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
                link,
                addr,
            })
            .await;
    }
}

/// How often each public torrent is announced on the DHT.
const DHT_ANNOUNCE_EVERY: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Announce every public torrent on the DHT (BEP 5) when it appears and every 15 minutes,
/// feeding the peers the lookups find to the torrent. Stops with the session.
async fn dht_announcer(weak: std::sync::Weak<Shared>) {
    use futures_util::StreamExt;
    let mut last: HashMap<Sha1Hash, std::time::Instant> = HashMap::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
    loop {
        tick.tick().await;
        let Some(shared) = weak.upgrade() else { return };
        let Some(dht) = shared.dht.clone() else {
            return;
        };
        // An announce made while the routing table is empty (just after start) reaches
        // nobody and would wait 15 minutes to be retried. Wait for nodes first.
        let stats = dht.stats();
        let known = stats.ipv4.map_or(0, |t| t.nodes) + stats.ipv6.map_or(0, |t| t.nodes);
        if known == 0 {
            continue;
        }
        let now = std::time::Instant::now();
        let public = shared.public_hashes();
        last.retain(|h, _| public.contains(h));
        for hash in public {
            if last
                .get(&hash)
                .is_some_and(|t| now.duration_since(*t) < DHT_ANNOUNCE_EVERY)
            {
                continue;
            }
            last.insert(hash, now);
            let port = shared.listen_port();
            let weak = weak.clone();
            let mut stream = dht.announce(*hash.as_bytes(), Some(port));
            tokio::spawn(async move {
                while let Some(peer) = stream.next().await {
                    let Some(shared) = weak.upgrade() else { return };
                    shared.discovered(&hash, vec![peer]).await;
                }
            });
        }
    }
}

//! The session: our identity on the network, the listen socket, and the set of torrents.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::net::TcpListener;
use tokio::sync::mpsc;

use apitorrent_metainfo::{InfoHashes, Sha1Hash};
use apitorrent_proxy::Proxy;
use apitorrent_tracker::TrackerClient;
use apitorrent_wire::PeerId;

use crate::ban::{BanInfo, BanPolicy, BanReason, Blocklist, IpFilter};
use crate::identity::IdentityRules;
use crate::limits::{Bucket, Counter, Group, GroupLimits};
use crate::peer;
use crate::reachability::{Reachability, ReachabilityReport};
use crate::torrent::{ActorMsg, AddTorrent, TorrentActor, TorrentHandle};

#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Where to accept peer connections. Port 0 picks a free port.
    pub listen: SocketAddr,
    pub max_peers_per_torrent: usize,
    /// The fewest block requests kept outstanding to a peer. Faster peers get more: three
    /// seconds of their rate, up to 500 or what the peer takes (its `reqq`).
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
    /// How long an outgoing uTP connection keeps retrying its SYN before the dialer falls
    /// back to TCP. The default, 5 s, is three tries with uTP's 1 s initial timeout
    /// doubling.
    pub utp_connect_timeout: std::time::Duration,
    /// Message stream encryption for peer connections, both directions.
    pub encryption: apitorrent_mse::Policy,
    /// Forward the peer port (TCP, and UDP for uTP) and the DHT's port on the router, by
    /// NAT-PMP or UPnP. Off unless set.
    pub port_mapping: Option<apitorrent_portmap::Config>,
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
            utp_connect_timeout: std::time::Duration::from_secs(5),
            encryption: apitorrent_mse::Policy::default(),
            port_mapping: None,
        }
    }
}

/// How many piece events a slow receiver may fall behind by.
const PIECE_EVENTS: usize = 4096;

/// How the session uses a proxy (see [`Session::set_proxy`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxySettings {
    pub proxy: Proxy,
    /// Peer connections (and web seeds) go through the proxy. They are TCP only then:
    /// uTP can't go through a proxy.
    pub peers: bool,
    /// Tracker announces and scrapes go through the proxy.
    pub trackers: bool,
    /// Everything that can go through the proxy does, and everything that can't is off:
    /// no incoming connections, no DHT, no LSD, no uTP. Implies `peers` and `trackers`.
    pub force: bool,
}

impl ProxySettings {
    fn for_peers(&self) -> bool {
        self.peers || self.force
    }

    fn for_trackers(&self) -> bool {
        self.trackers || self.force
    }
}

/// Timeouts of the web seed HTTP client.
const WEB_SEED_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const WEB_SEED_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Debug, thiserror::Error)]
pub enum AddError {
    #[error("the torrent is already in the session")]
    Duplicate,
}

/// State shared between the session and its torrents.
#[derive(Debug)]
pub(crate) struct Shared {
    pub(crate) config: SessionConfig,
    /// apitorrent's own peer id, for torrents that present as apitorrent.
    pub(crate) peer_id: PeerId,
    /// Which client each torrent presents as (see [`crate::identity`]).
    identity: std::sync::RwLock<Arc<IdentityRules>>,
    /// Bumped on every change to `identity`, so torrents notice without being told.
    identity_gen: AtomicU64,
    pub(crate) tracker: TrackerClient,
    /// Sent with every announce so trackers can recognise us across IP changes.
    pub(crate) announce_key: u32,
    pub(crate) memory: MemoryBudget,
    pub(crate) filter: IpFilter,
    /// For web seeds: verified TLS, no proxies from the environment, and the configured
    /// proxy when peers go through one. Replaced when that changes.
    http: std::sync::RwLock<apitorrent_tracker::reqwest::Client>,
    /// The proxy settings, as last set.
    proxy: Mutex<Option<ProxySettings>>,
    /// Set once force proxy has closed the listener and stopped the DHT and LSD. They
    /// can't start again in this session, so it stays set until a restart.
    pub(crate) forced: tokio::sync::watch::Sender<bool>,
    pub(crate) dht: Option<apitorrent_dht::Dht>,
    pub(crate) dialer: Arc<peer::Dialer>,
    pub(crate) download: Arc<Bucket>,
    pub(crate) upload: Arc<Bucket>,
    pub(crate) connections: Arc<Counter>,
    pub(crate) half_open: Arc<Counter>,
    pub(crate) connect_rate: Bucket,
    groups: Mutex<HashMap<String, Arc<Group>>>,
    /// Each space's shared limits, by the name the core gives it.
    spaces: Mutex<HashMap<String, Arc<Group>>>,
    listen_port: u16,
    next_key: AtomicU64,
    torrents: Mutex<HashMap<Sha1Hash, mpsc::Sender<ActorMsg>>>,
    /// The wire info-hashes in `torrents`, by `HASH('req2', info_hash)`: how an incoming
    /// encrypted connection names its torrent.
    skeys: Mutex<apitorrent_mse::SkeyTable>,
    /// Wire info-hashes of torrents that aren't private: the ones LSD and DHT may announce
    /// and find peers for.
    public: Mutex<HashSet<Sha1Hash>>,
    /// Every piece a torrent verifies (see [`Session::piece_events`]).
    pub(crate) pieces: tokio::sync::broadcast::Sender<crate::torrent::PieceEvent>,
    /// Every file a torrent completes (see [`Session::file_events`]).
    pub(crate) files: tokio::sync::broadcast::Sender<crate::torrent::FileEvent>,
    /// Evidence of whether peers on the internet can reach us (see [`Session::reachability`]).
    pub(crate) reach: Arc<Reachability>,
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

    /// The identity rules, and their generation.
    pub(crate) fn identity_rules(&self) -> (Arc<IdentityRules>, u64) {
        // The generation is read under the lock, so it always belongs to these rules.
        let guard = self.identity.read().expect("identity rules poisoned");
        (guard.clone(), self.identity_gen.load(Ordering::Acquire))
    }

    pub(crate) fn identity_gen(&self) -> u64 {
        self.identity_gen.load(Ordering::Acquire)
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

    pub(crate) fn space(&self, name: &str) -> Option<Arc<Group>> {
        self.spaces
            .lock()
            .expect("spaces poisoned")
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

    /// The web seed HTTP client.
    pub(crate) fn http(&self) -> apitorrent_tracker::reqwest::Client {
        self.http
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Whether force proxy has turned off incoming connections, the DHT and LSD.
    pub(crate) fn forced(&self) -> bool {
        *self.forced.borrow()
    }

    /// The UDP port our DHT listens on, for PORT messages.
    pub(crate) fn dht_port(&self) -> Option<u16> {
        if self.forced() {
            return None;
        }
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
    /// Which families the TCP listener takes connections on: IPv4, IPv6.
    tcp_families: [bool; 2],
    port_mapper: Arc<Mutex<Option<apitorrent_portmap::PortMapper>>>,
    /// When each port check service was last asked.
    port_checks: Arc<crate::port_check::Cooldown>,
}

impl Session {
    /// Bind the listen socket and start accepting peers.
    pub async fn start(config: SessionConfig) -> io::Result<Self> {
        let (listener, utp) = bind_peer_port(&config).await?;
        let listen_addr = listener.local_addr()?;
        let tcp_families = listen_families(&listener)?;
        let version = version_digits();
        let peer_id = PeerId::azureus(*b"AT", version, rand::random());
        let tracker = TrackerClient::new().map_err(io::Error::other)?;
        let http = apitorrent_tracker::strict_http_client(
            &config.client_name,
            WEB_SEED_TIMEOUT,
            WEB_SEED_CONNECT_TIMEOUT,
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
        let reach = Arc::new(Reachability::new(std::time::Instant::now()));
        let dialer = Arc::new(peer::Dialer {
            utp: utp.clone(),
            prefer_utp: config.prefer_utp,
            encryption: std::sync::Mutex::new(config.encryption),
            reach: reach.clone(),
            proxy: std::sync::Mutex::new(None),
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
            spaces: Mutex::new(HashMap::new()),
            public: Mutex::new(HashSet::new()),
            config,
            memory,
            filter,
            http: std::sync::RwLock::new(http),
            proxy: Mutex::new(None),
            forced: tokio::sync::watch::channel(false).0,
            dht,
            dialer,
            peer_id,
            identity: std::sync::RwLock::new(Arc::new(IdentityRules::default())),
            identity_gen: AtomicU64::new(0),
            tracker,
            announce_key: rand::random(),
            listen_port: listen_addr.port(),
            next_key: AtomicU64::new(1),
            torrents: Mutex::new(HashMap::new()),
            skeys: Mutex::new(apitorrent_mse::SkeyTable::new()),
            pieces: tokio::sync::broadcast::channel(PIECE_EVENTS).0,
            files: tokio::sync::broadcast::channel(PIECE_EVENTS).0,
            reach,
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
        let port_mapper = shared.config.port_mapping.clone().map(|cfg| {
            use apitorrent_portmap::{PortMapper, Protocol, Request};
            let mut requests = vec![Request {
                protocol: Protocol::Tcp,
                port: listen_addr.port(),
            }];
            if let Some(a) = utp_addr {
                requests.push(Request {
                    protocol: Protocol::Udp,
                    port: a.port(),
                });
            }
            if let Some(port) = shared.dht_port() {
                requests.push(Request {
                    protocol: Protocol::Udp,
                    port,
                });
            }
            PortMapper::start(cfg, requests)
        });
        Ok(Self {
            shared,
            listen_addr,
            lsd_addr,
            utp_addr,
            tcp_families,
            port_mapper: Arc::new(Mutex::new(port_mapper)),
            port_checks: Arc::default(),
        })
    }

    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    /// Every piece any torrent verifies from now on: downloaded, offered, or found in an
    /// adopted file. A receiver that falls behind gets `Lagged` and should look at the
    /// torrents' bitfields instead.
    pub fn piece_events(&self) -> tokio::sync::broadcast::Receiver<crate::torrent::PieceEvent> {
        self.shared.pieces.subscribe()
    }

    /// Every file any torrent completes from now on: each piece holding its bytes is
    /// verified and written, and flushed to disk. Files complete when added aren't
    /// reported. A receiver that falls behind gets `Lagged` and should look at the
    /// torrents' bitfields instead.
    pub fn file_events(&self) -> tokio::sync::broadcast::Receiver<crate::torrent::FileEvent> {
        self.shared.files.subscribe()
    }

    /// The uTP socket's address, when uTP is on.
    pub fn utp_addr(&self) -> Option<SocketAddr> {
        self.utp_addr
    }

    /// The router's port forwards, when port mapping is on.
    pub fn port_mapping(&self) -> Option<apitorrent_portmap::Status> {
        self.port_mapper
            .lock()
            .expect("port mapper poisoned")
            .as_ref()
            .map(apitorrent_portmap::PortMapper::status)
    }

    /// Whether peers on the internet can connect to us, from what has happened so far, and
    /// what our public address is (see [`crate::reachability`]).
    pub fn reachability(&self) -> ReachabilityReport {
        let dht = self.shared.dht.as_ref();
        let utp = self.utp_addr.map(|a| a.ip().to_canonical());
        let inputs = crate::reachability::Inputs {
            peer_port: self.listen_addr.port(),
            tcp_families: self.tcp_families,
            utp_families: [
                utp.is_some_and(|ip| ip.is_ipv4()),
                utp.is_some_and(|ip| ip.is_ipv6()),
            ],
            dht_ports: [
                dht.and_then(apitorrent_dht::Dht::local_addr_v4)
                    .map(|a| a.port()),
                dht.and_then(apitorrent_dht::Dht::local_addr_v6)
                    .map(|a| a.port()),
            ],
            dht: dht.map(apitorrent_dht::Dht::stats),
            router_ip: self.port_mapping().and_then(|p| p.external_ip),
        };
        self.shared.reach.report(&inputs, std::time::Instant::now())
    }

    /// Ask `checker` to connect back to the TCP peer port, once for each family it listens
    /// on, and keep the answers as evidence (see [`crate::port_check`]). Per family (IPv4
    /// first): whether it got through, or why there's no answer; `None` where we don't
    /// listen.
    pub async fn check_port(
        &self,
        checker: crate::port_check::PortChecker,
    ) -> Result<[Option<Result<bool, String>>; 2], crate::port_check::PortCheckError> {
        use crate::port_check::PortCheckError;
        if self.shared.forced() {
            return Err(PortCheckError::ForceProxy);
        }
        if !self.tcp_families.contains(&true) {
            return Err(PortCheckError::NotListening);
        }
        self.port_checks
            .take(checker, std::time::Instant::now())
            .map_err(|wait| PortCheckError::TooSoon {
                checker,
                secs: wait.as_secs().max(1),
            })?;
        let port = self.listen_addr.port();
        let name = &self.shared.config.client_name;
        let ask = |family: usize| async move {
            if !self.tcp_families[family] {
                return None;
            }
            let answer = crate::port_check::check(checker, port, family, name).await;
            match &answer {
                Ok(open) => {
                    self.shared
                        .reach
                        .checked(family, checker, *open, std::time::Instant::now());
                }
                Err(e) => {
                    tracing::info!(checker = checker.host(), family, error = %e, "port check failed");
                }
            }
            Some(answer)
        };
        let (v4, v6) = tokio::join!(ask(0), ask(1));
        Ok([v4, v6])
    }

    /// Remove the router's port forwards, before shutting down.
    pub async fn stop_port_mapping(&self) {
        let mapper = self
            .port_mapper
            .lock()
            .expect("port mapper poisoned")
            .take();
        if let Some(m) = mapper {
            m.stop().await;
        }
    }

    /// The LSD socket's address, when LSD is on.
    pub fn lsd_addr(&self) -> Option<SocketAddr> {
        self.lsd_addr
    }

    /// apitorrent's own peer id. Torrents that present as another client (see
    /// [`Session::set_identity_rules`]) use their own.
    pub fn peer_id(&self) -> PeerId {
        self.shared.peer_id
    }

    /// Choose which client each torrent presents as. Torrents pick it up within a second:
    /// a torrent whose identity changes tells its trackers `stopped` under the old one, then
    /// `started` under the new one with a new peer id and key; connections already open
    /// keep the old peer id.
    pub fn set_identity_rules(&self, rules: IdentityRules) {
        let mut current = self
            .shared
            .identity
            .write()
            .expect("identity rules poisoned");
        if **current == rules {
            return;
        }
        *current = Arc::new(rules);
        self.shared.identity_gen.fetch_add(1, Ordering::AcqRel);
    }

    /// The identity rules in force.
    pub fn identity_rules(&self) -> Arc<IdentityRules> {
        self.shared.identity_rules().0
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

    /// Use a proxy, or stop using one (`None`). New connections follow the change at once;
    /// open ones keep the path they have.
    ///
    /// Force proxy also closes the listen socket, shuts the DHT down, stops LSD, removes
    /// the router's port forwards, and refuses incoming uTP connections. Those can't start
    /// again while the session runs: turning force proxy off brings them back on the next
    /// start.
    pub fn set_proxy(&self, settings: Option<ProxySettings>) -> io::Result<()> {
        let shared = &self.shared;
        let peers = settings
            .as_ref()
            .filter(|p| p.for_peers())
            .map(|p| p.proxy.clone());
        let trackers = settings
            .as_ref()
            .filter(|p| p.for_trackers())
            .map(|p| p.proxy.clone());
        let http = apitorrent_tracker::strict_http_client_via(
            &shared.config.client_name,
            WEB_SEED_TIMEOUT,
            WEB_SEED_CONNECT_TIMEOUT,
            peers.as_ref(),
        )
        .map_err(io::Error::other)?;
        shared
            .tracker
            .set_proxy(trackers)
            .map_err(io::Error::other)?;
        *shared
            .http
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = http;
        shared.dialer.set_proxy(peers.map(Arc::new));
        let force = settings.as_ref().is_some_and(|p| p.force);
        if force && !shared.forced() {
            shared.forced.send_replace(true);
            if let Some(dht) = shared.dht.clone() {
                tokio::spawn(async move { dht.shutdown().await });
            }
            // Asking the router for forwards goes around the proxy, and nothing can come in.
            let mapper = self
                .port_mapper
                .lock()
                .expect("port mapper poisoned")
                .take();
            if let Some(m) = mapper {
                tokio::spawn(m.stop());
            }
            tracing::info!(
                "force proxy: incoming connections, DHT, LSD, uTP and port forwarding are off"
            );
        } else if !force && shared.forced() {
            tracing::info!(
                "force proxy is off: incoming connections, DHT, LSD, uTP and port forwarding come back on the \
                 next start"
            );
        }
        *shared.proxy.lock().expect("proxy settings poisoned") = settings;
        Ok(())
    }

    /// The proxy settings, as last set.
    pub fn proxy(&self) -> Option<ProxySettings> {
        self.shared
            .proxy
            .lock()
            .expect("proxy settings poisoned")
            .clone()
    }

    /// Whether force proxy has turned off incoming connections, the DHT and LSD in this
    /// session (they stay off until a restart, even after force proxy is turned off).
    pub fn forced(&self) -> bool {
        self.shared.forced()
    }

    /// Change the encryption policy. New connections use it; open ones keep theirs.
    pub fn set_encryption(&self, policy: apitorrent_mse::Policy) {
        self.shared.dialer.set_encryption(policy);
    }

    /// The encryption policy new connections use.
    pub fn encryption(&self) -> apitorrent_mse::Policy {
        self.shared.dialer.encryption()
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

    /// Create a space's limits, or change them. Every entry of the space draws from them
    /// (see [`crate::EntryParams::space`]), across all of its torrents.
    pub fn set_space(&self, name: &str, limits: GroupLimits) {
        let mut spaces = self.shared.spaces.lock().expect("spaces poisoned");
        match spaces.get(name) {
            Some(g) => g.set(limits),
            None => {
                spaces.insert(name.to_owned(), Group::new(limits));
            }
        }
    }

    /// Remove a space's limits. Entries already in it keep them until they go.
    pub fn remove_space(&self, name: &str) -> bool {
        self.shared
            .spaces
            .lock()
            .expect("spaces poisoned")
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

/// Wait until force proxy turns off what can't go through the proxy (or the session is
/// gone).
pub(crate) async fn until_forced(forced: &mut tokio::sync::watch::Receiver<bool>) {
    // The guard `wait_for` returns isn't `Send`; drop it here, not across an await.
    let _ = forced.wait_for(|f| *f).await.map(|_| ());
}

async fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    let mut forced = shared.forced.subscribe();
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            // Force proxy: no incoming connections. Dropping the listener closes the port.
            () = until_forced(&mut forced) => {
                tracing::debug!("listener closed: force proxy");
                return;
            }
        };
        match accepted {
            Ok((stream, addr)) => {
                if shared.filter.is_blocked(addr.ip()) {
                    tracing::debug!(%addr, "refused a blocked peer");
                    continue;
                }
                let _ = stream.set_nodelay(true);
                // We dialed our own address (trackers sometimes list us): it proves nothing
                // about the way in from outside.
                let from_self = stream.local_addr().is_ok_and(|l| l.ip() == addr.ip());
                tokio::spawn(route_incoming(
                    Box::new(stream),
                    false,
                    from_self,
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

/// Which families a listener takes connections on: IPv4, IPv6. An IPv6 wildcard takes
/// IPv4 too unless the socket is IPv6-only (the default on Windows, and on Linux with
/// `net.ipv6.bindv6only`).
fn listen_families(listener: &TcpListener) -> io::Result<[bool; 2]> {
    let addr = listener.local_addr()?;
    Ok(match addr.ip() {
        std::net::IpAddr::V6(ip) if ip.to_ipv4_mapped().is_some() => [true, false],
        std::net::IpAddr::V6(ip) if ip.is_unspecified() => {
            [!socket2::SockRef::from(listener).only_v6()?, true]
        }
        std::net::IpAddr::V6(_) => [false, true],
        std::net::IpAddr::V4(_) => [true, false],
    })
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
        connect_timeout: config.utp_connect_timeout,
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

async fn utp_accept_loop(socket: apitorrent_utp::UtpSocket, shared: Arc<Shared>) {
    loop {
        match socket.accept().await {
            Ok((stream, addr)) => {
                if shared.filter.is_blocked(addr.ip()) {
                    tracing::debug!(%addr, "refused a blocked peer");
                    continue;
                }
                // The uTP socket can't be closed while it runs; under force proxy its
                // connections are dropped as they arrive, until the next start.
                if shared.forced() {
                    tracing::debug!(%addr, "refused an incoming uTP peer: force proxy");
                    continue;
                }
                tokio::spawn(route_incoming(
                    Box::new(stream),
                    true,
                    false,
                    addr,
                    shared.clone(),
                ));
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
/// and hand the connection to the torrent it names. `from_self`: the connection comes from
/// one of this machine's own addresses.
async fn route_incoming(
    stream: peer::BoxIo,
    utp: bool,
    from_self: bool,
    addr: SocketAddr,
    shared: Arc<Shared>,
) {
    let policy = shared.dialer.encryption();
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
    // A peer got all the way here on its own, whichever torrent it asks for.
    if !from_self {
        shared.reach.incoming(utp, addr, std::time::Instant::now());
    }
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
        if shared.forced() {
            return;
        }
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
                // The lookup is over and the announces are sent.
                if let Some(shared) = weak.upgrade() {
                    shared.reach.announced_on_dht(std::time::Instant::now());
                }
            });
        }
    }
}

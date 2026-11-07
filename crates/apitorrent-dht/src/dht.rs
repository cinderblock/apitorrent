//! The tokio driver: sockets, a command channel, and a loop around the sans-IO [`Node`].

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::Stream;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, trace, warn};

use crate::id::NodeId;
use crate::node::{AnnounceSpec, DhtStats, Event, LookupId, Node, NodeConfig, ScrapeEstimate};
use crate::state::PersistedState;

/// The public bootstrap routers used when [`DhtConfig::bootstrap`] is left at its default.
pub const DEFAULT_BOOTSTRAP: &[&str] = &[
    "router.bittorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "router.utorrent.com:6881",
];

/// How long to wait for one bootstrap host name to resolve.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);
/// Peers buffered per lookup stream before new ones are dropped.
const STREAM_BUFFER: usize = 1024;
/// How often the stats snapshot behind [`Dht::stats`] is refreshed.
const STATS_EVERY: Duration = Duration::from_millis(250);

/// Settings for [`Dht::start`].
#[derive(Debug, Clone)]
pub struct DhtConfig {
    /// The IPv4 address to bind, or `None` for no IPv4 DHT.
    pub bind_v4: Option<SocketAddr>,
    /// The IPv6 address to bind, or `None` for no IPv6 DHT. The socket is IPv6-only, so it
    /// can share a port number with the IPv4 socket. If IPv6 is unavailable on the host,
    /// the DHT runs IPv4-only (unless there is no IPv4 socket either).
    pub bind_v6: Option<SocketAddr>,
    /// Bootstrap nodes as `host:port` (names are resolved in the background).
    pub bootstrap: Vec<String>,
    /// State saved by [`Dht::export_state`] in an earlier run.
    pub state: Option<PersistedState>,
    /// Protocol settings: read-only mode, BEP 42 behaviour, rate limits, store caps.
    /// `ipv4` and `ipv6` are set from the sockets that could be bound.
    pub node: NodeConfig,
}

impl Default for DhtConfig {
    fn default() -> Self {
        Self {
            bind_v4: Some(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 6881))),
            bind_v6: Some(SocketAddr::from((Ipv6Addr::UNSPECIFIED, 6881))),
            bootstrap: DEFAULT_BOOTSTRAP.iter().map(|s| (*s).to_owned()).collect(),
            state: None,
            node: NodeConfig::default(),
        }
    }
}

/// Why the DHT could not start, or no longer answers.
#[derive(Debug, thiserror::Error)]
pub enum DhtError {
    #[error("could not bind the DHT socket on {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: io::Error,
    },
    #[error("no DHT socket is configured")]
    NoSocket,
    #[error("the DHT has stopped")]
    Stopped,
}

enum Command {
    GetPeers {
        info_hash: NodeId,
        announce: Option<AnnounceSpec>,
        peers: mpsc::Sender<SocketAddr>,
    },
    Scrape {
        info_hash: NodeId,
        reply: oneshot::Sender<Option<ScrapeEstimate>>,
    },
    AddNode(SocketAddr),
    Bootstrap(Vec<SocketAddr>),
    Export(oneshot::Sender<PersistedState>),
    Shutdown,
}

#[derive(Debug)]
struct Shared {
    commands: mpsc::UnboundedSender<Command>,
    stats: watch::Receiver<DhtStats>,
    task: Mutex<Option<JoinHandle<()>>>,
    local_v4: Option<SocketAddr>,
    local_v6: Option<SocketAddr>,
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Command::GetPeers { .. } => "GetPeers",
            Command::Scrape { .. } => "Scrape",
            Command::AddNode(_) => "AddNode",
            Command::Bootstrap(_) => "Bootstrap",
            Command::Export(_) => "Export",
            Command::Shutdown => "Shutdown",
        })
    }
}

/// A running DHT node. Cheap to clone; every clone drives the same node. The node stops
/// when [`Dht::shutdown`] is called or the last clone is dropped.
#[derive(Debug, Clone)]
pub struct Dht {
    shared: Arc<Shared>,
}

/// Peers for one info-hash, each reported once, as the lookup finds them. The stream ends
/// when the lookup finishes.
///
/// Dropping a stream from [`Dht::get_peers`] cancels its lookup. Dropping one from
/// [`Dht::announce`] does not: the lookup and the announce still complete.
#[derive(Debug)]
pub struct PeerStream {
    rx: mpsc::Receiver<SocketAddr>,
}

impl Stream for PeerStream {
    type Item = SocketAddr;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<SocketAddr>> {
        self.rx.poll_recv(cx)
    }
}

impl Dht {
    /// Bind the sockets and start the node. Bootstrapping runs in the background.
    pub async fn start(config: DhtConfig) -> Result<Dht, DhtError> {
        let v4 = match config.bind_v4 {
            Some(addr) => Some(
                UdpSocket::bind(addr)
                    .await
                    .map_err(|source| DhtError::Bind { addr, source })?,
            ),
            None => None,
        };
        let v6 = match config.bind_v6 {
            Some(addr) => match bind_v6_only(addr) {
                Ok(s) => Some(s),
                Err(source) if v4.is_some() => {
                    warn!(%addr, error = %source, "IPv6 DHT unavailable; running IPv4 only");
                    None
                }
                Err(source) => return Err(DhtError::Bind { addr, source }),
            },
            None => None,
        };
        if v4.is_none() && v6.is_none() {
            return Err(DhtError::NoSocket);
        }
        let local_v4 = v4.as_ref().and_then(|s| s.local_addr().ok());
        let local_v6 = v6.as_ref().and_then(|s| s.local_addr().ok());

        let mut node_cfg = config.node;
        node_cfg.ipv4 = v4.is_some();
        node_cfg.ipv6 = v6.is_some();
        let now = Instant::now();
        let mut node = Node::new(node_cfg, config.state.as_ref(), now);
        // Starts from saved nodes straight away; routers join once resolved.
        node.add_bootstrap([], now);
        info!(id = %node.id(), ?local_v4, ?local_v6, "DHT started");

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (stats_tx, stats_rx) = watch::channel(node.stats(now));

        if !config.bootstrap.is_empty() {
            tokio::spawn(resolve_bootstrap(config.bootstrap, cmd_tx.downgrade()));
        }

        let driver = Driver {
            node,
            v4,
            v6,
            commands: cmd_rx,
            subs: HashMap::new(),
            stats: stats_tx,
            last_stats: now,
        };
        let task = tokio::spawn(driver.run());
        Ok(Dht {
            shared: Arc::new(Shared {
                commands: cmd_tx,
                stats: stats_rx,
                task: Mutex::new(Some(task)),
                local_v4,
                local_v6,
            }),
        })
    }

    /// The bound IPv4 address, if the IPv4 DHT runs.
    pub fn local_addr_v4(&self) -> Option<SocketAddr> {
        self.shared.local_v4
    }

    /// The bound IPv6 address, if the IPv6 DHT runs.
    pub fn local_addr_v6(&self) -> Option<SocketAddr> {
        self.shared.local_v6
    }

    fn lookup(&self, info_hash: [u8; 20], announce: Option<AnnounceSpec>) -> PeerStream {
        let (tx, rx) = mpsc::channel(STREAM_BUFFER);
        // If the DHT has stopped, the command is dropped with its sender and the stream
        // ends at once.
        let _ = self.shared.commands.send(Command::GetPeers {
            info_hash: NodeId(info_hash),
            announce,
            peers: tx,
        });
        PeerStream { rx }
    }

    /// Find peers for `info_hash`, over IPv4 and IPv6.
    pub fn get_peers(&self, info_hash: [u8; 20]) -> PeerStream {
        self.lookup(info_hash, None)
    }

    /// Find peers for `info_hash` and announce that we are one. `port` is the port peers
    /// should connect to; `None` asks nodes to use the DHT socket's port (`implied_port`).
    pub fn announce(&self, info_hash: [u8; 20], port: Option<u16>) -> PeerStream {
        self.announce_with(info_hash, AnnounceSpec { port, seed: false })
    }

    /// [`Dht::announce`] with every option, including the BEP 33 seed flag.
    pub fn announce_with(&self, info_hash: [u8; 20], spec: AnnounceSpec) -> PeerStream {
        self.lookup(info_hash, Some(spec))
    }

    /// Estimate a swarm's size with BEP 33 bloom filters. `None` when no node on the path
    /// supports BEP 33.
    pub async fn scrape(&self, info_hash: [u8; 20]) -> Result<Option<ScrapeEstimate>, DhtError> {
        let (tx, rx) = oneshot::channel();
        self.shared
            .commands
            .send(Command::Scrape {
                info_hash: NodeId(info_hash),
                reply: tx,
            })
            .map_err(|_| DhtError::Stopped)?;
        rx.await.map_err(|_| DhtError::Stopped)
    }

    /// Ping a node, as when a peer advertises its DHT port. It joins the routing table if
    /// it answers.
    pub fn add_node(&self, addr: SocketAddr) {
        let _ = self.shared.commands.send(Command::AddNode(addr));
    }

    /// The latest counters, refreshed about four times a second.
    pub fn stats(&self) -> DhtStats {
        self.shared.stats.borrow().clone()
    }

    /// The node ID and good nodes, as bytes for [`PersistedState::from_bytes`].
    pub async fn export_state(&self) -> Result<Vec<u8>, DhtError> {
        let (tx, rx) = oneshot::channel();
        self.shared
            .commands
            .send(Command::Export(tx))
            .map_err(|_| DhtError::Stopped)?;
        Ok(rx.await.map_err(|_| DhtError::Stopped)?.to_bytes())
    }

    /// Stop the node and wait for it to finish. Open streams end. Safe to call twice.
    pub async fn shutdown(&self) {
        let _ = self.shared.commands.send(Command::Shutdown);
        let task = self
            .shared
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

/// Bind an IPv6-only UDP socket, so the IPv4 socket can use the same port number.
fn bind_v6_only(addr: SocketAddr) -> io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_only_v6(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    UdpSocket::from_std(socket.into())
}

async fn resolve_bootstrap(hosts: Vec<String>, commands: mpsc::WeakUnboundedSender<Command>) {
    for host in hosts {
        let addrs: Vec<SocketAddr> = if let Ok(addr) = host.parse::<SocketAddr>() {
            vec![addr]
        } else {
            match tokio::time::timeout(RESOLVE_TIMEOUT, tokio::net::lookup_host(host.as_str()))
                .await
            {
                Ok(Ok(it)) => it.collect(),
                Ok(Err(e)) => {
                    debug!(%host, error = %e, "could not resolve DHT bootstrap host");
                    continue;
                }
                Err(_) => {
                    debug!(%host, "timed out resolving DHT bootstrap host");
                    continue;
                }
            }
        };
        let Some(tx) = commands.upgrade() else {
            return;
        };
        if tx.send(Command::Bootstrap(addrs)).is_err() {
            return;
        }
    }
}

struct Subscriber {
    peers: Option<mpsc::Sender<SocketAddr>>,
    scrape: Option<oneshot::Sender<Option<ScrapeEstimate>>>,
    /// An announce finishes even if nobody is listening.
    keep_running: bool,
}

struct Driver {
    node: Node,
    v4: Option<UdpSocket>,
    v6: Option<UdpSocket>,
    commands: mpsc::UnboundedReceiver<Command>,
    subs: HashMap<LookupId, Subscriber>,
    stats: watch::Sender<DhtStats>,
    last_stats: Instant,
}

async fn recv(socket: Option<&UdpSocket>, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
    match socket {
        Some(s) => s.recv_from(buf).await,
        None => std::future::pending().await,
    }
}

impl Driver {
    async fn run(mut self) {
        let mut buf4 = vec![0u8; 65_536];
        let mut buf6 = vec![0u8; 65_536];
        loop {
            self.flush();
            let now = Instant::now();
            if now.saturating_duration_since(self.last_stats) >= STATS_EVERY {
                self.publish_stats(now);
            }
            let deadline = self.node.poll_timeout(now);
            tokio::select! {
                r = recv(self.v4.as_ref(), &mut buf4) => self.received(r, &buf4),
                r = recv(self.v6.as_ref(), &mut buf6) => self.received(r, &buf6),
                cmd = self.commands.recv() => match cmd {
                    None | Some(Command::Shutdown) => break,
                    Some(cmd) => self.command(cmd),
                },
                () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {}
            }
            // Timers are handled whatever woke us, so a busy socket can't starve them.
            let now = Instant::now();
            if now >= deadline {
                self.node.handle_timeout(now);
            }
        }
        self.flush();
        debug!("DHT stopped");
    }

    fn received(&mut self, r: io::Result<(usize, SocketAddr)>, buf: &[u8]) {
        match r {
            Ok((n, from)) => self.node.handle_datagram(from, &buf[..n], Instant::now()),
            // Windows reports an ICMP port-unreachable for an earlier send as a receive
            // error (WSAECONNRESET). It says nothing about this socket; carry on.
            Err(e) => trace!(error = %e, "DHT receive error"),
        }
    }

    fn command(&mut self, cmd: Command) {
        let now = Instant::now();
        match cmd {
            Command::GetPeers {
                info_hash,
                announce,
                peers,
            } => {
                let lid = self.node.get_peers(info_hash, announce, false, now);
                self.subs.insert(
                    lid,
                    Subscriber {
                        peers: Some(peers),
                        scrape: None,
                        keep_running: announce.is_some(),
                    },
                );
            }
            Command::Scrape { info_hash, reply } => {
                let lid = self.node.get_peers(info_hash, None, true, now);
                self.subs.insert(
                    lid,
                    Subscriber {
                        peers: None,
                        scrape: Some(reply),
                        keep_running: false,
                    },
                );
            }
            Command::AddNode(addr) => self.node.ping(addr, now),
            Command::Bootstrap(addrs) => self.node.add_bootstrap(addrs, now),
            Command::Export(reply) => {
                let _ = reply.send(self.node.export_state(now));
            }
            Command::Shutdown => {}
        }
    }

    fn flush(&mut self) {
        while let Some(t) = self.node.poll_transmit() {
            let socket = if t.to.is_ipv6() { &self.v6 } else { &self.v4 };
            if let Some(s) = socket
                && let Err(e) = s.try_send_to(&t.data, t.to)
            {
                // UDP is lossy anyway: a full send buffer drops the datagram.
                trace!(to = %t.to, error = %e, "DHT send failed");
            }
        }
        while let Some(ev) = self.node.poll_event() {
            match ev {
                Event::Peers { lookup, peers } => self.deliver(lookup, &peers),
                Event::LookupDone { lookup, outcome } => {
                    if let Some(sub) = self.subs.remove(&lookup)
                        && let Some(reply) = sub.scrape
                    {
                        let _ = reply.send(outcome.scrape);
                    }
                }
                Event::IdChanged { id } => info!(%id, "DHT node ID changed (BEP 42)"),
            }
        }
    }

    fn deliver(&mut self, lookup: LookupId, peers: &[SocketAddr]) {
        let Some(sub) = self.subs.get_mut(&lookup) else {
            return;
        };
        let Some(tx) = &sub.peers else {
            return;
        };
        for p in peers {
            match tx.try_send(*p) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    trace!("peer stream full; dropping a peer");
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    sub.peers = None;
                    if !sub.keep_running && sub.scrape.is_none() {
                        self.subs.remove(&lookup);
                        self.node.cancel(lookup);
                    }
                    return;
                }
            }
        }
    }

    fn publish_stats(&mut self, now: Instant) {
        self.last_stats = now;
        // Cancel lookups whose listener went away without new peers to notice it by.
        let gone: Vec<LookupId> = self
            .subs
            .iter()
            .filter(|(_, s)| {
                !s.keep_running
                    && s.scrape.as_ref().is_none_or(oneshot::Sender::is_closed)
                    && s.peers.as_ref().is_none_or(mpsc::Sender::is_closed)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            self.subs.remove(&id);
            self.node.cancel(id);
        }
        self.stats.send_replace(self.node.stats(now));
    }
}

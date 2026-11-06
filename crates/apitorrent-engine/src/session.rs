//! The session: our identity on the network, the listen socket, and the set of torrents.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use apitorrent_metainfo::{InfoHashes, Sha1Hash};
use apitorrent_wire::PeerId;

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
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 6881)),
            max_peers_per_torrent: 60,
            request_queue: 64,
            upload_slots: 8,
            client_name: concat!("apitorrent ", env!("CARGO_PKG_VERSION")).to_owned(),
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
    listen_port: u16,
    next_key: AtomicU64,
    torrents: Mutex<HashMap<Sha1Hash, mpsc::Sender<ActorMsg>>>,
}

impl Shared {
    pub(crate) fn next_key(&self) -> u64 {
        self.next_key.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn listen_port(&self) -> u16 {
        self.listen_port
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
        let shared = Arc::new(Shared {
            config,
            peer_id,
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

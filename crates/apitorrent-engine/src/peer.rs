//! Peer connection tasks: socket I/O only.
//!
//! A connection does the handshake, then shuttles messages between the socket and its
//! torrent's actor. Every protocol decision is made by the actor.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use apitorrent_metainfo::Sha1Hash;
use apitorrent_mse::{MseConfig, MseStream, Policy};
use apitorrent_utp::UtpSocket;
use apitorrent_wire::{Decoder, HANDSHAKE_LEN, Handshake, Message, PeerId, Reserved};

use crate::limits::{Direction, Throttle};
use crate::picker::PeerKey;

pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// A peer that sends nothing (not even a keep-alive) for this long is dropped.
const IDLE_TIMEOUT: Duration = Duration::from_secs(150);
/// We send a keep-alive after this long without sending anything.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(60);

/// From a connection to its torrent.
#[derive(Debug)]
pub(crate) enum PeerEvent {
    /// The handshake is done and the connection is ready for messages.
    Ready {
        reserved: Reserved,
        peer_id: PeerId,
        link: Link,
        tx: mpsc::UnboundedSender<PeerCmd>,
    },
    Message(Message),
    /// The connection is gone. Always the last event for a key.
    Closed {
        reason: String,
    },
}

/// From a torrent to a connection.
#[derive(Debug)]
pub(crate) enum PeerCmd {
    Send(Message),
    Close,
}

pub(crate) type EventTx = mpsc::Sender<(PeerKey, PeerEvent)>;

/// Any byte stream a peer connection can run over: TCP, uTP, or either wrapped in
/// encryption.
pub(crate) trait PeerIo: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> PeerIo for T {}

/// A connection's stream, whatever it is underneath.
pub(crate) type BoxIo = Box<dyn PeerIo>;

/// What a connection runs over, for the peer list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Link {
    /// uTP rather than TCP.
    pub(crate) utp: bool,
    /// The payload is RC4-encrypted (MSE). An MSE connection that settled on plaintext
    /// only obfuscated its handshake, so it doesn't count.
    pub(crate) encrypted: bool,
}

/// How outgoing connections are made: which transports, in which order, and whether to
/// encrypt.
#[derive(Debug)]
pub(crate) struct Dialer {
    /// The uTP socket on the peer port, when uTP is on.
    pub(crate) utp: Option<UtpSocket>,
    /// Try uTP before TCP. Either way the other is the fallback.
    pub(crate) prefer_utp: bool,
    /// Changeable while running; connections already open keep what they settled on.
    pub(crate) encryption: std::sync::Mutex<Policy>,
}

impl Dialer {
    pub(crate) fn encryption(&self) -> Policy {
        *self
            .encryption
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn set_encryption(&self, policy: Policy) {
        *self
            .encryption
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = policy;
    }

    /// The transports to try for `addr`, in order: `true` is uTP.
    fn order(&self, addr: SocketAddr) -> &'static [bool] {
        let utp = self
            .utp
            .as_ref()
            .is_some_and(|s| reaches(s.local_addr(), addr));
        match (utp, self.prefer_utp) {
            (false, _) => &[false],
            (true, true) => &[true, false],
            (true, false) => &[false, true],
        }
    }

    /// Open the transport alone.
    async fn open(&self, addr: SocketAddr, utp: bool) -> Result<BoxIo, String> {
        if utp {
            let socket = self.utp.as_ref().ok_or("uTP is off")?;
            let stream = tokio::time::timeout(CONNECT_TIMEOUT, socket.connect(addr))
                .await
                .map_err(|_| "connect timed out".to_owned())?
                .map_err(|e| format!("connect failed: {e}"))?;
            Ok(Box::new(stream))
        } else {
            let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
                .await
                .map_err(|_| "connect timed out".to_owned())?
                .map_err(|e| format!("connect failed: {e}"))?;
            let _ = stream.set_nodelay(true);
            Ok(Box::new(stream))
        }
    }

    /// Connect over one transport and exchange handshakes: through MSE when the policy
    /// says so, and again in plain BitTorrent when that fails and the policy allows it
    /// (some peers close a connection they can't read).
    async fn handshake_over(
        &self,
        addr: SocketAddr,
        ours: &Handshake,
        utp: bool,
    ) -> Result<(BoxIo, Handshake, Link), String> {
        let policy = self.encryption();
        let mut io = self.open(addr, utp).await?;
        if policy.initiates_mse() {
            let config = MseConfig::new(policy);
            match MseStream::connect(io, ours.info_hash, &ours.encode(), config).await {
                Ok(mut stream) => {
                    let theirs = read_handshake(&mut stream).await?;
                    let link = Link {
                        utp,
                        encrypted: stream.encryption().is_rc4(),
                    };
                    return Ok((Box::new(stream), theirs, link));
                }
                Err(e) if !policy.retry_plaintext() => {
                    return Err(format!("encryption handshake failed: {e}"));
                }
                Err(e) => {
                    tracing::trace!(%addr, error = %e, "encryption failed, retrying in plaintext");
                    io = self.open(addr, utp).await?;
                }
            }
        }
        let sent = async {
            io.write_all(&ours.encode()).await?;
            io.flush().await
        };
        sent.await
            .map_err(|e| format!("handshake write failed: {e}"))?;
        let theirs = read_handshake(&mut io).await?;
        Ok((
            io,
            theirs,
            Link {
                utp,
                encrypted: false,
            },
        ))
    }

    /// Connect to `addr` and exchange handshakes, over each transport in turn until one
    /// works.
    async fn dial(
        &self,
        addr: SocketAddr,
        ours: &Handshake,
    ) -> Result<(BoxIo, Handshake, Link), String> {
        let mut errors = Vec::new();
        for &utp in self.order(addr) {
            match self.handshake_over(addr, ours, utp).await {
                Ok(done) => return Ok(done),
                Err(e) => errors.push(format!("{}: {e}", if utp { "uTP" } else { "TCP" })),
            }
        }
        Err(errors.join("; "))
    }
}

/// Whether a UDP socket bound to `local` can send to `remote`: the same address family,
/// and not from loopback to elsewhere. (TCP has no such limit, since outgoing TCP doesn't
/// use the listen socket.)
fn reaches(local: SocketAddr, remote: SocketAddr) -> bool {
    local.is_ipv4() == remote.is_ipv4() && (!local.ip().is_loopback() || remote.ip().is_loopback())
}

/// Connect to `addr`, handshake, and run the connection until it closes.
pub(crate) async fn run_outbound(
    addr: SocketAddr,
    ours: Handshake,
    key: PeerKey,
    max_message: u32,
    events: EventTx,
    throttle: Arc<Throttle>,
    dialer: Arc<Dialer>,
) {
    let result = async {
        let (stream, theirs, link) = dialer.dial(addr, &ours).await?;
        if theirs.info_hash != ours.info_hash {
            return Err("peer answered for a different torrent".to_owned());
        }
        if theirs.peer_id == ours.peer_id {
            return Err("connected to ourselves".to_owned());
        }
        Ok((stream, theirs, link))
    }
    .await;
    match result {
        Ok((stream, theirs, link)) => {
            run_connected(
                stream,
                theirs.reserved,
                theirs.peer_id,
                link,
                key,
                max_message,
                events,
                throttle,
            )
            .await;
        }
        Err(reason) => {
            let _ = events.send((key, PeerEvent::Closed { reason })).await;
        }
    }
}

/// Run an accepted connection whose handshake has already been read; ours is sent here.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_inbound(
    mut stream: BoxIo,
    theirs: Handshake,
    link: Link,
    ours: Handshake,
    key: PeerKey,
    max_message: u32,
    events: EventTx,
    throttle: Arc<Throttle>,
) {
    if theirs.peer_id == ours.peer_id {
        let reason = "connected to ourselves".to_owned();
        let _ = events.send((key, PeerEvent::Closed { reason })).await;
        return;
    }
    let sent = async {
        stream.write_all(&ours.encode()).await?;
        stream.flush().await
    };
    if let Err(e) = sent.await {
        let reason = format!("handshake write failed: {e}");
        let _ = events.send((key, PeerEvent::Closed { reason })).await;
        return;
    }
    run_connected(
        stream,
        theirs.reserved,
        theirs.peer_id,
        link,
        key,
        max_message,
        events,
        throttle,
    )
    .await;
}

/// Read a full handshake, with a timeout.
pub(crate) async fn read_handshake<S: AsyncRead + Unpin + ?Sized>(
    stream: &mut S,
) -> Result<Handshake, String> {
    let mut buf = [0u8; HANDSHAKE_LEN];
    tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.read_exact(&mut buf))
        .await
        .map_err(|_| "handshake timed out".to_owned())?
        .map_err(|e| format!("handshake read failed: {e}"))?;
    Handshake::decode(&buf).map_err(|e| format!("bad handshake: {e}"))
}

#[allow(clippy::too_many_arguments)]
async fn run_connected(
    stream: BoxIo,
    reserved: Reserved,
    peer_id: PeerId,
    link: Link,
    key: PeerKey,
    max_message: u32,
    events: EventTx,
    throttle: Arc<Throttle>,
) {
    let (read, write) = tokio::io::split(stream);
    let (tx, rx) = mpsc::unbounded_channel();
    if events
        .send((
            key,
            PeerEvent::Ready {
                reserved,
                peer_id,
                link,
                tx,
            },
        ))
        .await
        .is_err()
    {
        return;
    }
    let mut writer = tokio::spawn(write_loop(write, rx, throttle.clone()));
    let reader = read_loop(read, key, max_message, &events, &throttle);
    let reason = tokio::select! {
        r = reader => r,
        w = &mut writer => match w {
            Ok(Ok(())) => "closed by us".to_owned(),
            Ok(Err(e)) => e,
            Err(e) => format!("writer task failed: {e}"),
        },
    };
    writer.abort();
    let _ = events.send((key, PeerEvent::Closed { reason })).await;
}

async fn read_loop(
    mut read: ReadHalf<BoxIo>,
    key: PeerKey,
    max_message: u32,
    events: &EventTx,
    throttle: &Throttle,
) -> String {
    let mut decoder = Decoder::new(max_message);
    loop {
        loop {
            match decoder.next() {
                Ok(Some(m)) => {
                    if events.send((key, PeerEvent::Message(m))).await.is_err() {
                        return "torrent stopped".to_owned();
                    }
                }
                Ok(None) => break,
                Err(e) => return format!("protocol error: {e}"),
            }
        }
        let buf = decoder.buffer();
        if buf.capacity() - buf.len() < 16 * 1024 {
            buf.reserve(64 * 1024);
        }
        match tokio::time::timeout(IDLE_TIMEOUT, read.read_buf(buf)).await {
            Err(_) => return "peer went silent".to_owned(),
            Ok(Ok(0)) => return "closed by peer".to_owned(),
            Ok(Ok(n)) => throttle.take(Direction::Download, n as u64).await,
            Ok(Err(e)) => return format!("read failed: {e}"),
        }
    }
}

async fn write_loop(
    mut write: WriteHalf<BoxIo>,
    mut rx: mpsc::UnboundedReceiver<PeerCmd>,
    throttle: Arc<Throttle>,
) -> Result<(), String> {
    let mut out = BytesMut::with_capacity(64 * 1024);
    loop {
        let cmd = match tokio::time::timeout(KEEPALIVE_INTERVAL, rx.recv()).await {
            Err(_) => PeerCmd::Send(Message::KeepAlive),
            Ok(None | Some(PeerCmd::Close)) => {
                let _ = write.shutdown().await;
                return Ok(());
            }
            Ok(Some(cmd)) => cmd,
        };
        let PeerCmd::Send(m) = cmd else {
            unreachable!()
        };
        m.encode(&mut out);
        // Batch whatever else is already queued into the same write.
        while out.len() < 256 * 1024 {
            match rx.try_recv() {
                Ok(PeerCmd::Send(m)) => m.encode(&mut out),
                Ok(PeerCmd::Close) => {
                    write
                        .write_all(&out)
                        .await
                        .map_err(|e| format!("write failed: {e}"))?;
                    let _ = write.shutdown().await;
                    return Ok(());
                }
                Err(_) => break,
            }
        }
        throttle.take(Direction::Upload, out.len() as u64).await;
        write
            .write_all(&out)
            .await
            .map_err(|e| format!("write failed: {e}"))?;
        // An encrypted stream holds the tail of a write it couldn't send at once until
        // it is flushed; plain TCP and uTP have nothing to flush.
        write
            .flush()
            .await
            .map_err(|e| format!("write failed: {e}"))?;
        out.clear();
    }
}

/// The handshake we send for a torrent.
pub(crate) fn our_handshake(
    info_hash: Sha1Hash,
    peer_id: PeerId,
    v2: bool,
    dht: bool,
) -> Handshake {
    Handshake {
        reserved: Reserved::default()
            .with_extension_protocol(true)
            .with_fast(true)
            .with_dht(dht)
            .with_v2(v2),
        info_hash,
        peer_id,
    }
}

#[cfg(test)]
mod tests {
    use super::reaches;

    #[test]
    fn utp_reach() {
        let any4 = "0.0.0.0:6881".parse().unwrap();
        let lo4 = "127.0.0.1:6881".parse().unwrap();
        let any6 = "[::]:6881".parse().unwrap();
        let peer4 = "203.0.113.5:51413".parse().unwrap();
        let peer6 = "[2001:db8::5]:51413".parse().unwrap();
        let lo_peer = "127.0.0.1:7000".parse().unwrap();
        assert!(reaches(any4, peer4));
        assert!(!reaches(any4, peer6));
        assert!(reaches(any6, peer6));
        assert!(!reaches(any6, peer4));
        assert!(!reaches(lo4, peer4));
        assert!(reaches(lo4, lo_peer));
        assert!(reaches(any4, lo_peer));
    }
}

//! The tokio driver: one task per socket multiplexes every connection over one UDP port.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

use crate::config::Config;
use crate::conn::{Connection, State, Stats};
use crate::packet::{self, HEADER_LEN, Header, Packet, PacketType, looks_like_utp};

/// Datagrams queued between the receive path and the driver. When the driver falls this
/// far behind, more are dropped, as a full socket buffer would.
const DATAGRAM_QUEUE: usize = 4096;

/// Datagrams that aren't uTP, queued for [`UtpSocket::take_unrecognized`].
const UNRECOGNIZED_QUEUE: usize = 1024;

/// The largest datagram we read.
const MAX_DATAGRAM: usize = 64 * 1024;

/// The UDP socket buffer sizes we ask the OS for.
const SOCKET_BUFFER: usize = 4 * 1024 * 1024;

/// How the driver sends datagrams. Implemented for tokio's [`UdpSocket`]; implement it to
/// run uTP over a socket that something else (such as the DHT) owns, and feed incoming
/// packets with [`UtpSocket::handle_datagram`].
pub trait Transport: Send + Sync + 'static {
    fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
        target: SocketAddr,
    ) -> Poll<io::Result<usize>>;
}

impl Transport for UdpSocket {
    fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
        target: SocketAddr,
    ) -> Poll<io::Result<usize>> {
        UdpSocket::poll_send_to(self, cx, buf, target)
    }
}

type Key = (SocketAddr, u16);

/// Datagrams with their sources.
type DatagramRx = mpsc::Receiver<(Vec<u8>, SocketAddr)>;

#[derive(Debug)]
enum Control {
    Connect(SocketAddr, oneshot::Sender<io::Result<UtpStream>>),
    /// A stream has something for the driver: data to send, a window update, a close.
    Wake(Key),
}

/// A connection's state, shared between the driver and the stream.
#[derive(Debug)]
struct Shared {
    conn: Connection,
    read_waker: Option<Waker>,
    write_waker: Option<Waker>,
    shutdown_waker: Option<Waker>,
    /// The application dropped the stream.
    dropped: bool,
    /// A `Wake` for this connection is already queued.
    notified: bool,
}

type SharedRef = Arc<Mutex<Shared>>;

fn lock(s: &SharedRef) -> MutexGuard<'_, Shared> {
    s.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A UDP socket carrying uTP connections.
///
/// Cloning gives another handle to the same socket. The socket keeps running, for the
/// streams already open, after the last handle is dropped.
#[derive(Clone)]
pub struct UtpSocket {
    inner: Arc<SocketInner>,
}

struct SocketInner {
    local_addr: SocketAddr,
    control: mpsc::UnboundedSender<Control>,
    datagrams: mpsc::Sender<(Vec<u8>, SocketAddr)>,
    accept: tokio::sync::Mutex<mpsc::Receiver<(UtpStream, SocketAddr)>>,
    udp: Option<Arc<UdpSocket>>,
    unrecognized: Mutex<Option<DatagramRx>>,
}

impl fmt::Debug for UtpSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UtpSocket")
            .field("local_addr", &self.inner.local_addr)
            .finish_non_exhaustive()
    }
}

impl UtpSocket {
    /// Bind a UDP socket and start serving uTP on it, with the default configuration.
    pub async fn bind(addr: SocketAddr) -> io::Result<Self> {
        Self::bind_with_config(addr, Config::default()).await
    }

    /// Bind a UDP socket and start serving uTP on it.
    ///
    /// Datagrams that aren't uTP (a DHT sharing the port, say) go to
    /// [`take_unrecognized`](Self::take_unrecognized), and [`send_to`](Self::send_to)
    /// sends on the same socket.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)] // async like tokio's bind, and free to await later
    pub async fn bind_with_config(addr: SocketAddr, config: Config) -> io::Result<Self> {
        let udp = Arc::new(bind_udp(addr)?);
        let local_addr = udp.local_addr()?;
        let (other_tx, other_rx) = mpsc::channel(UNRECOGNIZED_QUEUE);
        let socket = Self::start(udp.clone(), local_addr, config, Some(udp.clone()));
        *socket
            .inner
            .unrecognized
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(other_rx);
        tokio::spawn(recv_loop(udp, socket.inner.datagrams.clone(), other_tx));
        Ok(socket)
    }

    /// Serve uTP over a transport owned elsewhere. The owner reads the datagrams and passes
    /// the uTP ones to [`handle_datagram`](Self::handle_datagram).
    pub fn with_transport(
        transport: Arc<dyn Transport>,
        local_addr: SocketAddr,
        config: Config,
    ) -> Self {
        Self::start(transport, local_addr, config, None)
    }

    fn start(
        transport: Arc<dyn Transport>,
        local_addr: SocketAddr,
        config: Config,
        udp: Option<Arc<UdpSocket>>,
    ) -> Self {
        let config = config.sanitized();
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let (dgram_tx, dgram_rx) = mpsc::channel(DATAGRAM_QUEUE);
        let (accept_tx, accept_rx) = mpsc::channel(config.accept_backlog);
        let driver = Driver {
            epoch: Instant::now(),
            config,
            transport,
            local_addr,
            control_tx: control_tx.downgrade(),
            accept_tx,
            conns: HashMap::new(),
            connecting: HashMap::new(),
            timers: BinaryHeap::new(),
            scheduled: HashMap::new(),
            touched: Vec::new(),
            outbox: Vec::new(),
        };
        tokio::spawn(driver.run(control_rx, dgram_rx));
        Self {
            inner: Arc::new(SocketInner {
                local_addr,
                control: control_tx,
                datagrams: dgram_tx,
                accept: tokio::sync::Mutex::new(accept_rx),
                udp,
                unrecognized: Mutex::new(None),
            }),
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr
    }

    /// Open a connection.
    pub async fn connect(&self, addr: SocketAddr) -> io::Result<UtpStream> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .control
            .send(Control::Connect(addr, tx))
            .map_err(|_| shut_down())?;
        rx.await.map_err(|_| shut_down())?
    }

    /// Wait for an incoming connection.
    pub async fn accept(&self) -> io::Result<(UtpStream, SocketAddr)> {
        self.inner
            .accept
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(shut_down)
    }

    /// Pass in a datagram received on the shared port. Returns false (and ignores it) when
    /// it isn't uTP. A datagram that arrives while the driver is overloaded is dropped.
    pub fn handle_datagram(&self, datagram: &[u8], from: SocketAddr) -> bool {
        if !looks_like_utp(datagram) {
            return false;
        }
        let _ = self.inner.datagrams.try_send((datagram.to_vec(), from));
        true
    }

    /// The datagrams that arrived on a bound socket but aren't uTP, for another protocol
    /// on the same port. Returns `None` after the first call, and for a socket made with
    /// [`with_transport`](Self::with_transport). If nobody takes them, they are dropped.
    pub fn take_unrecognized(&self) -> Option<DatagramRx> {
        self.inner
            .unrecognized
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Send a datagram on a bound socket, for another protocol sharing the port.
    pub async fn send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        match &self.inner.udp {
            Some(udp) => udp.send_to(buf, target).await,
            None => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the socket is owned by its transport",
            )),
        }
    }
}

fn shut_down() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "the uTP socket has shut down")
}

fn bind_udp(addr: SocketAddr) -> io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    // Bulk transfers need more than the default buffers (64 KiB on Windows). The OS may
    // grant less; that only costs throughput.
    let _ = socket.set_recv_buffer_size(SOCKET_BUFFER);
    let _ = socket.set_send_buffer_size(SOCKET_BUFFER);
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    UdpSocket::from_std(socket.into())
}

async fn recv_loop(
    udp: Arc<UdpSocket>,
    utp: mpsc::Sender<(Vec<u8>, SocketAddr)>,
    other: mpsc::Sender<(Vec<u8>, SocketAddr)>,
) {
    let mut buf = vec![0u8; MAX_DATAGRAM];
    loop {
        let r = tokio::select! {
            r = udp.recv_from(&mut buf) => r,
            () = utp.closed() => return,
        };
        match r {
            Ok((n, from)) => {
                let d = buf[..n].to_vec();
                if looks_like_utp(&d) {
                    let _ = utp.try_send((d, from));
                } else {
                    let _ = other.try_send((d, from));
                }
            }
            // Windows reports an ICMP "port unreachable" for an earlier send as a
            // ConnectionReset on the next receive. It says nothing about this socket.
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            Err(e) => {
                tracing::debug!(error = %e, "uTP socket receive failed");
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
    }
}

struct Driver {
    epoch: Instant,
    config: Config,
    transport: Arc<dyn Transport>,
    local_addr: SocketAddr,
    /// Weak, so the driver doesn't keep its own control channel open.
    control_tx: mpsc::WeakUnboundedSender<Control>,
    accept_tx: mpsc::Sender<(UtpStream, SocketAddr)>,
    conns: HashMap<Key, SharedRef>,
    connecting: HashMap<Key, oneshot::Sender<io::Result<UtpStream>>>,
    timers: BinaryHeap<Reverse<(Instant, Key)>>,
    scheduled: HashMap<Key, Instant>,
    touched: Vec<Key>,
    outbox: Vec<(Vec<u8>, SocketAddr)>,
}

impl Driver {
    async fn run(
        mut self,
        mut control: mpsc::UnboundedReceiver<Control>,
        mut datagrams: DatagramRx,
    ) {
        let mut control_open = true;
        loop {
            if !control_open && self.conns.is_empty() {
                return;
            }
            let deadline = self.timers.peek().map(|Reverse((t, _))| *t);
            tokio::select! {
                d = datagrams.recv() => {
                    let Some((d, from)) = d else { return };
                    self.on_datagram(&d, from);
                    // Take what else has arrived before answering, so ACKs coalesce.
                    for _ in 0..64 {
                        match datagrams.try_recv() {
                            Ok((d, from)) => self.on_datagram(&d, from),
                            Err(_) => break,
                        }
                    }
                }
                c = control.recv(), if control_open => match c {
                    Some(c) => {
                        self.on_control(c);
                        while let Ok(c) = control.try_recv() {
                            self.on_control(c);
                        }
                    }
                    None => control_open = false,
                },
                () = sleep_until(deadline) => self.on_timers(),
            }
            self.flush().await;
        }
    }

    fn on_control(&mut self, c: Control) {
        match c {
            Control::Wake(key) => {
                if let Some(s) = self.conns.get(&key) {
                    lock(s).notified = false;
                    self.touched.push(key);
                }
            }
            Control::Connect(addr, reply) => {
                if self.conns.len() >= self.config.max_connections {
                    let _ = reply.send(Err(io::Error::other("too many uTP connections")));
                    return;
                }
                let mut recv_id: u16 = rand::random();
                while self.conns.contains_key(&(addr, recv_id)) {
                    recv_id = rand::random();
                }
                let now = Instant::now();
                let conn =
                    Connection::connect(&self.config, now, self.epoch, recv_id, rand::random());
                let key = (addr, recv_id);
                self.insert(key, conn);
                self.connecting.insert(key, reply);
                self.touched.push(key);
            }
        }
    }

    fn insert(&mut self, key: Key, conn: Connection) -> SharedRef {
        let shared = Arc::new(Mutex::new(Shared {
            conn,
            read_waker: None,
            write_waker: None,
            shutdown_waker: None,
            dropped: false,
            notified: false,
        }));
        self.conns.insert(key, shared.clone());
        shared
    }

    fn stream(&self, key: Key, shared: SharedRef) -> Option<UtpStream> {
        let control = self.control_tx.upgrade()?;
        Some(UtpStream {
            inner: Arc::new(StreamInner {
                shared,
                key,
                control,
                local_addr: self.local_addr,
            }),
        })
    }

    fn on_datagram(&mut self, data: &[u8], from: SocketAddr) {
        let Ok(pkt) = Packet::decode(data) else {
            return;
        };
        let h = pkt.header;
        let now = Instant::now();
        if h.ty == PacketType::Syn {
            let key = (from, h.connection_id.wrapping_add(1));
            if let Some(s) = self.conns.get(&key) {
                lock(s).conn.handle_packet(now, &pkt);
                self.touched.push(key);
                return;
            }
            self.on_syn(&h, from, now);
            return;
        }
        let key = (from, h.connection_id);
        let key = if self.conns.contains_key(&key) {
            Some(key)
        } else if h.ty == PacketType::Reset {
            // A reset may carry either of the two ids.
            [
                h.connection_id.wrapping_add(1),
                h.connection_id.wrapping_sub(1),
            ]
            .into_iter()
            .map(|id| (from, id))
            .find(|k| {
                self.conns
                    .get(k)
                    .is_some_and(|s| lock(s).conn.send_id() == h.connection_id)
            })
        } else {
            None
        };
        match key {
            Some(key) => {
                let s = &self.conns[&key];
                lock(s).conn.handle_packet(now, &pkt);
                self.touched.push(key);
            }
            None if h.ty != PacketType::Reset => {
                self.send_reset(h.connection_id, h.ack_nr, from, now);
            }
            None => {}
        }
    }

    fn on_syn(&mut self, h: &Header, from: SocketAddr, now: Instant) {
        if self.conns.len() >= self.config.max_connections || self.accept_tx.capacity() == 0 {
            self.send_reset(h.connection_id, h.seq_nr, from, now);
            return;
        }
        let conn = Connection::accept(&self.config, now, self.epoch, h, rand::random());
        let key = (from, conn.recv_id());
        let shared = self.insert(key, conn);
        let Some(stream) = self.stream(key, shared) else {
            self.conns.remove(&key);
            return;
        };
        if self.accept_tx.try_send((stream, from)).is_err() {
            // No one is accepting: the stream was dropped and will close itself.
            self.send_reset(h.connection_id, h.seq_nr, from, now);
            self.conns.remove(&key);
            return;
        }
        self.touched.push(key);
    }

    fn send_reset(&mut self, connection_id: u16, ack_nr: u16, to: SocketAddr, now: Instant) {
        let us = now.saturating_duration_since(self.epoch).as_micros();
        #[allow(clippy::cast_possible_truncation)]
        let timestamp = us as u32;
        let h = Header {
            ty: PacketType::Reset,
            connection_id,
            timestamp,
            timestamp_diff: 0,
            wnd_size: 0,
            seq_nr: rand::random(),
            ack_nr,
        };
        let mut out = Vec::with_capacity(HEADER_LEN);
        packet::encode(&h, None, &[], &mut out);
        self.outbox.push((out, to));
    }

    fn on_timers(&mut self) {
        let now = Instant::now();
        while let Some(Reverse((t, key))) = self.timers.peek().copied() {
            if t > now {
                break;
            }
            self.timers.pop();
            if self.scheduled.get(&key) != Some(&t) {
                continue; // superseded
            }
            self.scheduled.remove(&key);
            if let Some(s) = self.conns.get(&key) {
                lock(s).conn.handle_timeout(now);
                self.touched.push(key);
            }
        }
    }

    /// Send what the touched connections have queued, update their timers and wake their
    /// streams.
    async fn flush(&mut self) {
        let now = Instant::now();
        let mut touched = std::mem::take(&mut self.touched);
        touched.sort_unstable();
        touched.dedup();
        for key in touched.drain(..) {
            self.service(key, now);
        }
        self.touched = touched;
        for (buf, to) in std::mem::take(&mut self.outbox) {
            let t = &self.transport;
            if let Err(e) = std::future::poll_fn(|cx| t.poll_send_to(cx, &buf, to)).await {
                tracing::trace!(error = %e, %to, "uTP send failed");
            }
        }
    }

    fn service(&mut self, key: Key, now: Instant) {
        let Some(shared) = self.conns.get(&key).cloned() else {
            return;
        };
        let mut s = lock(&shared);
        if s.dropped && !s.conn.is_finished() {
            s.conn.close();
        }
        while let Some(d) = s.conn.poll_transmit(now) {
            self.outbox.push((d, key.0));
        }

        if s.conn.is_readable()
            && let Some(w) = s.read_waker.take()
        {
            w.wake();
        }
        if (s.conn.send_capacity() > 0 || s.conn.is_finished())
            && let Some(w) = s.write_waker.take()
        {
            w.wake();
        }
        if (s.conn.is_write_closed() || s.conn.is_finished())
            && let Some(w) = s.shutdown_waker.take()
        {
            w.wake();
        }

        if self.connecting.contains_key(&key) {
            if s.conn.is_connected() {
                let reply = self.connecting.remove(&key).expect("checked");
                drop(s);
                match self.stream(key, shared.clone()) {
                    Some(stream) => {
                        if let Err(Ok(stream)) = reply.send(Ok(stream)) {
                            drop(stream); // the caller gave up; it will close itself
                        }
                    }
                    None => {
                        let _ = reply.send(Err(shut_down()));
                    }
                }
                s = lock(&shared);
            } else if let Some(e) = s.conn.error() {
                let reply = self.connecting.remove(&key).expect("checked");
                let _ = reply.send(Err(e.into()));
            }
        }

        // A dropped stream whose FIN is acknowledged is forgotten at once while the peer is
        // still sending (nobody will read it; the peer gets a reset). Once the peer's FIN
        // has arrived too, the connection is closed cleanly and lingers instead: our final
        // ACKs may have been lost, and its retransmissions must be answered with an ACK,
        // not a reset that fails its shutdown.
        let release = s.conn.can_release()
            || (s.dropped && s.conn.is_write_closed() && s.conn.state() != State::Closed);
        let timeout = s.conn.poll_timeout();
        drop(s);
        if release {
            self.conns.remove(&key);
            self.scheduled.remove(&key);
            if let Some(reply) = self.connecting.remove(&key) {
                let _ = reply.send(Err(shut_down()));
            }
            return;
        }
        if let Some(t) = timeout
            && self.scheduled.get(&key) != Some(&t)
        {
            self.scheduled.insert(key, t);
            self.timers.push(Reverse((t, key)));
        }
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t.into()).await,
        None => std::future::pending().await,
    }
}

struct StreamInner {
    shared: SharedRef,
    key: Key,
    control: mpsc::UnboundedSender<Control>,
    local_addr: SocketAddr,
}

impl StreamInner {
    fn notify(&self, s: &mut Shared) {
        if !s.notified {
            s.notified = true;
            let _ = self.control.send(Control::Wake(self.key));
        }
    }

    fn poll_read(&self, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let mut s = lock(&self.shared);
        match s.conn.read(buf.initialize_unfilled()) {
            Ok(Some(n)) => {
                buf.advance(n);
                if s.conn.wants_transmit() {
                    self.notify(&mut s);
                }
                Poll::Ready(Ok(()))
            }
            Ok(None) => {
                s.read_waker = Some(cx.waker().clone());
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn poll_write(&self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let mut s = lock(&self.shared);
        match s.conn.write(buf) {
            Ok(0) => {
                s.write_waker = Some(cx.waker().clone());
                Poll::Pending
            }
            Ok(n) => {
                self.notify(&mut s);
                Poll::Ready(Ok(n))
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut s = lock(&self.shared);
        if s.conn.is_write_closed() {
            return Poll::Ready(Ok(()));
        }
        if let Some(e) = s.conn.error() {
            return Poll::Ready(Err(e.into()));
        }
        s.conn.close();
        s.shutdown_waker = Some(cx.waker().clone());
        self.notify(&mut s);
        Poll::Pending
    }
}

impl Drop for StreamInner {
    fn drop(&mut self) {
        let mut s = lock(&self.shared);
        s.dropped = true;
        s.conn.close();
        let _ = self.control.send(Control::Wake(self.key));
    }
}

/// A uTP connection: an ordered, reliable byte stream, like a TCP stream.
///
/// `poll_shutdown` sends a FIN and completes once the peer has acknowledged everything.
/// Dropping the stream closes it gracefully in the background.
pub struct UtpStream {
    inner: Arc<StreamInner>,
}

impl fmt::Debug for UtpStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UtpStream")
            .field("local_addr", &self.inner.local_addr)
            .field("peer_addr", &self.inner.key.0)
            .finish_non_exhaustive()
    }
}

impl UtpStream {
    pub fn peer_addr(&self) -> SocketAddr {
        self.inner.key.0
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr
    }

    /// The transport's current state: windows, round-trip time, queuing delay.
    pub fn stats(&self) -> Stats {
        lock(&self.inner.shared).conn.stats()
    }

    /// Split into halves that can be used from different tasks. The connection closes
    /// once both are dropped.
    pub fn into_split(self) -> (UtpReadHalf, UtpWriteHalf) {
        (
            UtpReadHalf {
                inner: self.inner.clone(),
            },
            UtpWriteHalf { inner: self.inner },
        )
    }
}

impl AsyncRead for UtpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.inner.poll_read(cx, buf)
    }
}

impl AsyncWrite for UtpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.inner.poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Written bytes are already with the connection, which sends them as the window
        // allows; like TCP, there is nothing to flush.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.poll_shutdown(cx)
    }
}

/// The read half of a [`UtpStream`].
pub struct UtpReadHalf {
    inner: Arc<StreamInner>,
}

/// The write half of a [`UtpStream`].
pub struct UtpWriteHalf {
    inner: Arc<StreamInner>,
}

impl fmt::Debug for UtpReadHalf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UtpReadHalf")
            .field("peer_addr", &self.inner.key.0)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for UtpWriteHalf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UtpWriteHalf")
            .field("peer_addr", &self.inner.key.0)
            .finish_non_exhaustive()
    }
}

impl AsyncRead for UtpReadHalf {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.inner.poll_read(cx, buf)
    }
}

impl AsyncWrite for UtpWriteHalf {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.inner.poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.poll_shutdown(cx)
    }
}

//! `UtpSocket` over real loopback UDP: bulk transfers with integrity checks, a lossy and
//! reordering proxy, many concurrent streams, timeouts, resets and port sharing.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use apitorrent_utp::packet::{self, Header, Packet, PacketType};
use apitorrent_utp::{Config, Transport, UtpSocket, UtpStream};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UdpSocket;

fn localhost() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

fn fast() -> Config {
    Config {
        min_rto: Duration::from_millis(50),
        initial_rto: Duration::from_millis(200),
        connect_timeout: Duration::from_secs(5),
        ..Config::default()
    }
}

fn data(len: usize, seed: u64) -> Vec<u8> {
    let mut v = vec![0u8; len];
    StdRng::seed_from_u64(seed).fill(&mut v[..]);
    v
}

fn digest(b: &[u8]) -> [u8; 20] {
    Sha1::digest(b).into()
}

async fn pair(a: &UtpSocket, b: &UtpSocket, to: SocketAddr) -> (UtpStream, UtpStream) {
    let (connected, accepted) = tokio::join!(a.connect(to), b.accept());
    (connected.unwrap(), accepted.unwrap().0)
}

/// Write `out` while reading `expect_len` bytes; then shut down and expect EOF.
async fn exchange(stream: UtpStream, out: Vec<u8>, expect_len: usize) -> Vec<u8> {
    let (mut r, mut w) = stream.into_split();
    let writer = tokio::spawn(async move {
        w.write_all(&out).await.unwrap();
        w.shutdown().await.unwrap();
    });
    let mut got = Vec::with_capacity(expect_len);
    r.read_to_end(&mut got).await.unwrap();
    writer.await.unwrap();
    assert_eq!(got.len(), expect_len);
    got
}

async fn bulk(a: &UtpSocket, b: &UtpSocket, to: SocketAddr, len: usize) {
    let (x, y) = pair(a, b, to).await;
    let (to_y, to_x) = (data(len, 1), data(len / 3, 2));
    let (dy, dx) = (digest(&to_y), digest(&to_x));
    let (ly, lx) = (to_y.len(), to_x.len());
    let (got_y, got_x) = tokio::join!(exchange(y, to_x, ly), exchange(x, to_y, lx));
    assert_eq!(digest(&got_y), dy, "data to the acceptor corrupted");
    assert_eq!(digest(&got_x), dx, "data to the initiator corrupted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loopback_bulk_transfer() {
    let a = UtpSocket::bind_with_config(localhost(), fast())
        .await
        .unwrap();
    let b = UtpSocket::bind_with_config(localhost(), fast())
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(60),
        bulk(&a, &b, b.local_addr(), 8 << 20),
    )
    .await
    .expect("transfer took too long");
}

/// A UDP relay between a client and a server that drops, duplicates, delays and so
/// reorders packets.
struct LossyProxy {
    addr: SocketAddr,
}

impl LossyProxy {
    async fn start(
        server: SocketAddr,
        loss: f64,
        dup: f64,
        max_delay: Duration,
        seed: u64,
    ) -> Self {
        let sock = Arc::new(UdpSocket::bind(localhost()).await.unwrap());
        let addr = sock.local_addr().unwrap();
        tokio::spawn(async move {
            let mut rng = StdRng::seed_from_u64(seed);
            let mut client: Option<SocketAddr> = None;
            let mut buf = vec![0u8; 65_536];
            loop {
                let Ok((n, from)) = sock.recv_from(&mut buf).await else {
                    continue; // Windows reports ICMP errors from earlier sends here
                };
                let to = if from == server {
                    match client {
                        Some(c) => c,
                        None => continue,
                    }
                } else {
                    client = Some(from);
                    server
                };
                if rng.random_bool(loss) {
                    continue;
                }
                let copies = if rng.random_bool(dup) { 2 } else { 1 };
                for _ in 0..copies {
                    let d = buf[..n].to_vec();
                    let delay = max_delay.mul_f64(rng.random());
                    let sock = sock.clone();
                    tokio::spawn(async move {
                        if !delay.is_zero() {
                            tokio::time::sleep(delay).await;
                        }
                        let _ = sock.send_to(&d, to).await;
                    });
                }
            }
        });
        Self { addr }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transfer_through_a_lossy_reordering_proxy() {
    let a = UtpSocket::bind_with_config(localhost(), fast())
        .await
        .unwrap();
    let b = UtpSocket::bind_with_config(localhost(), fast())
        .await
        .unwrap();
    let proxy = LossyProxy::start(b.local_addr(), 0.05, 0.02, Duration::from_millis(15), 7).await;
    tokio::time::timeout(Duration::from_secs(120), bulk(&a, &b, proxy.addr, 3 << 20))
        .await
        .expect("transfer took too long");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::many_single_char_names)]
async fn many_concurrent_streams() {
    let a = UtpSocket::bind_with_config(localhost(), fast())
        .await
        .unwrap();
    let b = UtpSocket::bind_with_config(localhost(), fast())
        .await
        .unwrap();
    let server = b.clone();
    let accepts = tokio::spawn(async move {
        let mut tasks = Vec::new();
        for _ in 0..64 {
            let (mut s, _) = server.accept().await.unwrap();
            tasks.push(tokio::spawn(async move {
                // Echo the digest of what was sent, then close.
                let mut got = Vec::new();
                let (mut r, mut w) = {
                    let mut len = [0u8; 4];
                    s.read_exact(&mut len).await.unwrap();
                    let len = u32::from_be_bytes(len) as usize;
                    got.resize(len, 0);
                    s.read_exact(&mut got).await.unwrap();
                    s.into_split()
                };
                w.write_all(&digest(&got)).await.unwrap();
                w.shutdown().await.unwrap();
                let mut rest = Vec::new();
                r.read_to_end(&mut rest).await.unwrap();
                assert!(rest.is_empty());
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
    });
    let mut clients = Vec::new();
    for i in 0..64u64 {
        let a = a.clone();
        let to = b.local_addr();
        clients.push(tokio::spawn(async move {
            let mut s = a.connect(to).await.unwrap();
            let payload = data(50_000 + usize::try_from(i).unwrap() * 997, i);
            s.write_all(&u32::try_from(payload.len()).unwrap().to_be_bytes())
                .await
                .unwrap();
            s.write_all(&payload).await.unwrap();
            let mut d = [0u8; 20];
            s.read_exact(&mut d).await.unwrap();
            assert_eq!(d, digest(&payload));
            s.shutdown().await.unwrap();
            let mut rest = Vec::new();
            s.read_to_end(&mut rest).await.unwrap();
        }));
    }
    tokio::time::timeout(Duration::from_secs(60), async {
        for c in clients {
            c.await.unwrap();
        }
        accepts.await.unwrap();
    })
    .await
    .expect("streams took too long");
}

#[tokio::test]
async fn connect_to_a_silent_port_times_out() {
    let silent = UdpSocket::bind(localhost()).await.unwrap();
    let cfg = Config {
        connect_timeout: Duration::from_millis(1500),
        initial_rto: Duration::from_millis(200),
        ..fast()
    };
    let a = UtpSocket::bind_with_config(localhost(), cfg).await.unwrap();
    let start = std::time::Instant::now();
    let err = a.connect(silent.local_addr().unwrap()).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    assert!(start.elapsed() >= Duration::from_millis(1400));
    // Several SYNs arrived, all with the same connection id and sequence number.
    let mut buf = [0u8; 100];
    let mut syns = Vec::new();
    while let Ok(Ok((n, _))) =
        tokio::time::timeout(Duration::from_millis(50), silent.recv_from(&mut buf)).await
    {
        let p = Packet::decode(&buf[..n]).unwrap();
        assert_eq!(p.header.ty, PacketType::Syn);
        syns.push((p.header.connection_id, p.header.seq_nr));
    }
    assert!(syns.len() >= 3, "{syns:?}");
    assert!(syns.windows(2).all(|w| w[0] == w[1]));
}

#[tokio::test]
async fn a_full_backlog_refuses() {
    let cfg = Config {
        accept_backlog: 1,
        ..fast()
    };
    let a = UtpSocket::bind_with_config(localhost(), fast())
        .await
        .unwrap();
    let b = UtpSocket::bind_with_config(localhost(), cfg).await.unwrap();
    // Nobody accepts: the first connection fills the backlog, the second is refused.
    let _first = a.connect(b.local_addr()).await.unwrap();
    let err = a.connect(b.local_addr()).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused);
}

/// A hand-driven peer: raw UDP, speaking uTP packet by packet.
struct RawPeer {
    sock: UdpSocket,
}

impl RawPeer {
    async fn recv(&self) -> (Header, Vec<u8>) {
        let mut buf = [0u8; 2048];
        let (n, _) = tokio::time::timeout(Duration::from_secs(5), self.sock.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        let p = Packet::decode(&buf[..n]).unwrap();
        (p.header, p.payload.to_vec())
    }

    async fn send(&self, h: Header, payload: &[u8], to: SocketAddr) {
        let mut out = Vec::new();
        packet::encode(&h, None, payload, &mut out);
        self.sock.send_to(&out, to).await.unwrap();
    }
}

#[tokio::test]
async fn a_reset_fails_the_stream() {
    let b = UtpSocket::bind_with_config(localhost(), fast())
        .await
        .unwrap();
    let peer = RawPeer {
        sock: UdpSocket::bind(localhost()).await.unwrap(),
    };
    let to = b.local_addr();
    let syn = Header {
        ty: PacketType::Syn,
        connection_id: 1000,
        timestamp: 1,
        timestamp_diff: 0,
        wnd_size: 1 << 20,
        seq_nr: 77,
        ack_nr: 0,
    };
    peer.send(syn, &[], to).await;
    let (stream, _) = b.accept().await.unwrap();
    let (state, _) = peer.recv().await;
    assert_eq!(state.ty, PacketType::State);
    assert_eq!(state.connection_id, 1000);
    assert_eq!(state.ack_nr, 77);
    // Data from the initiator (with its send id, 1001), then a reset.
    let data = Header {
        ty: PacketType::Data,
        connection_id: 1001,
        seq_nr: 78,
        ack_nr: state.seq_nr.wrapping_sub(1),
        ..syn
    };
    peer.send(data, b"hello", to).await;
    let (ack, _) = peer.recv().await;
    assert_eq!((ack.ty, ack.ack_nr), (PacketType::State, 78));
    let mut stream = stream;
    let mut got = [0u8; 5];
    stream.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"hello");
    let reset = Header {
        ty: PacketType::Reset,
        seq_nr: 79,
        ..data
    };
    peer.send(reset, &[], to).await;
    let mut buf = [0u8; 8];
    let err = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::ConnectionReset);
}

#[tokio::test]
async fn packets_for_unknown_connections_get_a_reset() {
    let b = UtpSocket::bind_with_config(localhost(), fast())
        .await
        .unwrap();
    let peer = RawPeer {
        sock: UdpSocket::bind(localhost()).await.unwrap(),
    };
    let stray = Header {
        ty: PacketType::Data,
        connection_id: 4242,
        timestamp: 1,
        timestamp_diff: 0,
        wnd_size: 1 << 20,
        seq_nr: 5,
        ack_nr: 9,
    };
    peer.send(stray, b"?", b.local_addr()).await;
    let (h, _) = peer.recv().await;
    assert_eq!((h.ty, h.connection_id), (PacketType::Reset, 4242));
}

#[tokio::test]
async fn garbage_datagrams_are_ignored() {
    let a = UtpSocket::bind_with_config(localhost(), fast())
        .await
        .unwrap();
    let b = UtpSocket::bind_with_config(localhost(), fast())
        .await
        .unwrap();
    let junk = UdpSocket::bind(localhost()).await.unwrap();
    let mut rng = StdRng::seed_from_u64(3);
    for _ in 0..2000 {
        let mut d = vec![0u8; rng.random_range(0..200)];
        rng.fill(&mut d[..]);
        if let Some(first) = d.first_mut() {
            // Make about half of them look like uTP, so they reach the parser.
            if rng.random_bool(0.5) {
                *first = (rng.random_range(0..5u8) << 4) | 1;
            }
        }
        let _ = junk.send_to(&d, b.local_addr()).await;
    }
    bulk(&a, &b, b.local_addr(), 100_000).await;
}

#[tokio::test]
async fn dht_shares_a_bound_port() {
    let a = UtpSocket::bind_with_config(localhost(), fast())
        .await
        .unwrap();
    let b = UtpSocket::bind_with_config(localhost(), fast())
        .await
        .unwrap();
    let mut other = b.take_unrecognized().unwrap();
    assert!(b.take_unrecognized().is_none());
    let dht = UdpSocket::bind(localhost()).await.unwrap();
    let ping = b"d1:ad2:id20:abcdefghij0123456789e1:q4:ping1:t2:aa1:y1:qe";
    dht.send_to(ping, b.local_addr()).await.unwrap();
    let (got, from) = tokio::time::timeout(Duration::from_secs(5), other.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got, ping);
    assert_eq!(from, dht.local_addr().unwrap());
    b.send_to(b"d1:rd2:id20:mnopqrstuvwxyz123456e1:t2:aa1:y1:re", from)
        .await
        .unwrap();
    let mut buf = [0u8; 128];
    let (n, _) = dht.recv_from(&mut buf).await.unwrap();
    assert!(buf[..n].starts_with(b"d1:r"));
    // uTP on the same port still works.
    bulk(&a, &b, b.local_addr(), 200_000).await;
}

struct Shared(Arc<UdpSocket>);

impl Transport for Shared {
    fn poll_send_to(
        &self,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
        target: SocketAddr,
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.0.poll_send_to(cx, buf, target)
    }
}

/// A socket owned by "the DHT", which hands uTP datagrams to the uTP socket.
async fn externally_owned() -> UtpSocket {
    let udp = Arc::new(UdpSocket::bind(localhost()).await.unwrap());
    let addr = udp.local_addr().unwrap();
    let utp = UtpSocket::with_transport(Arc::new(Shared(udp.clone())), addr, fast());
    let feeder = utp.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65_536];
        loop {
            if let Ok((n, from)) = udp.recv_from(&mut buf).await {
                feeder.handle_datagram(&buf[..n], from);
            }
        }
    });
    utp
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn utp_over_an_external_transport() {
    let a = externally_owned().await;
    let b = externally_owned().await;
    assert!(!b.handle_datagram(b"d1:q4:pinge", localhost()));
    bulk(&a, &b, b.local_addr(), 1 << 20).await;
}

#[tokio::test]
async fn dropping_streams_closes_them_gracefully() {
    let a = UtpSocket::bind_with_config(localhost(), fast())
        .await
        .unwrap();
    let b = UtpSocket::bind_with_config(localhost(), fast())
        .await
        .unwrap();
    let (mut x, mut y) = pair(&a, &b, b.local_addr()).await;
    x.write_all(&data(300_000, 9)).await.unwrap();
    drop(x); // with data still queued
    let mut got = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), y.read_to_end(&mut got))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(digest(&got), digest(&data(300_000, 9)));
}

/// A relay that forwards everything, except that once armed it drops the server's ACKs
/// (`State` packets) until the client retransmits a data or FIN packet.
struct AckHoldingProxy {
    addr: SocketAddr,
    armed: Arc<std::sync::atomic::AtomicBool>,
}

impl AckHoldingProxy {
    async fn start(server: SocketAddr) -> Self {
        use std::sync::atomic::Ordering;
        let sock = Arc::new(UdpSocket::bind(localhost()).await.unwrap());
        let addr = sock.local_addr().unwrap();
        let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = armed.clone();
        tokio::spawn(async move {
            let mut client: Option<SocketAddr> = None;
            let mut sent = std::collections::HashSet::new();
            let mut buf = vec![0u8; 65_536];
            loop {
                let Ok((n, from)) = sock.recv_from(&mut buf).await else {
                    continue;
                };
                let Ok(p) = Packet::decode(&buf[..n]) else {
                    continue;
                };
                let to = if from == server {
                    if flag.load(Ordering::SeqCst) && p.header.ty == PacketType::State {
                        continue;
                    }
                    match client {
                        Some(c) => c,
                        None => continue,
                    }
                } else {
                    client = Some(from);
                    if flag.load(Ordering::SeqCst)
                        && matches!(p.header.ty, PacketType::Data | PacketType::Fin)
                        && !sent.insert(p.header.seq_nr)
                    {
                        flag.store(false, Ordering::SeqCst); // a retransmission: let ACKs through
                    }
                    server
                };
                let _ = sock.send_to(&buf[..n], to).await;
            }
        });
        Self { addr, armed }
    }
}

/// The server has closed its side and read the client's last data and FIN, but its ACKs
/// for them were lost. Dropping the stream must not forget the connection at once: the
/// client's retransmission would get a reset, and the client's shutdown would fail
/// although every byte was delivered. (Found as a reset in
/// `transfer_through_a_lossy_reordering_proxy` on Linux CI.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_stream_lingers_for_lost_final_acks() {
    let a = UtpSocket::bind_with_config(localhost(), fast())
        .await
        .unwrap();
    let b = UtpSocket::bind_with_config(localhost(), fast())
        .await
        .unwrap();
    let proxy = AckHoldingProxy::start(b.local_addr()).await;
    let (mut client, mut server) = pair(&a, &b, proxy.addr).await;
    let run = async {
        // The server finishes sending first, and the client sees its end of stream.
        server.shutdown().await.unwrap();
        let mut rest = Vec::new();
        client.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
        // From here, the server's ACKs are lost until the client retransmits.
        proxy.armed.store(true, std::sync::atomic::Ordering::SeqCst);
        let writer = tokio::spawn(async move {
            client.write_all(b"last words").await?;
            client.shutdown().await
        });
        let mut got = Vec::new();
        server.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"last words");
        drop(server);
        writer.await.unwrap()
    };
    tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .expect("close took too long")
        .expect("the client's shutdown failed");
}

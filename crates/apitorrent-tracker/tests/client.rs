//! The public client against mock HTTP, HTTPS and UDP trackers on loopback.

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use apitorrent_metainfo::Sha1Hash;
use apitorrent_tracker::udp::{
    ACTION_ANNOUNCE, ACTION_CONNECT, ACTION_ERROR, ACTION_SCRAPE, PROTOCOL_ID,
};
use apitorrent_tracker::{
    AnnounceEvent, AnnounceRequest, RetryPolicy, ScrapeStats, TrackerClient, TrackerConfig,
    TrackerError,
};
use apitorrent_wire::PeerId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

const HASH: Sha1Hash = Sha1Hash([0x01; 20]);
/// The mock UDP tracker answers announces for this hash with an error packet.
const BANNED: Sha1Hash = Sha1Hash([0xEE; 20]);

fn request(info_hash: Sha1Hash) -> AnnounceRequest {
    AnnounceRequest {
        info_hash,
        peer_id: PeerId(*b"-AT0100-\x00\xffabcdefghij"),
        port: 6881,
        uploaded: 10,
        downloaded: 20,
        left: 30,
        event: AnnounceEvent::Started,
        num_want: Some(25),
        key: 0x0BAD_F00D,
        tracker_id: None,
    }
}

fn fast_config() -> TrackerConfig {
    TrackerConfig {
        http_timeout: Duration::from_secs(5),
        udp_retry: RetryPolicy {
            initial_timeout: Duration::from_millis(200),
            attempts: 3,
        },
        ..TrackerConfig::default()
    }
}

fn client() -> TrackerClient {
    TrackerClient::with_config(fast_config()).unwrap()
}

// ---------------------------------------------------------------------------------------
// HTTP

/// One canned HTTP response.
#[derive(Clone)]
struct Reply {
    status: &'static str,
    headers: Vec<String>,
    body: Vec<u8>,
    /// Send `Content-Length`; without it the body is delimited by closing the connection.
    content_length: bool,
}

impl Reply {
    fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: "200 OK",
            headers: Vec::new(),
            body: body.into(),
            content_length: true,
        }
    }
}

/// A mock HTTP tracker that answers every request with `reply` and records each request's
/// target (path and query).
async fn http_tracker(reply: Reply) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let reply = reply.clone();
            let log = log.clone();
            tokio::spawn(async move { serve_one(stream, reply, log).await });
        }
    });
    (addr, seen)
}

async fn serve_one(mut stream: TcpStream, reply: Reply, log: Arc<Mutex<Vec<String>>>) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).await.unwrap_or(0) == 0 {
            return;
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head);
    let target = head.split(' ').nth(1).unwrap_or("").to_owned();
    log.lock().unwrap().push(target);

    let mut out = format!("HTTP/1.1 {}\r\nConnection: close\r\n", reply.status);
    if reply.content_length {
        let _ = write!(out, "Content-Length: {}\r\n", reply.body.len());
    }
    for h in &reply.headers {
        out.push_str(h);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    let _ = stream.write_all(out.as_bytes()).await;
    let _ = stream.write_all(&reply.body).await;
    let _ = stream.shutdown().await;
}

fn compact_body() -> Vec<u8> {
    let mut body = b"d8:completei3e10:incompletei4e8:intervali1800e5:peers12:".to_vec();
    body.extend_from_slice(&[10, 0, 0, 1, 0x1a, 0xe1, 10, 0, 0, 2, 0x1a, 0xe2]);
    body.extend_from_slice(b"10:tracker id4:t-ide");
    body
}

#[tokio::test]
async fn http_announce_compact() {
    let (addr, seen) = http_tracker(Reply::ok(compact_body())).await;
    let url = format!("http://{addr}/announce?passkey=s3cr3t");
    let r = client().announce(&url, &request(HASH)).await.unwrap();

    assert_eq!(r.interval, Duration::from_secs(1800));
    assert_eq!(r.seeders, Some(3));
    assert_eq!(r.leechers, Some(4));
    assert_eq!(r.tracker_id.as_deref(), Some("t-id"));
    assert_eq!(
        r.peers,
        [
            "10.0.0.1:6881".parse::<SocketAddr>().unwrap(),
            "10.0.0.2:6882".parse().unwrap()
        ]
    );

    let target = seen.lock().unwrap()[0].clone();
    assert!(
        target.starts_with("/announce?passkey=s3cr3t&info_hash="),
        "{target}"
    );
    // The binary info-hash and peer id arrive byte for byte.
    assert!(
        target.contains(&format!("info_hash={}&", "%01".repeat(20))),
        "{target}"
    );
    assert!(
        target.contains("&peer_id=-AT0100-%00%FFabcdefghij&"),
        "{target}"
    );
    assert!(target.contains("&event=started"), "{target}");
    assert!(target.contains("&numwant=25"), "{target}");
    assert!(target.contains("&key=0BADF00D"), "{target}");
}

#[tokio::test]
async fn http_announce_non_compact_and_ipv6() {
    let mut body = b"d8:intervali60e5:peersld2:ip9:127.0.0.17:peer id20:xxxxxxxxxxxxxxxxxxxx4:porti5000eed2:ip8:2001:db84:porti1eed2:ip11:2001:db8::24:porti5001eee6:peers618:".to_vec();
    let mut v6 = [0u8; 18];
    v6[0] = 0x20;
    v6[1] = 0x01;
    v6[15] = 3;
    v6[16..].copy_from_slice(&5002u16.to_be_bytes());
    body.extend_from_slice(&v6);
    body.push(b'e');
    let (addr, _) = http_tracker(Reply::ok(body)).await;
    let r = client()
        .announce(&format!("http://{addr}/announce"), &request(HASH))
        .await
        .unwrap();
    assert_eq!(
        r.peers,
        [
            "127.0.0.1:5000".parse::<SocketAddr>().unwrap(),
            "[2001:db8::2]:5001".parse().unwrap(),
            "[2001::3]:5002".parse().unwrap(),
        ]
    );
}

#[tokio::test]
async fn http_announce_failure_reason() {
    let (addr, _) = http_tracker(Reply::ok(&b"d14:failure reason17:torrent not founde"[..])).await;
    let err = client()
        .announce(&format!("http://{addr}/announce"), &request(HASH))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, TrackerError::Failure(m) if m == "torrent not found"),
        "{err:?}"
    );

    // The same with an error status: the tracker's reason still wins.
    let (addr, _) = http_tracker(Reply {
        status: "403 Forbidden",
        ..Reply::ok(&b"d14:failure reason6:bannede"[..])
    })
    .await;
    let err = client()
        .announce(&format!("http://{addr}/announce"), &request(HASH))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, TrackerError::Failure(m) if m == "banned"),
        "{err:?}"
    );

    // An error status with no bencoded reason.
    let (addr, _) = http_tracker(Reply {
        status: "502 Bad Gateway",
        ..Reply::ok(&b"<html>oops</html>"[..])
    })
    .await;
    let err = client()
        .announce(&format!("http://{addr}/announce"), &request(HASH))
        .await
        .unwrap_err();
    assert!(matches!(err, TrackerError::HttpStatus(502)), "{err:?}");
}

#[tokio::test]
async fn http_scrape() {
    let other = Sha1Hash([0x02; 20]);
    let mut body = b"d5:filesd20:".to_vec();
    body.extend_from_slice(HASH.as_bytes());
    body.extend_from_slice(b"d8:completei5e10:downloadedi50e10:incompletei6eeee");
    let (addr, seen) = http_tracker(Reply::ok(body)).await;
    let r = client()
        .scrape(
            &format!("http://{addr}/x/announce.php?pk=1"),
            &[HASH, other],
        )
        .await
        .unwrap();
    assert_eq!(r.files.len(), 1);
    assert_eq!(
        r.files[&HASH],
        ScrapeStats {
            complete: 5,
            incomplete: 6,
            downloaded: 50
        }
    );
    let target = seen.lock().unwrap()[0].clone();
    assert_eq!(
        target,
        format!(
            "/x/scrape.php?pk=1&info_hash={}&info_hash={}",
            "%01".repeat(20),
            "%02".repeat(20)
        )
    );

    let err = client()
        .scrape(&format!("http://{addr}/tracker"), &[HASH])
        .await
        .unwrap_err();
    assert!(matches!(err, TrackerError::ScrapeUnsupported), "{err:?}");
}

#[tokio::test]
async fn http_response_size_is_bounded() {
    let config = TrackerConfig {
        max_response_size: 1024,
        ..fast_config()
    };
    let client = TrackerClient::with_config(config).unwrap();

    // Refused up front from Content-Length …
    let (addr, _) = http_tracker(Reply::ok(vec![b'x'; 4096])).await;
    let err = client
        .announce(&format!("http://{addr}/announce"), &request(HASH))
        .await
        .unwrap_err();
    assert!(
        matches!(err, TrackerError::ResponseTooLarge { limit: 1024 }),
        "{err:?}"
    );

    // … and while reading a body of unknown length.
    let (addr, _) = http_tracker(Reply {
        content_length: false,
        ..Reply::ok(vec![b'x'; 4096])
    })
    .await;
    let err = client
        .announce(&format!("http://{addr}/announce"), &request(HASH))
        .await
        .unwrap_err();
    assert!(
        matches!(err, TrackerError::ResponseTooLarge { limit: 1024 }),
        "{err:?}"
    );
}

#[tokio::test]
async fn http_redirects_are_limited() {
    // A tracker that redirects to itself forever.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let reply = Reply {
        status: "302 Found",
        headers: vec![format!("Location: http://{addr}/announce")],
        ..Reply::ok(Vec::new())
    };
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(serve_one(stream, reply.clone(), log.clone()));
        }
    });
    let err = client()
        .announce(&format!("http://{addr}/announce"), &request(HASH))
        .await
        .unwrap_err();
    assert!(matches!(err, TrackerError::Network(_)), "{err:?}");
    // The original request plus five redirects.
    assert_eq!(seen.lock().unwrap().len(), 6);
}

#[tokio::test]
async fn http_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // Accept and never answer.
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    let client = TrackerClient::with_config(TrackerConfig {
        http_timeout: Duration::from_millis(300),
        ..fast_config()
    })
    .unwrap();
    let err = client
        .announce(&format!("http://{addr}/announce"), &request(HASH))
        .await
        .unwrap_err();
    assert!(matches!(err, TrackerError::Timeout), "{err:?}");
}

#[tokio::test]
async fn unsupported_scheme() {
    let err = client()
        .announce("wss://t.example/announce", &request(HASH))
        .await
        .unwrap_err();
    assert!(matches!(err, TrackerError::UnsupportedScheme(s) if s == "wss"));
}

/// HTTPS is verified against the OS trust store, and there is no setting that turns this
/// off: a tracker with a self-signed certificate is refused.
#[tokio::test]
async fn https_rejects_self_signed_certificate() {
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};

    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server_config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert.der().clone()], key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = Arc::new(AtomicUsize::new(0));
    let served_log = served.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            let served = served_log.clone();
            tokio::spawn(async move {
                if let Ok(mut tls) = acceptor.accept(stream).await {
                    // Only reached if the client accepted the certificate.
                    served.fetch_add(1, Ordering::SeqCst);
                    let body = compact_body();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = tls.write_all(head.as_bytes()).await;
                    let _ = tls.write_all(&body).await;
                }
            });
        }
    });

    let err = client()
        .announce(&format!("https://{addr}/announce"), &request(HASH))
        .await
        .unwrap_err();
    assert!(matches!(err, TrackerError::Network(_)), "{err:?}");
    let mut chain = String::new();
    let mut source: Option<&dyn std::error::Error> = Some(&err);
    while let Some(e) = source {
        chain.push_str(&e.to_string());
        chain.push('\n');
        source = e.source();
    }
    assert!(
        chain.to_lowercase().contains("certificate"),
        "error chain should name the certificate:\n{chain}"
    );
    assert_eq!(served.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------------------------------
// UDP

const CONNECTION_ID: u64 = 0x1122_3344_5566_7788;

#[derive(Default)]
struct UdpStats {
    connects: AtomicUsize,
    announces: AtomicUsize,
    scrapes: AtomicUsize,
}

/// A mock BEP 15 tracker. It drops the first `drop_first` datagrams it receives (to force
/// retransmission), and sends a decoy with the wrong transaction id before every real
/// response.
async fn udp_tracker(bind: &str, drop_first: usize) -> Option<(SocketAddr, Arc<UdpStats>)> {
    let socket = UdpSocket::bind(bind).await.ok()?;
    let addr = socket.local_addr().unwrap();
    let stats = Arc::new(UdpStats::default());
    let s = stats.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        let mut dropped = 0;
        loop {
            let Ok((n, from)) = socket.recv_from(&mut buf).await else {
                return;
            };
            if dropped < drop_first {
                dropped += 1;
                continue;
            }
            let p = &buf[..n];
            if n < 16 {
                continue;
            }
            let action = u32::from_be_bytes(p[8..12].try_into().unwrap());
            let tx: [u8; 4] = p[12..16].try_into().unwrap();
            let conn = u64::from_be_bytes(p[0..8].try_into().unwrap());

            let mut decoy = ACTION_CONNECT.to_be_bytes().to_vec();
            decoy.extend_from_slice(&(u32::from_be_bytes(tx) ^ 1).to_be_bytes());
            decoy.extend_from_slice(&[0xFF; 8]);
            let _ = socket.send_to(&decoy, from).await;

            let mut out = Vec::new();
            if action == ACTION_CONNECT && conn == PROTOCOL_ID {
                s.connects.fetch_add(1, Ordering::SeqCst);
                out.extend_from_slice(&ACTION_CONNECT.to_be_bytes());
                out.extend_from_slice(&tx);
                out.extend_from_slice(&CONNECTION_ID.to_be_bytes());
            } else if conn != CONNECTION_ID {
                out.extend_from_slice(&ACTION_ERROR.to_be_bytes());
                out.extend_from_slice(&tx);
                out.extend_from_slice(b"bad connection id");
            } else if action == ACTION_ANNOUNCE && n == 98 {
                s.announces.fetch_add(1, Ordering::SeqCst);
                if p[16..36] == BANNED.0 {
                    out.extend_from_slice(&ACTION_ERROR.to_be_bytes());
                    out.extend_from_slice(&tx);
                    out.extend_from_slice(b"torrent banned");
                } else {
                    out.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
                    out.extend_from_slice(&tx);
                    // Echo `left` as the interval so the test sees the request decoded.
                    let left = u64::from_be_bytes(p[64..72].try_into().unwrap());
                    out.extend_from_slice(&u32::try_from(left).unwrap().to_be_bytes());
                    out.extend_from_slice(&7u32.to_be_bytes()); // leechers
                    out.extend_from_slice(&8u32.to_be_bytes()); // seeders
                    // Peer entries follow the family of the packet.
                    if from.is_ipv4() {
                        out.extend_from_slice(&[192, 0, 2, 1, 0x1a, 0xe1]);
                    } else {
                        let mut e = [0u8; 18];
                        e[0] = 0x20;
                        e[1] = 0x01;
                        e[2] = 0x0d;
                        e[3] = 0xb8;
                        e[15] = 9;
                        e[16..].copy_from_slice(&[0x1a, 0xe1]);
                        out.extend_from_slice(&e);
                    }
                }
            } else if action == ACTION_SCRAPE {
                s.scrapes.fetch_add(1, Ordering::SeqCst);
                out.extend_from_slice(&ACTION_SCRAPE.to_be_bytes());
                out.extend_from_slice(&tx);
                for i in 0..(n - 16) / 20 {
                    let i = u32::try_from(i).unwrap();
                    for n in [100 + i, 200 + i, 300 + i] {
                        out.extend_from_slice(&n.to_be_bytes());
                    }
                }
            } else {
                continue;
            }
            let _ = socket.send_to(&out, from).await;
        }
    });
    Some((addr, stats))
}

#[tokio::test]
async fn udp_announce_reuses_connection_id() {
    let (addr, stats) = udp_tracker("127.0.0.1:0", 0).await.unwrap();
    let client = client();
    let url = format!("udp://{addr}/announce");

    let r = client.announce(&url, &request(HASH)).await.unwrap();
    assert_eq!(r.interval, Duration::from_secs(30));
    assert_eq!(r.seeders, Some(8));
    assert_eq!(r.leechers, Some(7));
    assert_eq!(r.peers, ["192.0.2.1:6881".parse::<SocketAddr>().unwrap()]);

    // A clone shares the cache: no second connect.
    let again = client.clone();
    again.announce(&url, &request(HASH)).await.unwrap();
    assert_eq!(stats.connects.load(Ordering::SeqCst), 1);
    assert_eq!(stats.announces.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn udp_retransmits_lost_packets() {
    // The first connect request is lost; the retransmission gets through.
    let (addr, stats) = udp_tracker("127.0.0.1:0", 1).await.unwrap();
    let r = client()
        .announce(&format!("udp://{addr}"), &request(HASH))
        .await
        .unwrap();
    assert_eq!(r.peers.len(), 1);
    assert_eq!(stats.connects.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn udp_failure_retries_with_fresh_connection_once() {
    let (addr, stats) = udp_tracker("127.0.0.1:0", 0).await.unwrap();
    let client = client();
    let url = format!("udp://{addr}/announce");
    client.announce(&url, &request(HASH)).await.unwrap();

    let err = client.announce(&url, &request(BANNED)).await.unwrap_err();
    assert!(
        matches!(&err, TrackerError::Failure(m) if m == "torrent banned"),
        "{err:?}"
    );
    // The cached id was suspect, so the client reconnected and tried once more.
    assert_eq!(stats.connects.load(Ordering::SeqCst), 2);
    assert_eq!(stats.announces.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn udp_scrape() {
    let (addr, stats) = udp_tracker("127.0.0.1:0", 0).await.unwrap();
    let other = Sha1Hash([0x02; 20]);
    let r = client()
        .scrape(&format!("udp://{addr}"), &[HASH, other])
        .await
        .unwrap();
    assert_eq!(
        r.files[&HASH],
        ScrapeStats {
            complete: 100,
            downloaded: 200,
            incomplete: 300
        }
    );
    assert_eq!(r.files[&other].complete, 101);
    assert_eq!(stats.scrapes.load(Ordering::SeqCst), 1);

    let err = client()
        .scrape(&format!("udp://{addr}"), &[])
        .await
        .unwrap_err();
    assert!(matches!(err, TrackerError::InvalidRequest(_)), "{err:?}");
}

#[tokio::test]
async fn udp_ipv6_peers() {
    let Some((addr, _)) = udp_tracker("[::1]:0", 0).await else {
        eprintln!("skipping: no IPv6 loopback");
        return;
    };
    let r = client()
        .announce(
            &format!("udp://[::1]:{}/announce", addr.port()),
            &request(HASH),
        )
        .await
        .unwrap();
    assert_eq!(
        r.peers,
        ["[2001:db8::9]:6881".parse::<SocketAddr>().unwrap()]
    );
}

#[tokio::test]
async fn udp_timeout() {
    // A socket that never answers.
    let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = silent.local_addr().unwrap();
    let client = TrackerClient::with_config(TrackerConfig {
        udp_retry: RetryPolicy {
            initial_timeout: Duration::from_millis(50),
            attempts: 2,
        },
        ..fast_config()
    })
    .unwrap();
    let started = std::time::Instant::now();
    let err = client
        .announce(&format!("udp://{addr}"), &request(HASH))
        .await
        .unwrap_err();
    assert!(matches!(err, TrackerError::Timeout), "{err:?}");
    // 50 ms + 100 ms.
    assert!(started.elapsed() >= Duration::from_millis(150));
    drop(silent);
}

#[tokio::test]
async fn udp_url_needs_port() {
    let err = client()
        .announce("udp://tracker.example/announce", &request(HASH))
        .await
        .unwrap_err();
    assert!(matches!(err, TrackerError::InvalidUrl(_)), "{err:?}");
}

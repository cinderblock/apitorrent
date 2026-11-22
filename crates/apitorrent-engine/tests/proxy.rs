//! Downloads through a proxy: a local SOCKS5 or HTTP CONNECT proxy that logs what it
//! carries, so the tests see the traffic really went through it.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use apitorrent_engine::bitfield::Bitfield;
use apitorrent_engine::{AddTorrent, ProxySettings, Session, SessionConfig};
use apitorrent_metainfo::{FileSource, Torrent, TorrentBuilder, Version};
use apitorrent_proxy::testing::TestProxy;

type Swarms = Arc<Mutex<HashMap<Vec<u8>, HashSet<u16>>>>;

/// A minimal HTTP tracker: each announcer is 127.0.0.1:<port>; the answer lists every
/// peer it knows for the hash, in compact form.
async fn run_tracker(listener: TcpListener, swarms: Swarms) {
    while let Ok((mut stream, _)) = listener.accept().await {
        let swarms = swarms.clone();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let head = String::from_utf8_lossy(&buf).into_owned();
            let target = head.split_whitespace().nth(1).unwrap_or("").to_owned();
            let query = target.split_once('?').map_or("", |(_, q)| q);
            let (mut hash, mut port) = (Vec::new(), 0u16);
            for pair in query.split('&') {
                match pair.split_once('=') {
                    Some(("info_hash", v)) => hash = percent_decode(v),
                    Some(("port", v)) => port = v.parse().unwrap_or(0),
                    _ => {}
                }
            }
            let peers: Vec<u8> = {
                let mut map = swarms.lock().unwrap();
                let swarm = map.entry(hash).or_default();
                swarm.insert(port);
                swarm
                    .iter()
                    .flat_map(|p| [[127, 0, 0, 1].as_slice(), &p.to_be_bytes()].concat())
                    .collect()
            };
            let mut body = format!("d8:intervali60e5:peers{}:", peers.len()).into_bytes();
            body.extend_from_slice(&peers);
            body.push(b'e');
            let resp = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
            let _ = stream.write_all(resp.as_bytes()).await;
            let _ = stream.write_all(&body).await;
        });
    }
}

fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

fn config() -> SessionConfig {
    SessionConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ..SessionConfig::default()
    }
}

struct Swarm {
    tracker: SocketAddr,
    torrent: Arc<Torrent>,
    data: Vec<u8>,
    seeder: Session,
    _seed_dir: tempfile::TempDir,
}

/// A tracker and a seeding session, with uTP on (so a direct dial would try it first).
async fn swarm() -> Swarm {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tracker = listener.local_addr().unwrap();
    let swarms = Swarms::default();
    tokio::spawn(run_tracker(listener, swarms.clone()));
    let data: Vec<u8> = (0..300_000u32)
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect();
    let bytes = TorrentBuilder::new("file.bin", Version::Hybrid)
        .piece_length(32 * 1024)
        .tracker_tier(vec![format!("http://{tracker}/announce")])
        .file(FileSource::from_bytes(&[], data.clone()))
        .build()
        .unwrap();
    let torrent = Arc::new(Torrent::from_bytes(&bytes).unwrap());
    let seed_dir = tempfile::tempdir().unwrap();
    std::fs::write(seed_dir.path().join("file.bin"), &data).unwrap();
    let seeder = Session::start(config()).await.unwrap();
    let seed = seeder
        .add_torrent(AddTorrent {
            have: None,
            paused: false,
            ..AddTorrent::new(torrent.clone(), seed_dir.path().to_path_buf())
        })
        .unwrap();
    seed.wait_finished().await;
    // The leecher's first announce must find the seeder: with force proxy it takes no
    // incoming connections, so the seeder can't find it instead.
    let port = seeder.listen_addr().port();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !swarms.lock().unwrap().values().any(|s| s.contains(&port)) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the seeder announced");
    Swarm {
        tracker,
        torrent,
        data,
        seeder,
        _seed_dir: seed_dir,
    }
}

/// Download the swarm's torrent in `leecher`.
async fn download(swarm: &Swarm, leecher: &Session) {
    let dir = tempfile::tempdir().unwrap();
    let pieces = u32::try_from(swarm.torrent.info.piece_count()).unwrap();
    let leech = leecher
        .add_torrent(AddTorrent {
            have: Some(Bitfield::new(pieces)),
            paused: false,
            ..AddTorrent::new(swarm.torrent.clone(), dir.path().to_path_buf())
        })
        .unwrap();
    if tokio::time::timeout(Duration::from_secs(30), leech.wait_finished())
        .await
        .is_err()
    {
        panic!(
            "download timed out: {:?}, trackers {:?}, seeder at {}",
            leech.stats(),
            leech.trackers().await,
            swarm.seeder.listen_addr()
        );
    }
    assert_eq!(
        std::fs::read(dir.path().join("file.bin")).unwrap(),
        swarm.data
    );
    leech.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn force_proxy_downloads_through_socks5_and_takes_no_incoming() {
    let swarm = swarm().await;
    let proxy = TestProxy::socks5(Some(("me", "secret"))).await.unwrap();
    let leecher = Session::start(config()).await.unwrap();
    let listen = leecher.listen_addr();
    leecher
        .set_proxy(Some(ProxySettings {
            proxy: proxy.proxy(),
            peers: false,
            trackers: false,
            force: true,
        }))
        .unwrap();
    assert!(leecher.forced());

    download(&swarm, &leecher).await;

    let log = proxy.requests();
    let seeder = swarm.seeder.listen_addr();
    assert!(
        log.contains(&format!("CONNECT {}", swarm.tracker)),
        "the tracker went through the proxy: {log:?}"
    );
    assert!(
        log.contains(&format!("CONNECT {seeder}")),
        "the seeder was reached through the proxy: {log:?}"
    );

    // The listener is closed.
    let mut closed = false;
    for _ in 0..50 {
        if TcpStream::connect(listen).await.is_err() {
            closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(closed, "force proxy closes the listen socket");

    // Turning force proxy off goes direct again, but the listener stays closed until the
    // next start.
    leecher.set_proxy(None).unwrap();
    assert!(leecher.forced() && leecher.proxy().is_none());
    assert!(TcpStream::connect(listen).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peers_only_through_an_http_proxy() {
    let swarm = swarm().await;
    let proxy = TestProxy::http(Some(("me", "secret"))).await.unwrap();
    let leecher = Session::start(config()).await.unwrap();
    leecher
        .set_proxy(Some(ProxySettings {
            proxy: proxy.proxy(),
            peers: true,
            trackers: false,
            force: false,
        }))
        .unwrap();

    download(&swarm, &leecher).await;

    let log = proxy.requests();
    let seeder = swarm.seeder.listen_addr();
    assert!(
        log.contains(&format!("CONNECT {seeder}")),
        "{log:?}, seeder at {seeder}"
    );
    assert!(
        log.iter().all(|r| !r.contains(&swarm.tracker.to_string())),
        "the tracker was reached directly: {log:?}"
    );
    // Not forced: the listener still takes connections.
    assert!(!leecher.forced());
    TcpStream::connect(leecher.listen_addr()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_proxy_is_never_bypassed() {
    let swarm = swarm().await;
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = dead.local_addr().unwrap().port();
    drop(dead);
    let leecher = Session::start(config()).await.unwrap();
    leecher
        .set_proxy(Some(ProxySettings {
            proxy: apitorrent_engine::Proxy {
                kind: apitorrent_engine::ProxyKind::Socks5,
                host: "127.0.0.1".into(),
                port,
                credentials: None,
                remote_dns: true,
            },
            peers: true,
            trackers: true,
            force: false,
        }))
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let pieces = u32::try_from(swarm.torrent.info.piece_count()).unwrap();
    let leech = leecher
        .add_torrent(AddTorrent {
            have: Some(Bitfield::new(pieces)),
            paused: false,
            // Even a peer handed over directly is only dialled through the proxy.
            peers: vec![swarm.seeder.listen_addr()],
            ..AddTorrent::new(swarm.torrent.clone(), dir.path().to_path_buf())
        })
        .unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let stats = leech.stats();
    assert_eq!(stats.downloaded, 0, "{stats:?}");
    let trackers = leech.trackers().await;
    assert!(trackers[0].error.is_some(), "{trackers:?}");
    leech.shutdown().await;
}

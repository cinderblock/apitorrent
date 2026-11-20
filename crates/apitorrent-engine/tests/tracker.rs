//! Peers find each other through a tracker, with no other peer source.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use apitorrent_engine::bitfield::Bitfield;
use apitorrent_engine::{AddTorrent, Session, SessionConfig};
use apitorrent_metainfo::{FileSource, Torrent, TorrentBuilder, Version};

type Swarms = Arc<Mutex<HashMap<Vec<u8>, HashSet<SocketAddr>>>>;

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

/// A minimal HTTP tracker: remembers each announcer as 127.0.0.1:<port> per info-hash and
/// answers with every peer it knows for that hash, in compact form.
async fn run_tracker(listener: TcpListener, swarms: Swarms, events: Arc<Mutex<Vec<String>>>) {
    run_tracker_logging(listener, swarms, events, Arc::default()).await;
}

/// [`run_tracker`], also recording each request's head (request line and headers).
async fn run_tracker_logging(
    listener: TcpListener,
    swarms: Swarms,
    events: Arc<Mutex<Vec<String>>>,
    heads: Arc<Mutex<Vec<String>>>,
) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let swarms = swarms.clone();
        let events = events.clone();
        let heads = heads.clone();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut chunk).await.unwrap();
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let head = String::from_utf8_lossy(&buf).into_owned();
            heads.lock().unwrap().push(head.clone());
            let target = head.split_whitespace().nth(1).unwrap().to_owned();
            let query = target.split_once('?').map_or("", |(_, q)| q);
            let mut hash = Vec::new();
            let mut port = 0u16;
            for pair in query.split('&') {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                match k {
                    "info_hash" => hash = percent_decode(v),
                    "port" => port = v.parse().unwrap(),
                    "event" => events.lock().unwrap().push(v.to_owned()),
                    _ => {}
                }
            }
            let peers: Vec<u8> = {
                let mut map = swarms.lock().unwrap();
                let swarm = map.entry(hash).or_default();
                swarm.insert(SocketAddr::from(([127, 0, 0, 1], port)));
                swarm
                    .iter()
                    .flat_map(|a| {
                        let SocketAddr::V4(v4) = a else {
                            unreachable!()
                        };
                        let mut e = v4.ip().octets().to_vec();
                        e.extend_from_slice(&a.port().to_be_bytes());
                        e
                    })
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

fn config() -> SessionConfig {
    SessionConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ..SessionConfig::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peers_meet_through_a_tracker() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/announce", listener.local_addr().unwrap());
    let swarms = Swarms::default();
    let events = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn(run_tracker(listener, swarms.clone(), events.clone()));

    let data: Vec<u8> = (0..500_000u32)
        .map(|i| u8::try_from(i % 253).unwrap())
        .collect();
    let bytes = TorrentBuilder::new("file.bin", Version::Hybrid)
        .piece_length(32 * 1024)
        .tracker_tier(vec![url])
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
            peers: Vec::new(),
            file_priorities: None,
            ..AddTorrent::new(torrent.clone(), seed_dir.path().to_path_buf())
        })
        .unwrap();
    seed.wait_finished().await;

    let leech_dir = tempfile::tempdir().unwrap();
    let leecher = Session::start(config()).await.unwrap();
    let pieces = u32::try_from(torrent.info.piece_count()).unwrap();
    let leech = leecher
        .add_torrent(AddTorrent {
            have: Some(Bitfield::new(pieces)),
            paused: false,
            peers: Vec::new(),
            file_priorities: None,
            ..AddTorrent::new(torrent.clone(), leech_dir.path().to_path_buf())
        })
        .unwrap();
    // The seeder announced as soon as its check finished, so the leecher's first announce
    // returns the seeder's address.
    tokio::time::timeout(Duration::from_secs(60), leech.wait_finished())
        .await
        .unwrap_or_else(|_| panic!("download via tracker timed out: {:?}", leech.stats()));
    assert_eq!(
        std::fs::read(leech_dir.path().join("file.bin")).unwrap(),
        data
    );

    // The tracker saw `started` from both, and `completed` from the leecher.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let seen = events.lock().unwrap().clone();
    assert!(
        seen.iter().filter(|e| *e == "started").count() >= 2,
        "{seen:?}"
    );
    assert!(seen.iter().any(|e| e == "completed"), "{seen:?}");
    let trackers = leech.trackers().await;
    assert_eq!(trackers.len(), 1);
    assert!(trackers[0].error.is_none(), "{trackers:?}");

    // Stopping sends `stopped`.
    leech.shutdown().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(events.lock().unwrap().iter().any(|e| e == "stopped"));
    seed.shutdown().await;
}

/// One announce as the tracker saw it.
#[derive(Debug)]
struct Seen {
    peer_id: Vec<u8>,
    user_agent: String,
    event: String,
    numwant: String,
    uploaded: u64,
}

fn parse_heads(heads: &[String]) -> Vec<Seen> {
    heads
        .iter()
        .map(|h| {
            let target = h.split_whitespace().nth(1).unwrap();
            let query = target.split_once('?').map_or("", |(_, q)| q);
            let param = |name: &str| {
                query
                    .split('&')
                    .find_map(|p| p.strip_prefix(name).and_then(|v| v.strip_prefix('=')))
                    .unwrap_or("")
                    .to_owned()
            };
            let user_agent = h
                .lines()
                .filter_map(|l| l.split_once(':'))
                .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
                .map(|(_, v)| v.trim().to_owned())
                .unwrap_or_default();
            Seen {
                peer_id: percent_decode(&param("peer_id")),
                user_agent,
                event: param("event"),
                numwant: param("numwant"),
                uploaded: param("uploaded").parse().unwrap_or(0),
            }
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_torrent_presents_as_its_trackers_identity() {
    use apitorrent_engine::identity::{ClientProfile, HostPattern, IdentityRule, IdentityRules};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/announce", listener.local_addr().unwrap());
    let heads = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn(run_tracker_logging(
        listener,
        Swarms::default(),
        Arc::default(),
        heads.clone(),
    ));

    let data: Vec<u8> = (0..200_000u32)
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect();
    let bytes = TorrentBuilder::new("file.bin", Version::V1)
        .piece_length(32 * 1024)
        .tracker_tier(vec![url])
        .file(FileSource::from_bytes(&[], data.clone()))
        .build()
        .unwrap();
    let torrent = Arc::new(Torrent::from_bytes(&bytes).unwrap());

    // The seeder presents as Deluge to trackers on 127.0.0.1; the leecher has no rules.
    let seed_dir = tempfile::tempdir().unwrap();
    std::fs::write(seed_dir.path().join("file.bin"), &data).unwrap();
    let seeder = Session::start(config()).await.unwrap();
    let deluge = Arc::new(ClientProfile::deluge("2.1.1", "2.0.10.0").unwrap());
    let rules = IdentityRules {
        default: None,
        rules: vec![IdentityRule {
            host: HostPattern::parse("127.0.0.1").unwrap(),
            profile: Some(deluge.clone()),
        }],
    };
    seeder.set_identity_rules(rules);
    let seed = seeder
        .add_torrent(AddTorrent::new(
            torrent.clone(),
            seed_dir.path().to_path_buf(),
        ))
        .unwrap();
    seed.wait_finished().await;

    let leech_dir = tempfile::tempdir().unwrap();
    let leecher = Session::start(config()).await.unwrap();
    let pieces = u32::try_from(torrent.info.piece_count()).unwrap();
    let leech = leecher
        .add_torrent(AddTorrent {
            have: Some(Bitfield::new(pieces)),
            ..AddTorrent::new(torrent.clone(), leech_dir.path().to_path_buf())
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(60), leech.wait_finished())
        .await
        .unwrap_or_else(|_| panic!("download timed out: {:?}", leech.stats()));

    // The tracker saw the seeder as Deluge, with libtorrent's numwant, and the leecher as
    // apitorrent, as before.
    let seen = parse_heads(&heads.lock().unwrap());
    let ours = concat!("apitorrent/", env!("CARGO_PKG_VERSION"));
    let spoofed: Vec<&Seen> = seen
        .iter()
        .filter(|s| s.peer_id.starts_with(b"-DE211s-"))
        .collect();
    assert!(!spoofed.is_empty(), "{seen:?}");
    for s in &spoofed {
        assert_eq!(s.user_agent, "Deluge/2.1.1 libtorrent/2.0.10.0", "{s:?}");
        assert_eq!(s.numwant, "200", "{s:?}");
    }
    let deluge_id = spoofed[0].peer_id.clone();
    assert!(
        spoofed.iter().all(|s| s.peer_id == deluge_id),
        "one peer id for the session"
    );
    let native: Vec<&Seen> = seen
        .iter()
        .filter(|s| s.peer_id.starts_with(b"-AT"))
        .collect();
    assert!(!native.is_empty(), "{seen:?}");
    assert!(native.iter().all(|s| s.user_agent == ours), "{native:?}");
    assert_eq!(native[0].peer_id, leecher.peer_id().0);

    // Peers see it too: the seeder's handshake carried the Deluge peer id, and its
    // extension handshake Deluge's `v`.
    let peers = leech.peers().await;
    assert!(
        peers
            .iter()
            .any(|p| p.client == "Deluge/2.1.1 libtorrent/2.0.10.0"),
        "{peers:?}"
    );

    // Taking the rule away applies live, and looks like one client quitting and another
    // starting: `stopped` from the Deluge identity first, then `started` from apitorrent,
    // under a peer id the tracker hasn't seen (not even the session's), with the upload
    // reported once, by the `stopped`.
    let before = heads.lock().unwrap().len();
    seeder.set_identity_rules(IdentityRules::default());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let after = parse_heads(&heads.lock().unwrap()[before..]);
        let stopped = after
            .iter()
            .position(|s| s.peer_id == deluge_id && s.event == "stopped");
        let restarted = after.iter().position(|s| {
            s.peer_id.starts_with(b"-AT")
                && s.peer_id != leecher.peer_id().0
                && s.event == "started"
        });
        if let (Some(stop), Some(start)) = (stopped, restarted) {
            assert!(stop < start, "`started` before `stopped`: {after:?}");
            let (stop, start) = (&after[stop], &after[start]);
            assert_eq!(start.user_agent, ours);
            assert_ne!(start.peer_id, seeder.peer_id().0, "a fresh peer id");
            assert!(stop.uploaded >= data.len() as u64, "{stop:?}");
            assert_eq!(start.uploaded, 0, "nothing reported twice: {start:?}");
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no switch seen: {after:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    leech.shutdown().await;
    seed.shutdown().await;
}

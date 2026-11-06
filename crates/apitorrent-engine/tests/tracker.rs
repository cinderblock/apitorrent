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
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let swarms = swarms.clone();
        let events = events.clone();
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
            torrent: torrent.clone(),
            save_path: seed_dir.path().to_path_buf(),
            have: None,
            paused: false,
            peers: Vec::new(),
            file_priorities: None,
        })
        .unwrap();
    seed.wait_finished().await;

    let leech_dir = tempfile::tempdir().unwrap();
    let leecher = Session::start(config()).await.unwrap();
    let pieces = u32::try_from(torrent.info.piece_count()).unwrap();
    let leech = leecher
        .add_torrent(AddTorrent {
            torrent: torrent.clone(),
            save_path: leech_dir.path().to_path_buf(),
            have: Some(Bitfield::new(pieces)),
            paused: false,
            peers: Vec::new(),
            file_priorities: None,
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

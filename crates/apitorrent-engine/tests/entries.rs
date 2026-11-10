//! One torrent with several entries (one per space): one swarm, each entry with its own
//! trackers and its own counts, and nothing one entry brought in reported by another.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use apitorrent_engine::bitfield::Bitfield;
use apitorrent_engine::{AddTorrent, EntryParams, Session, SessionConfig, TorrentHandle};
use apitorrent_metainfo::{FileSource, Torrent, TorrentBuilder, Version};

/// One announce as a tracker saw it.
#[derive(Debug, Clone)]
struct Announce {
    event: String,
    downloaded: u64,
    uploaded: u64,
    left: u64,
    port: u16,
}

/// A minimal HTTP tracker that records every announce and answers with every other peer
/// it has seen (compact form).
#[derive(Clone, Default)]
struct Tracker {
    seen: Arc<Mutex<Vec<Announce>>>,
    peers: Arc<Mutex<HashSet<SocketAddr>>>,
}

impl Tracker {
    async fn start(&self) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/announce", listener.local_addr().unwrap());
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let me = me.clone();
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
                    let q: HashMap<&str, &str> =
                        query.split('&').filter_map(|p| p.split_once('=')).collect();
                    let num = |k: &str| q.get(k).and_then(|v| v.parse().ok()).unwrap_or(0);
                    let a = Announce {
                        event: q.get("event").copied().unwrap_or("").to_owned(),
                        downloaded: num("downloaded"),
                        uploaded: num("uploaded"),
                        left: num("left"),
                        port: u16::try_from(num("port")).unwrap(),
                    };
                    let me_addr = SocketAddr::from(([127, 0, 0, 1], a.port));
                    let stopped = a.event == "stopped";
                    me.seen.lock().unwrap().push(a);
                    let peers: Vec<u8> = {
                        let mut set = me.peers.lock().unwrap();
                        if stopped {
                            set.remove(&me_addr);
                        } else {
                            set.insert(me_addr);
                        }
                        set.iter()
                            .filter(|p| **p != me_addr)
                            .flat_map(|p| {
                                let SocketAddr::V4(v4) = p else {
                                    unreachable!()
                                };
                                let mut e = v4.ip().octets().to_vec();
                                e.extend_from_slice(&p.port().to_be_bytes());
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
        });
        url
    }

    /// Announces from the session listening on `port`.
    fn from(&self, port: u16) -> Vec<Announce> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|a| a.port == port)
            .cloned()
            .collect()
    }
}

fn config() -> SessionConfig {
    SessionConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ..SessionConfig::default()
    }
}

fn data() -> Vec<u8> {
    (0..700_000u32)
        .map(|i| u8::try_from((i * 7 + i / 255) % 251).unwrap())
        .collect()
}

fn torrent(data: &[u8]) -> Arc<Torrent> {
    let bytes = TorrentBuilder::new("file.bin", Version::V1)
        .piece_length(32 * 1024)
        .file(FileSource::from_bytes(&[], data.to_vec()))
        .build()
        .unwrap();
    Arc::new(Torrent::from_bytes(&bytes).unwrap())
}

/// A seeder that announces only to `url`.
async fn seeder(t: &Arc<Torrent>, data: &[u8], url: &str) -> (Session, TorrentHandle) {
    let dir = tempfile::tempdir().unwrap().keep();
    std::fs::write(dir.join("file.bin"), data).unwrap();
    let session = Session::start(config()).await.unwrap();
    let handle = session
        .add_torrent(AddTorrent {
            trackers: Some(vec![vec![url.to_owned()]]),
            ..AddTorrent::new(t.clone(), dir)
        })
        .unwrap();
    handle.wait_finished().await;
    (session, handle)
}

async fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..300 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_entry_reports_only_its_own_peers() {
    let data = data();
    let t = torrent(&data);
    let one = Tracker::default();
    let two = Tracker::default();
    let url_one = one.start().await;
    let url_two = two.start().await;
    let (_seed_session, seed) = seeder(&t, &data, &url_one).await;

    // Entry 1 announces to tracker one (where the seeder is), entry 2 to tracker two.
    let dir = tempfile::tempdir().unwrap();
    let session = Session::start(config()).await.unwrap();
    let port = session.listen_addr().port();
    let pieces = u32::try_from(t.info.piece_count()).unwrap();
    let handle = session
        .add_torrent(AddTorrent {
            have: Some(Bitfield::new(pieces)),
            entry: 1,
            trackers: Some(vec![vec![url_one.clone()]]),
            more_entries: vec![EntryParams {
                trackers: Some(vec![vec![url_two.clone()]]),
                ..EntryParams::new(2)
            }],
            ..AddTorrent::new(t.clone(), dir.path().to_path_buf())
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(60), handle.wait_finished())
        .await
        .unwrap_or_else(|_| panic!("download timed out: {:?}", handle.stats()));
    assert_eq!(std::fs::read(dir.path().join("file.bin")).unwrap(), data);

    let stats = handle.stats();
    let e1 = stats.entries.iter().find(|e| e.key == 1).unwrap();
    let e2 = stats.entries.iter().find(|e| e.key == 2).unwrap();
    assert!(e1.downloaded >= data.len() as u64, "{stats:?}");
    assert_eq!(e2.downloaded, 0, "entry 2 had no peers of its own");
    // Every connection is entry 1's: the seeder came from its tracker.
    let peers = handle.peers().await;
    assert!(!peers.is_empty());
    assert!(peers.iter().all(|p| p.entry == 1), "{peers:?}");

    // Tracker one hears `completed` with entry 1's download; tracker two never hears of a
    // byte downloaded, though the data is complete, and is told `completed` with left=0.
    eventually("completed on tracker one", || {
        one.from(port).iter().any(|a| a.event == "completed")
    })
    .await;
    let done = one
        .from(port)
        .into_iter()
        .find(|a| a.event == "completed")
        .unwrap();
    assert!(done.downloaded >= data.len() as u64, "{done:?}");
    assert_eq!(done.left, 0);
    eventually("an announce to tracker two after completion", || {
        two.from(port).iter().any(|a| a.left == 0)
    })
    .await;
    for a in two.from(port) {
        assert_eq!(a.downloaded, 0, "{a:?}");
        assert_eq!(a.uploaded, 0, "{a:?}");
    }

    // Removing entry 1 leaves entry 2 seeding, with its own (zero) counts.
    handle.remove_entry(1).await;
    eventually("stopped on tracker one", || {
        one.from(port).iter().any(|a| a.event == "stopped")
    })
    .await;
    let stats = handle.stats();
    assert_eq!(stats.entries.len(), 1);
    assert_eq!(stats.entries[0].key, 2);
    assert_eq!(stats.state, apitorrent_engine::TorrentState::Seeding);
    handle.shutdown().await;
    seed.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_entry_added_after_completion_announces_nothing_left() {
    let data = data();
    let t = torrent(&data);
    let one = Tracker::default();
    let two = Tracker::default();
    let url_one = one.start().await;
    let url_two = two.start().await;

    // The data is already here (the first entry's copy).
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file.bin"), &data).unwrap();
    let session = Session::start(config()).await.unwrap();
    let port = session.listen_addr().port();
    let handle = session
        .add_torrent(AddTorrent {
            entry: 1,
            trackers: Some(vec![vec![url_one]]),
            ..AddTorrent::new(t.clone(), dir.path().to_path_buf())
        })
        .unwrap();
    handle.wait_finished().await;
    handle
        .add_entry(EntryParams {
            trackers: Some(vec![vec![url_two]]),
            ..EntryParams::new(2)
        })
        .await;
    eventually("entry 2's first announce", || !two.from(port).is_empty()).await;
    let first = two.from(port).remove(0);
    assert_eq!(first.event, "started");
    assert_eq!(first.left, 0, "{first:?}");
    assert_eq!(first.downloaded, 0);
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shared_tracker_is_announced_once() {
    let data = data();
    let t = torrent(&data);
    let one = Tracker::default();
    let url = one.start().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file.bin"), &data).unwrap();
    let session = Session::start(config()).await.unwrap();
    let port = session.listen_addr().port();
    let handle = session
        .add_torrent(AddTorrent {
            entry: 1,
            trackers: Some(vec![vec![url.clone()]]),
            more_entries: vec![EntryParams {
                trackers: Some(vec![vec![url.clone()]]),
                ..EntryParams::new(2)
            }],
            ..AddTorrent::new(t.clone(), dir.path().to_path_buf())
        })
        .unwrap();
    handle.wait_finished().await;
    eventually("the first announce", || !one.from(port).is_empty()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let started = |a: &Vec<Announce>| a.iter().filter(|a| a.event == "started").count();
    assert_eq!(started(&one.from(port)), 1, "{:?}", one.from(port));
    assert_eq!(handle.trackers_of(Some(2)).await.len(), 1);

    // Pausing entry 1 hands the tracker to entry 2, which starts afresh.
    handle.pause_entry(1).await;
    eventually("entry 2 takes the tracker over", || {
        started(&one.from(port)) == 2
    })
    .await;
    let events: Vec<String> = one.from(port).into_iter().map(|a| a.event).collect();
    assert_eq!(events, ["started", "stopped", "started"], "{events:?}");
    handle.shutdown().await;
}

//! Two spaces holding the same torrent: one swarm, a placement in each space's storage,
//! removing one space's entry leaves the other seeding, and data one space brought in is
//! never reported to the other's tracker.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use apitorrent_core::{Core, CoreConfig, Key};
use apitorrent_engine::{AddTorrent, Session, SessionConfig};
use apitorrent_metainfo::{FileSource, Torrent, TorrentBuilder, Version};
use apitorrent_types::spaces::{CreateSpaceRequest, DEFAULT_SPACE, SpaceId, SpaceSettings};
use apitorrent_types::{AddSource, AddTorrentRequest, TorrentState, UnwantedPolicy};

/// One announce as a tracker saw it.
#[derive(Debug, Clone)]
struct Announce {
    event: String,
    downloaded: u64,
    uploaded: u64,
    left: u64,
}

/// An HTTP tracker that records announces and always answers with `peer` (if any).
async fn tracker(peer: Option<SocketAddr>) -> (String, Arc<Mutex<Vec<Announce>>>) {
    let seen: Arc<Mutex<Vec<Announce>>> = Arc::default();
    let mut body = b"d8:intervali60e5:peers".to_vec();
    match peer {
        Some(SocketAddr::V4(v4)) => {
            body.extend_from_slice(b"6:");
            body.extend_from_slice(&v4.ip().octets());
            body.extend_from_slice(&v4.port().to_be_bytes());
        }
        _ => body.extend_from_slice(b"0:"),
    }
    body.push(b'e');
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let (body, log) = (body.clone(), log.clone());
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                let target = head.split_whitespace().nth(1).unwrap_or("").to_owned();
                let query = target.split_once('?').map_or("", |(_, q)| q);
                let q: HashMap<&str, &str> =
                    query.split('&').filter_map(|p| p.split_once('=')).collect();
                let num = |k: &str| q.get(k).and_then(|v| v.parse().ok()).unwrap_or(0);
                log.lock().unwrap().push(Announce {
                    event: q.get("event").copied().unwrap_or("").to_owned(),
                    downloaded: num("downloaded"),
                    uploaded: num("uploaded"),
                    left: num("left"),
                });
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.write_all(&body).await;
            });
        }
    });
    (format!("http://{addr}/announce"), seen)
}

fn data() -> Vec<u8> {
    (0..600_000u32)
        .map(|i| u8::try_from((i * 31 + i / 97) % 251).unwrap())
        .collect()
}

/// The release, announcing to `url`.
fn release(data: &[u8], url: &str) -> Vec<u8> {
    TorrentBuilder::new("show.mkv", Version::V1)
        .piece_length(32 * 1024)
        .tracker_tier(vec![url.to_owned()])
        .file(FileSource::from_bytes(&[], data.to_vec()))
        .build()
        .unwrap()
}

fn add(bytes: &[u8]) -> AddTorrentRequest {
    AddTorrentRequest {
        source: AddSource::TorrentFile {
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        },
        save_path: None,
        paused: false,
        label: None,
        file_priorities: None,
        unwanted_policy: UnwantedPolicy::Hold,
    }
}

async fn wait_for(
    core: &Core,
    key: &Key,
    what: &str,
    ok: impl Fn(&apitorrent_types::TorrentSummary) -> bool,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if core.store().get(key).is_some_and(|s| ok(&s)) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}: {:?}",
            core.store().get(key)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn eventually(what: &str, ok: impl Fn() -> bool) {
    for _ in 0..300 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_torrent_in_two_spaces() {
    let root = tempfile::tempdir().unwrap();
    let data = data();

    // A seeder elsewhere, found through the first space's tracker.
    let seeder = Session::start(SessionConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ..SessionConfig::default()
    })
    .await
    .unwrap();
    let (url_a, seen_a) = tracker(Some(seeder.listen_addr())).await;
    // The second space's tracker knows nobody: whatever it hears is the daemon's own.
    let (url_b, seen_b) = tracker(None).await;
    let bytes_a = release(&data, &url_a);
    let bytes_b = release(&data, &url_b);
    let torrent = Arc::new(Torrent::from_bytes(&bytes_a).unwrap());
    assert_eq!(
        torrent.info.info_hashes(),
        Torrent::from_bytes(&bytes_b).unwrap().info.info_hashes(),
        "the same torrent, announced elsewhere"
    );
    let seed_dir = root.path().join("seed");
    std::fs::create_dir_all(&seed_dir).unwrap();
    std::fs::write(seed_dir.join("show.mkv"), &data).unwrap();
    let seed = seeder
        .add_torrent(AddTorrent::new(torrent.clone(), seed_dir))
        .unwrap();
    seed.wait_finished().await;

    let core = Core::start(CoreConfig {
        data_dir: root.path().join("data"),
        download_dir: root.path().join("downloads"),
        session: SessionConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            ..SessionConfig::default()
        },
    })
    .await
    .unwrap();
    let tv_root = root.path().join("tv");
    let tv: SpaceId = core
        .create_space(CreateSpaceRequest {
            slug: "tv".into(),
            name: "TV".into(),
            description: String::new(),
            settings: SpaceSettings {
                storage_roots: vec![tv_root.to_string_lossy().into_owned()],
                ..SpaceSettings::default()
            },
        })
        .await
        .unwrap()
        .id;

    // The default space downloads it.
    let id = core.add(DEFAULT_SPACE, add(&bytes_a)).await.unwrap();
    let a = Key::new(DEFAULT_SPACE, id.clone());
    wait_for(&core, &a, "the download", |s| {
        s.state == TorrentState::Seeding
    })
    .await;

    // The tv space adds the same torrent: it joins the swarm and gets its own copy of the
    // file in its own storage, without downloading anything.
    let id_b = core.add(tv, add(&bytes_b)).await.unwrap();
    assert_eq!(id_b, id);
    let b = Key::new(tv, id.clone());
    assert_eq!(core.swarm_spaces(&id), [DEFAULT_SPACE, tv], "one swarm");
    let mine = root.path().join("downloads").join("show.mkv");
    let theirs = tv_root.join("show.mkv");
    eventually("the tv space's copy", || {
        std::fs::read(&theirs).is_ok_and(|b| b == data)
    })
    .await;
    assert_eq!(std::fs::read(&mine).unwrap(), data);
    wait_for(&core, &b, "the tv entry seeding", |s| {
        s.state == TorrentState::Seeding && s.progress >= 1.0
    })
    .await;
    let sa = core.store().get(&a).unwrap();
    let sb = core.store().get(&b).unwrap();
    assert!(sa.downloaded >= data.len() as u64, "{sa:?}");
    assert_eq!(sb.downloaded, 0, "the tv entry downloaded nothing itself");
    assert_eq!(sb.save_path, tv_root.to_string_lossy());

    // Its tracker hears only its own (empty) share: nothing downloaded, nothing left.
    eventually("the tv entry's first announce", || {
        !seen_b.lock().unwrap().is_empty()
    })
    .await;
    for an in seen_b.lock().unwrap().iter() {
        assert_eq!((an.downloaded, an.uploaded, an.left), (0, 0, 0), "{an:?}");
    }
    // The first space's tracker heard the download.
    assert!(
        seen_a
            .lock()
            .unwrap()
            .iter()
            .any(|an| an.event == "completed" && an.downloaded >= data.len() as u64),
        "{:?}",
        seen_a.lock().unwrap()
    );

    // Removing the default space's entry with its data deletes only its file; the tv entry
    // keeps the swarm, on its own files, and goes on seeding.
    core.remove(&a, true).await.unwrap();
    assert!(!mine.exists(), "the default space's copy is gone");
    assert_eq!(std::fs::read(&theirs).unwrap(), data, "the tv copy stays");
    assert_eq!(core.swarm_spaces(&id), [tv]);
    assert!(core.store().get(&a).is_none());
    // The swarm restarted on the tv files and checks them; ask it, not the last summary.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !core
        .detail(&b)
        .await
        .unwrap()
        .files
        .iter()
        .all(|f| f.progress >= 1.0)
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the tv files never checked"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    wait_for(&core, &b, "the tv entry seeding alone", |s| {
        s.state == TorrentState::Seeding && s.progress >= 1.0
    })
    .await;
    eventually("stopped on the first space's tracker", || {
        seen_a
            .lock()
            .unwrap()
            .iter()
            .any(|an| an.event == "stopped")
    })
    .await;
    for an in seen_b.lock().unwrap().iter() {
        assert_eq!(
            an.downloaded, 0,
            "never anything downloaded on tv's tracker: {an:?}"
        );
    }

    // A restart brings the tv entry back as the swarm's primary.
    core.shutdown().await.unwrap();
    drop(core);
    let core = Core::start(CoreConfig {
        data_dir: root.path().join("data"),
        download_dir: root.path().join("downloads"),
        session: SessionConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            ..SessionConfig::default()
        },
    })
    .await
    .unwrap();
    assert_eq!(core.swarm_spaces(&id), [tv]);
    wait_for(&core, &b, "the tv entry seeding after a restart", |s| {
        s.state == TorrentState::Seeding && s.progress >= 1.0
    })
    .await;
    core.shutdown().await.unwrap();
    seed.shutdown().await;
}

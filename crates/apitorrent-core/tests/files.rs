//! File order, sequential files and moving completed files, through the core.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use apitorrent_core::{Core, CoreConfig, Key};
use apitorrent_engine::{AddTorrent, Session, SessionConfig};
use apitorrent_metainfo::{FileSource, Torrent, TorrentBuilder, Version};
use apitorrent_types::files::{FileOrder, MoveCompleted, MoveRule, MoveWhen, SequentialFile};
use apitorrent_types::spaces::DEFAULT_SPACE;
use apitorrent_types::{AddSource, AddTorrentRequest, TorrentDetail, TorrentState, UnwantedPolicy};
use base64::Engine as _;

fn config(root: &Path) -> CoreConfig {
    CoreConfig {
        data_dir: root.join("data"),
        download_dir: root.join("downloads"),
        session: SessionConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            ..SessionConfig::default()
        },
    }
}

fn k(id: &str) -> Key {
    Key::new(DEFAULT_SPACE, id)
}

fn data(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from((i * 7 + usize::from(seed)) % 251).unwrap())
        .collect()
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

async fn wait_state(core: &Core, id: &str, state: TorrentState) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while core.store().get(&k(id)).is_none_or(|s| s.state != state) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {state:?}: {:?}",
            core.store().get(&k(id))
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll the torrent's detail until `ok` holds.
async fn wait_detail(
    core: &Core,
    id: &str,
    what: &str,
    ok: impl Fn(&TorrentDetail) -> bool,
) -> TorrentDetail {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let d = core.detail(&k(id)).await.unwrap();
        if ok(&d) {
            return d;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}: {:?}",
            d.files
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn moved(d: &TorrentDetail, name: &str) -> Option<PathBuf> {
    d.files
        .iter()
        .find(|f| f.path.ends_with(name))
        .and_then(|f| f.moved_to.as_deref())
        .map(PathBuf::from)
}

/// A torrent already complete on disk: setting a move moves its files at once, by rule,
/// and it keeps seeding them from there, across a restart. Removing it with its data
/// deletes them where they are.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_files_move_by_rule_and_stay_moved() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let public = root.path().join("public");
    let video = root.path().join("video");
    let (a, nfo, c) = (data(100_000, 1), data(500, 2), data(70_000, 3));
    let torrent = TorrentBuilder::new("Show", Version::V1)
        .piece_length(16 * 1024)
        .file(FileSource::from_bytes(&["a.mkv"], a.clone()))
        .file(FileSource::from_bytes(&["info.nfo"], nfo.clone()))
        .file(FileSource::from_bytes(&["extras", "c.jpg"], c.clone()))
        .build()
        .unwrap();
    let dir = cfg.download_dir.join("Show");
    std::fs::create_dir_all(dir.join("extras")).unwrap();
    std::fs::write(dir.join("a.mkv"), &a).unwrap();
    std::fs::write(dir.join("info.nfo"), &nfo).unwrap();
    std::fs::write(dir.join("extras").join("c.jpg"), &c).unwrap();

    let core = Core::start(cfg.clone()).await.unwrap();
    let id = core.add(DEFAULT_SPACE, add(&torrent)).await.unwrap();
    wait_state(&core, &id, TorrentState::Seeding).await;

    let setting = MoveCompleted {
        enabled: true,
        to: public.to_string_lossy().into_owned(),
        when: MoveWhen::EachFile,
        rules: vec![
            MoveRule {
                pattern: "*.nfo".into(),
                to: String::new(),
            },
            MoveRule {
                pattern: "*.mkv".into(),
                to: video.to_string_lossy().into_owned(),
            },
        ],
    };
    assert!(
        core.set_move_completed(
            &k(&id),
            Some(MoveCompleted {
                to: "relative/dir".into(),
                ..setting.clone()
            })
        )
        .await
        .is_err(),
        "destinations are full paths"
    );
    core.set_move_completed(&k(&id), Some(setting.clone()))
        .await
        .unwrap();
    let d = wait_detail(&core, &id, "the moves", |d| {
        moved(d, "a.mkv").is_some() && moved(d, "c.jpg").is_some()
    })
    .await;
    assert_eq!(moved(&d, "a.mkv"), Some(video.join("Show").join("a.mkv")));
    assert_eq!(
        moved(&d, "c.jpg"),
        Some(public.join("Show").join("extras").join("c.jpg"))
    );
    assert_eq!(
        moved(&d, "info.nfo"),
        None,
        "a rule with no destination keeps it"
    );
    assert_eq!(d.move_completed.as_ref(), Some(&setting));
    assert_eq!(std::fs::read(video.join("Show").join("a.mkv")).unwrap(), a);
    assert!(!dir.join("a.mkv").exists());
    assert!(!dir.join("extras").exists(), "emptied folders go");
    assert!(dir.join("info.nfo").exists());
    assert_eq!(
        core.store().get(&k(&id)).unwrap().state,
        TorrentState::Seeding
    );

    core.shutdown().await.unwrap();
    drop(core);
    let core = Core::start(cfg.clone()).await.unwrap();
    wait_state(&core, &id, TorrentState::Seeding).await;
    let d = core.detail(&k(&id)).await.unwrap();
    assert_eq!(
        moved(&d, "a.mkv"),
        Some(video.join("Show").join("a.mkv")),
        "kept across a restart"
    );
    let s = core.store().get(&k(&id)).unwrap();
    assert_eq!(s.pieces_have, s.pieces_total);

    core.remove(&k(&id), true).await.unwrap();
    assert!(
        !video.join("Show").join("a.mkv").exists(),
        "deleted where it was moved"
    );
    assert!(!public.join("Show").join("extras").join("c.jpg").exists());
    core.shutdown().await.unwrap();
}

/// A tracker that hands out one peer (an IPv4 address) to everyone.
async fn tracker_with_peer(peer: std::net::SocketAddr) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let std::net::SocketAddr::V4(v4) = peer else {
        panic!("IPv4 only")
    };
    let mut body = b"d8:intervali60e5:peers6:".to_vec();
    body.extend_from_slice(&v4.ip().octets());
    body.extend_from_slice(&v4.port().to_be_bytes());
    body.push(b'e');
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let body = body.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
            });
        }
    });
    format!("http://{addr}/announce")
}

/// Downloading from a peer, with the instance's defaults: files in name order, each moved
/// as it completes. The torrent's own order and sequential files are kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_move_as_they_complete_while_downloading() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let public = root.path().join("public");
    let (ep1, ep2) = (data(300_000, 4), data(200_000, 5));

    let seeder = Session::start(SessionConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        upload_rate: 1024 * 1024,
        ..SessionConfig::default()
    })
    .await
    .unwrap();
    let tracker = tracker_with_peer(seeder.listen_addr()).await;
    let bytes = TorrentBuilder::new("Season", Version::Hybrid)
        .piece_length(16 * 1024)
        .file(FileSource::from_bytes(&["ep2.mkv"], ep2.clone()))
        .file(FileSource::from_bytes(&["ep1.mkv"], ep1.clone()))
        .tracker_tier(vec![tracker])
        .build()
        .unwrap();
    let seed_dir = tempfile::tempdir().unwrap();
    let season = seed_dir.path().join("Season");
    std::fs::create_dir_all(&season).unwrap();
    std::fs::write(season.join("ep1.mkv"), &ep1).unwrap();
    std::fs::write(season.join("ep2.mkv"), &ep2).unwrap();
    let seeding = seeder
        .add_torrent(AddTorrent::new(
            Arc::new(Torrent::from_bytes(&bytes).unwrap()),
            seed_dir.path().to_path_buf(),
        ))
        .unwrap();
    seeding.wait_finished().await;

    let core = Core::start(cfg.clone()).await.unwrap();
    let mut settings = core.settings();
    settings.file_order = FileOrder::Name;
    settings.move_completed = MoveCompleted {
        enabled: true,
        to: public.to_string_lossy().into_owned(),
        ..MoveCompleted::default()
    };
    core.set_settings(settings).await.unwrap();
    let id = core.add(DEFAULT_SPACE, add(&bytes)).await.unwrap();
    let d = core.detail(&k(&id)).await.unwrap();
    assert_eq!(d.file_order, None);
    assert_eq!(d.effective_file_order, FileOrder::Name);
    let ep1_index = d
        .files
        .iter()
        .find(|f| f.path.ends_with("ep1.mkv"))
        .unwrap()
        .index;
    core.set_sequential(
        &k(&id),
        &[SequentialFile {
            index: ep1_index,
            sequential: true,
            from: 0,
        }],
    )
    .await
    .unwrap();
    core.set_file_order(&k(&id), Some(FileOrder::LargestFirst))
        .await
        .unwrap();

    wait_state(&core, &id, TorrentState::Seeding).await;
    let d = wait_detail(&core, &id, "both moves", |d| {
        moved(d, "ep1.mkv").is_some() && moved(d, "ep2.mkv").is_some()
    })
    .await;
    assert_eq!(
        std::fs::read(public.join("Season").join("ep1.mkv")).unwrap(),
        ep1
    );
    assert_eq!(
        std::fs::read(public.join("Season").join("ep2.mkv")).unwrap(),
        ep2
    );
    assert_eq!(d.file_order, Some(FileOrder::LargestFirst));
    assert!(
        d.files
            .iter()
            .find(|f| f.index == ep1_index)
            .unwrap()
            .sequential
    );
    assert!(d.files.iter().all(|f| f.move_error.is_none()));

    core.shutdown().await.unwrap();
    drop(core);
    let core = Core::start(cfg).await.unwrap();
    let d = core.detail(&k(&id)).await.unwrap();
    assert_eq!(
        d.file_order,
        Some(FileOrder::LargestFirst),
        "kept across a restart"
    );
    assert!(
        d.files
            .iter()
            .find(|f| f.index == ep1_index)
            .unwrap()
            .sequential
    );
    core.shutdown().await.unwrap();
    seeding.shutdown().await;
}

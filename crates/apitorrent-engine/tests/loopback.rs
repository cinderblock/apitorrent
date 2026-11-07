//! End-to-end transfers between real sessions over loopback TCP.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use apitorrent_engine::bitfield::Bitfield;
use apitorrent_engine::{AddTorrent, Session, SessionConfig, TorrentState};
use apitorrent_metainfo::{FileSource, Torrent, TorrentBuilder, Version};

fn data(len: usize, seed: u64) -> Vec<u8> {
    // A cheap deterministic generator, so pieces differ from each other.
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x.to_le_bytes()[0]
        })
        .collect()
}

struct Fixture {
    torrent: Arc<Torrent>,
    files: Vec<(Vec<String>, Vec<u8>)>,
}

fn fixture(version: Version) -> Fixture {
    let files: Vec<(Vec<String>, Vec<u8>)> = vec![
        (vec!["big.bin".into()], data(1_500_000, 1)),
        (vec!["dir".into(), "small.txt".into()], data(70_000, 2)),
        (vec!["dir".into(), "empty".into()], Vec::new()),
        (vec!["tail.bin".into()], data(400_123, 3)),
    ];
    let mut b = TorrentBuilder::new("pack", version).piece_length(64 * 1024);
    for (path, bytes) in &files {
        let p: Vec<&str> = path.iter().map(String::as_str).collect();
        b = b.file(FileSource::from_bytes(&p, bytes.clone()));
    }
    let torrent = Arc::new(Torrent::from_bytes(&b.build().unwrap()).unwrap());
    Fixture { torrent, files }
}

fn write_files(root: &Path, fx: &Fixture) {
    for (path, bytes) in &fx.files {
        let mut p = root.join("pack");
        for c in path {
            p.push(c);
        }
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
    }
}

fn assert_files(root: &Path, fx: &Fixture) {
    for (path, bytes) in &fx.files {
        let mut p = root.join("pack");
        for c in path {
            p.push(c);
        }
        let got = std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        assert!(got == *bytes, "{} differs", p.display());
    }
}

fn local_config() -> SessionConfig {
    SessionConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ..SessionConfig::default()
    }
}

async fn transfer(version: Version) {
    let fx = fixture(version);
    let seed_dir = tempfile::tempdir().unwrap();
    let leech_dir = tempfile::tempdir().unwrap();
    write_files(seed_dir.path(), &fx);

    let seeder = Session::start(local_config()).await.unwrap();
    let seed = seeder
        .add_torrent(AddTorrent {
            torrent: fx.torrent.clone(),
            save_path: seed_dir.path().to_path_buf(),
            have: None,
            paused: false,
            peers: Vec::new(),
            file_priorities: None,
        })
        .unwrap();
    let stats = tokio::time::timeout(Duration::from_secs(30), seed.wait_finished())
        .await
        .expect("seeder check timed out");
    assert_eq!(
        stats.pieces_have, stats.pieces_total,
        "{version:?} seeder check"
    );

    let leecher = Session::start(local_config()).await.unwrap();
    let pieces = u32::try_from(fx.torrent.info.piece_count()).unwrap();
    let leech = leecher
        .add_torrent(AddTorrent {
            torrent: fx.torrent.clone(),
            save_path: leech_dir.path().to_path_buf(),
            have: Some(Bitfield::new(pieces)),
            paused: false,
            peers: vec![seeder.listen_addr()],
            file_priorities: None,
        })
        .unwrap();
    let stats = tokio::time::timeout(Duration::from_secs(60), leech.wait_finished())
        .await
        .unwrap_or_else(|_| panic!("{version:?} download timed out: {:?}", leech.stats()));
    assert_eq!(stats.state, TorrentState::Seeding);
    assert_eq!(stats.pieces_have, pieces);
    assert!(stats.downloaded >= fx.torrent.info.content_length());
    let have = leech.shutdown().await.unwrap();
    assert!(have.is_full());
    assert_files(leech_dir.path(), &fx);
    seed.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v1_transfer() {
    transfer(Version::V1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_transfer() {
    transfer(Version::V2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hybrid_transfer() {
    transfer(Version::Hybrid).await;
}

/// One seeder and three leechers that only know the seeder and each other: pieces must flow
/// between leechers too, since the seeder has fewer upload slots than there are leechers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn small_swarm() {
    let fx = fixture(Version::Hybrid);
    let seed_dir = tempfile::tempdir().unwrap();
    write_files(seed_dir.path(), &fx);
    let seeder = Session::start(SessionConfig {
        upload_slots: 1,
        ..local_config()
    })
    .await
    .unwrap();
    let seed = seeder
        .add_torrent(AddTorrent {
            torrent: fx.torrent.clone(),
            save_path: seed_dir.path().to_path_buf(),
            have: None,
            paused: false,
            peers: Vec::new(),
            file_priorities: None,
        })
        .unwrap();
    seed.wait_finished().await;

    let pieces = u32::try_from(fx.torrent.info.piece_count()).unwrap();
    let mut sessions = Vec::new();
    let mut dirs = Vec::new();
    for _ in 0..3 {
        sessions.push(Session::start(local_config()).await.unwrap());
        dirs.push(tempfile::tempdir().unwrap());
    }
    let addrs: Vec<_> = sessions.iter().map(Session::listen_addr).collect();
    let mut handles = Vec::new();
    for (i, s) in sessions.iter().enumerate() {
        let mut peers = vec![seeder.listen_addr()];
        peers.extend(
            addrs
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, a)| *a),
        );
        handles.push(
            s.add_torrent(AddTorrent {
                torrent: fx.torrent.clone(),
                save_path: dirs[i].path().to_path_buf(),
                have: Some(Bitfield::new(pieces)),
                paused: false,
                peers,
                file_priorities: None,
            })
            .unwrap(),
        );
    }
    for (i, h) in handles.iter().enumerate() {
        tokio::time::timeout(Duration::from_secs(90), h.wait_finished())
            .await
            .unwrap_or_else(|_| panic!("leecher {i} timed out: {:?}", h.stats()));
    }
    for d in &dirs {
        assert_files(d.path(), &fx);
    }
    let from_leechers: u64 = handles.iter().map(|h| h.stats().uploaded).sum();
    assert!(
        from_leechers > 0,
        "leechers should have shared pieces with each other"
    );
}

/// A file with priority 0 is not downloaded. In a hybrid torrent files are piece-aligned,
/// so a skipped file shares no piece with a wanted one and is never created.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skipped_files_are_not_downloaded() {
    let fx = fixture(Version::Hybrid);
    let seed_dir = tempfile::tempdir().unwrap();
    let leech_dir = tempfile::tempdir().unwrap();
    write_files(seed_dir.path(), &fx);
    let seeder = Session::start(local_config()).await.unwrap();
    let seed = seeder
        .add_torrent(AddTorrent {
            torrent: fx.torrent.clone(),
            save_path: seed_dir.path().to_path_buf(),
            have: None,
            paused: false,
            peers: Vec::new(),
            file_priorities: None,
        })
        .unwrap();
    seed.wait_finished().await;

    // Skip the big file; keep everything else (pad entries get 0 too, which is ignored).
    let prios: Vec<u8> = fx
        .torrent
        .info
        .files
        .iter()
        .map(|f| u8::from(f.path.last().is_some_and(|n| n != "big.bin")) * 4)
        .collect();
    let leecher = Session::start(local_config()).await.unwrap();
    let pieces = u32::try_from(fx.torrent.info.piece_count()).unwrap();
    let leech = leecher
        .add_torrent(AddTorrent {
            torrent: fx.torrent.clone(),
            save_path: leech_dir.path().to_path_buf(),
            have: Some(Bitfield::new(pieces)),
            paused: false,
            peers: vec![seeder.listen_addr()],
            file_priorities: Some(prios),
        })
        .unwrap();
    let stats = tokio::time::timeout(Duration::from_secs(60), leech.wait_finished())
        .await
        .expect("download timed out");
    assert!(stats.pieces_have < stats.pieces_total);
    let root = leech_dir.path().join("pack");
    assert!(!root.join("big.bin").exists());
    assert_eq!(std::fs::read(root.join("tail.bin")).unwrap(), fx.files[3].1);
    assert_eq!(
        std::fs::read(root.join("dir").join("small.txt")).unwrap(),
        fx.files[1].1
    );
    assert!(root.join("dir").join("empty").exists());
}

/// A storage error stops the torrent instead of downloading data that can't be stored. The
/// first real-world test kept downloading (about 500 MB) after every write had failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disk_error_stops_the_torrent() {
    let fx = fixture(Version::V1);
    let seed_dir = tempfile::tempdir().unwrap();
    let leech_dir = tempfile::tempdir().unwrap();
    write_files(seed_dir.path(), &fx);
    // A plain file where the torrent's directory should go, so every write fails.
    std::fs::write(leech_dir.path().join("pack"), b"in the way").unwrap();

    let seeder = Session::start(local_config()).await.unwrap();
    let seed = seeder
        .add_torrent(AddTorrent {
            torrent: fx.torrent.clone(),
            save_path: seed_dir.path().to_path_buf(),
            have: None,
            paused: false,
            peers: Vec::new(),
            file_priorities: None,
        })
        .unwrap();
    seed.wait_finished().await;

    let leecher = Session::start(local_config()).await.unwrap();
    let pieces = u32::try_from(fx.torrent.info.piece_count()).unwrap();
    let leech = leecher
        .add_torrent(AddTorrent {
            torrent: fx.torrent.clone(),
            save_path: leech_dir.path().to_path_buf(),
            have: Some(Bitfield::new(pieces)),
            paused: false,
            peers: vec![seeder.listen_addr()],
            file_priorities: None,
        })
        .unwrap();
    let mut watch = leech.watch();
    let stats = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let s = watch.borrow_and_update().clone();
            if s.state == TorrentState::Error {
                return s;
            }
            watch.changed().await.unwrap();
        }
    })
    .await
    .expect("torrent never reported the disk error");
    assert!(stats.error.is_some());
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = leech.stats();
    assert_eq!(after.state, TorrentState::Error);
    assert_eq!(after.peers, 0, "peers are disconnected");
    let downloaded = after.downloaded;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        leech.stats().downloaded,
        downloaded,
        "nothing more is downloaded"
    );
    seed.shutdown().await;
}

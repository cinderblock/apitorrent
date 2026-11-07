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
            have: None,
            paused: false,
            peers: Vec::new(),
            file_priorities: None,
            ..AddTorrent::new(fx.torrent.clone(), seed_dir.path().to_path_buf())
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
            have: Some(Bitfield::new(pieces)),
            paused: false,
            peers: vec![seeder.listen_addr()],
            file_priorities: None,
            ..AddTorrent::new(fx.torrent.clone(), leech_dir.path().to_path_buf())
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
            have: None,
            paused: false,
            peers: Vec::new(),
            file_priorities: None,
            ..AddTorrent::new(fx.torrent.clone(), seed_dir.path().to_path_buf())
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
                have: Some(Bitfield::new(pieces)),
                paused: false,
                peers,
                file_priorities: None,
                ..AddTorrent::new(fx.torrent.clone(), dirs[i].path().to_path_buf())
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
            have: None,
            paused: false,
            peers: Vec::new(),
            file_priorities: None,
            ..AddTorrent::new(fx.torrent.clone(), seed_dir.path().to_path_buf())
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
            have: Some(Bitfield::new(pieces)),
            paused: false,
            peers: vec![seeder.listen_addr()],
            file_priorities: Some(prios),
            ..AddTorrent::new(fx.torrent.clone(), leech_dir.path().to_path_buf())
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
            have: None,
            paused: false,
            peers: Vec::new(),
            file_priorities: None,
            ..AddTorrent::new(fx.torrent.clone(), seed_dir.path().to_path_buf())
        })
        .unwrap();
    seed.wait_finished().await;

    let leecher = Session::start(local_config()).await.unwrap();
    let pieces = u32::try_from(fx.torrent.info.piece_count()).unwrap();
    let leech = leecher
        .add_torrent(AddTorrent {
            have: Some(Bitfield::new(pieces)),
            paused: false,
            peers: vec![seeder.listen_addr()],
            file_priorities: None,
            ..AddTorrent::new(fx.torrent.clone(), leech_dir.path().to_path_buf())
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

/// Start a seeder for the fixture and return it with its handle.
async fn seed_fixture(fx: &Fixture, dir: &Path) -> (Session, apitorrent_engine::TorrentHandle) {
    write_files(dir, fx);
    let seeder = Session::start(local_config()).await.unwrap();
    let seed = seeder
        .add_torrent(AddTorrent::new(fx.torrent.clone(), dir.to_path_buf()))
        .unwrap();
    seed.wait_finished().await;
    (seeder, seed)
}

/// Priorities that skip the files named in `skip`.
fn skipping(fx: &Fixture, skip: &[&str]) -> Vec<u8> {
    fx.torrent
        .info
        .files
        .iter()
        .map(|f| {
            let name = f.path.last().map_or("", String::as_str);
            if f.is_pad() || skip.contains(&name) {
                0
            } else {
                4
            }
        })
        .collect()
}

async fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// In a v1 torrent `dir/small.txt` shares both of its pieces with its neighbours, so those
/// pieces must be downloaded even when it is skipped. Its bytes go to the part store and
/// it never appears in the save directory.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skipped_file_bytes_go_to_the_part_store() {
    let fx = fixture(Version::V1);
    let seed_dir = tempfile::tempdir().unwrap();
    let leech_dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let (seeder, _seed) = seed_fixture(&fx, seed_dir.path()).await;

    let leecher = Session::start(local_config()).await.unwrap();
    let pieces = u32::try_from(fx.torrent.info.piece_count()).unwrap();
    let parts = state.path().join("t.parts");
    let leech = leecher
        .add_torrent(AddTorrent {
            have: Some(Bitfield::new(pieces)),
            peers: vec![seeder.listen_addr()],
            file_priorities: Some(skipping(&fx, &["small.txt"])),
            parts_path: Some(parts.clone()),
            held_path: Some(state.path().join("held")),
            ..AddTorrent::new(fx.torrent.clone(), leech_dir.path().to_path_buf())
        })
        .unwrap();
    let stats = tokio::time::timeout(Duration::from_secs(60), leech.wait_finished())
        .await
        .expect("download timed out");
    assert_eq!(
        stats.pieces_have, stats.pieces_total,
        "every piece touches a wanted file"
    );
    let root = leech_dir.path().join("pack");
    assert!(!root.join("dir").join("small.txt").exists());
    assert_eq!(std::fs::read(root.join("big.bin")).unwrap(), fx.files[0].1);
    assert_eq!(std::fs::read(root.join("tail.bin")).unwrap(), fx.files[3].1);
    assert!(parts.exists(), "boundary bytes are in the part store");
    assert!(
        !leech_dir.path().join(".apitorrent").exists(),
        "nothing internal in the save directory"
    );

    // Selecting it again restores it from the part store, with no download.
    let downloaded = leech.stats().downloaded;
    leech.set_file_priorities(skipping(&fx, &[])).await;
    let small = root.join("dir").join("small.txt");
    wait_for("small.txt to come back", || {
        std::fs::read(&small).ok().as_deref() == Some(fx.files[1].1.as_slice())
    })
    .await;
    assert_eq!(leech.stats().downloaded, downloaded);
    wait_for("the empty part store to be deleted", || !parts.exists()).await;
}

/// Deselecting a downloaded file moves it out of the save directory: to the held area by
/// default, or away entirely with the delete policy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deselecting_a_file_holds_or_deletes_it() {
    let fx = fixture(Version::V1);
    let seed_dir = tempfile::tempdir().unwrap();
    let leech_dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let (seeder, _seed) = seed_fixture(&fx, seed_dir.path()).await;

    let leecher = Session::start(local_config()).await.unwrap();
    let pieces = u32::try_from(fx.torrent.info.piece_count()).unwrap();
    let held = state.path().join("held");
    let leech = leecher
        .add_torrent(AddTorrent {
            have: Some(Bitfield::new(pieces)),
            peers: vec![seeder.listen_addr()],
            parts_path: Some(state.path().join("t.parts")),
            held_path: Some(held.clone()),
            ..AddTorrent::new(fx.torrent.clone(), leech_dir.path().to_path_buf())
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(60), leech.wait_finished())
        .await
        .expect("download timed out");
    let root = leech_dir.path().join("pack");
    let big = root.join("big.bin");
    let held_big = held.join("pack").join("big.bin");

    // Hold: the file moves to the held area and the torrent keeps every piece.
    leech.set_file_priorities(skipping(&fx, &["big.bin"])).await;
    wait_for("big.bin to be held", || held_big.exists() && !big.exists()).await;
    assert_eq!(std::fs::read(&held_big).unwrap(), fx.files[0].1);
    assert_eq!(leech.stats().pieces_have, pieces);

    // Select it again: it moves back.
    leech.set_file_priorities(skipping(&fx, &[])).await;
    wait_for("big.bin to come back", || {
        big.exists() && !held_big.exists()
    })
    .await;
    assert_eq!(std::fs::read(&big).unwrap(), fx.files[0].1);

    // Delete: only the boundary piece it shares with small.txt is kept.
    leech
        .set_unwanted_policy(apitorrent_engine::UnwantedPolicy::Delete)
        .await;
    leech.set_file_priorities(skipping(&fx, &["big.bin"])).await;
    wait_for("big.bin to be deleted", || !big.exists()).await;
    assert!(!held_big.exists());
    let exclusive = u32::try_from(1_500_000 / (64 * 1024)).unwrap();
    wait_for("pieces to drop", || {
        leech.stats().pieces_have == pieces - exclusive
    })
    .await;
    assert!(root.join("dir").join("small.txt").exists());

    // Select it again: the dropped pieces are downloaded again.
    leech.set_file_priorities(skipping(&fx, &[])).await;
    tokio::time::timeout(Duration::from_secs(60), async {
        leech
            .watch()
            .wait_for(|s| s.pieces_have == pieces)
            .await
            .unwrap();
    })
    .await
    .expect("re-download timed out");
    wait_for("big.bin to be complete", || {
        std::fs::read(&big).ok().as_deref() == Some(fx.files[0].1.as_slice())
    })
    .await;
}

/// Deleting a torrent's data removes its files, held copies and part store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_data_removes_everything() {
    let fx = fixture(Version::V1);
    let seed_dir = tempfile::tempdir().unwrap();
    let leech_dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let (seeder, _seed) = seed_fixture(&fx, seed_dir.path()).await;
    let leecher = Session::start(local_config()).await.unwrap();
    let pieces = u32::try_from(fx.torrent.info.piece_count()).unwrap();
    let held = state.path().join("held");
    let parts = state.path().join("t.parts");
    let leech = leecher
        .add_torrent(AddTorrent {
            have: Some(Bitfield::new(pieces)),
            peers: vec![seeder.listen_addr()],
            file_priorities: Some(skipping(&fx, &["small.txt"])),
            parts_path: Some(parts.clone()),
            held_path: Some(held.clone()),
            ..AddTorrent::new(fx.torrent.clone(), leech_dir.path().to_path_buf())
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(60), leech.wait_finished())
        .await
        .expect("download timed out");
    leech
        .set_file_priorities(skipping(&fx, &["small.txt", "tail.bin"]))
        .await;
    let held_tail = held.join("pack").join("tail.bin");
    wait_for("tail.bin to be held", || held_tail.exists()).await;

    leech.shutdown().await;
    leech.delete_data(true).await.unwrap();
    assert!(
        !leech_dir.path().join("pack").exists(),
        "torrent directory removed"
    );
    assert!(!held.exists());
    assert!(!parts.exists());
}

/// Pausing closes every connection; resuming dials the same peers again. Before this was
/// fixed, peers dropped by a pause were forgotten for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_reconnects_to_known_peers() {
    let fx = fixture(Version::V1);
    let seed_dir = tempfile::tempdir().unwrap();
    let leech_dir = tempfile::tempdir().unwrap();
    let (seeder, _seed) = seed_fixture(&fx, seed_dir.path()).await;
    let leecher = Session::start(local_config()).await.unwrap();
    let pieces = u32::try_from(fx.torrent.info.piece_count()).unwrap();
    let leech = leecher
        .add_torrent(AddTorrent {
            have: Some(Bitfield::new(pieces)),
            peers: vec![seeder.listen_addr()],
            paused: true,
            ..AddTorrent::new(fx.torrent.clone(), leech_dir.path().to_path_buf())
        })
        .unwrap();
    leech.resume().await;
    wait_for("a connection", || leech.stats().peers > 0).await;
    leech.pause().await;
    wait_for("the pause", || leech.stats().peers == 0).await;
    leech.resume().await;
    tokio::time::timeout(Duration::from_secs(60), leech.wait_finished())
        .await
        .unwrap_or_else(|_| panic!("download after resume timed out: {:?}", leech.stats()));
    assert_files(leech_dir.path(), &fx);
}

/// A transfer under a two-piece memory budget still completes, and never holds more than
/// the budget. (With one torrent the "always start one piece" rule can't exceed it: that
/// only applies when the torrent holds nothing.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_budget_is_respected() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let fx = fixture(Version::V1);
    let seed_dir = tempfile::tempdir().unwrap();
    let leech_dir = tempfile::tempdir().unwrap();
    let (seeder, _seed) = seed_fixture(&fx, seed_dir.path()).await;
    let piece = fx.torrent.info.piece_length;
    let leecher = Session::start(SessionConfig {
        memory_limit: 2 * piece,
        ..local_config()
    })
    .await
    .unwrap();
    let pieces = u32::try_from(fx.torrent.info.piece_count()).unwrap();
    let leech = leecher
        .add_torrent(AddTorrent {
            have: Some(Bitfield::new(pieces)),
            peers: vec![seeder.listen_addr()],
            ..AddTorrent::new(fx.torrent.clone(), leech_dir.path().to_path_buf())
        })
        .unwrap();
    let peak = Arc::new(AtomicU64::new(0));
    let sampler = {
        let (leecher, peak) = (leecher.clone(), peak.clone());
        tokio::spawn(async move {
            loop {
                peak.fetch_max(leecher.memory_used(), Ordering::Relaxed);
                tokio::time::sleep(Duration::from_micros(200)).await;
            }
        })
    };
    tokio::time::timeout(Duration::from_secs(60), leech.wait_finished())
        .await
        .expect("download timed out");
    sampler.abort();
    assert_files(leech_dir.path(), &fx);
    let peak = peak.load(Ordering::Relaxed);
    assert!(peak > 0, "the sampler saw pieces in progress");
    assert!(
        peak <= 2 * piece,
        "peak {peak} is over the {} budget",
        2 * piece
    );
    wait_for("memory to be released", || leecher.memory_used() == 0).await;
}

/// A seeder that claims every piece but serves corrupted data, listening on its own
/// loopback address so it can be banned without banning the honest seeder.
async fn lying_seeder(
    fx: &Fixture,
    dir: &Path,
    ip: &str,
) -> (Session, apitorrent_engine::TorrentHandle) {
    write_files(dir, fx);
    // Corrupt a few bytes in every 64 KiB piece of big.bin.
    let big = dir.join("pack").join("big.bin");
    let mut bytes = std::fs::read(&big).unwrap();
    for at in (100..bytes.len()).step_by(64 * 1024) {
        bytes[at] ^= 0xff;
    }
    std::fs::write(&big, bytes).unwrap();
    let liar = Session::start(SessionConfig {
        listen: format!("{ip}:0").parse().unwrap(),
        ..SessionConfig::default()
    })
    .await
    .unwrap();
    let pieces = u32::try_from(fx.torrent.info.piece_count()).unwrap();
    let handle = liar
        .add_torrent(AddTorrent {
            // Resume data that claims everything, without a recheck.
            have: Some(Bitfield::full(pieces)),
            ..AddTorrent::new(fx.torrent.clone(), dir.to_path_buf())
        })
        .unwrap();
    handle.wait_finished().await;
    (liar, handle)
}

/// With an honest seeder and a lying one, the download still completes with the right
/// data, and only the liar is banned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_sending_bad_data_is_banned() {
    let fx = fixture(Version::V1);
    let good_dir = tempfile::tempdir().unwrap();
    let bad_dir = tempfile::tempdir().unwrap();
    let leech_dir = tempfile::tempdir().unwrap();
    let (good, _g) = seed_fixture(&fx, good_dir.path()).await;
    let (liar, _l) = lying_seeder(&fx, bad_dir.path(), "127.0.0.2").await;

    let leecher = Session::start(local_config()).await.unwrap();
    let pieces = u32::try_from(fx.torrent.info.piece_count()).unwrap();
    let leech = leecher
        .add_torrent(AddTorrent {
            have: Some(Bitfield::new(pieces)),
            peers: vec![liar.listen_addr()],
            ..AddTorrent::new(fx.torrent.clone(), leech_dir.path().to_path_buf())
        })
        .unwrap();
    // Alone, the liar sends whole pieces, which fail: proof enough for a ban. Only then
    // does the honest seeder join, so the test doesn't depend on who answers first.
    let liar_ip: std::net::IpAddr = "127.0.0.2".parse().unwrap();
    let honest_ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
    wait_for("the liar to be banned", || leecher.is_blocked(liar_ip)).await;
    leech.add_peers(vec![good.listen_addr()]).await;
    tokio::time::timeout(Duration::from_secs(60), leech.wait_finished())
        .await
        .unwrap_or_else(|_| panic!("download timed out: {:?}", leech.stats()));
    assert_files(leech_dir.path(), &fx);
    assert!(
        leecher.is_blocked(liar_ip),
        "the liar is banned: {:?}",
        leecher.bans()
    );
    assert!(
        !leecher.is_blocked(honest_ip),
        "the honest seeder is not: {:?}",
        leecher.bans()
    );
    let bans = leecher.bans();
    assert_eq!(bans.len(), 1);
    assert_eq!(bans[0].reason, apitorrent_engine::ban::BanReason::BadData);
}

/// A blocklisted address is never connected to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocklisted_peers_are_refused() {
    let fx = fixture(Version::V1);
    let blocked_dir = tempfile::tempdir().unwrap();
    let good_dir = tempfile::tempdir().unwrap();
    let leech_dir = tempfile::tempdir().unwrap();
    let (good, _g) = seed_fixture(&fx, good_dir.path()).await;
    write_files(blocked_dir.path(), &fx);
    let blocked = Session::start(SessionConfig {
        listen: "127.0.0.3:0".parse().unwrap(),
        ..SessionConfig::default()
    })
    .await
    .unwrap();
    let blocked_handle = blocked
        .add_torrent(AddTorrent::new(
            fx.torrent.clone(),
            blocked_dir.path().to_path_buf(),
        ))
        .unwrap();
    blocked_handle.wait_finished().await;

    let leecher = Session::start(local_config()).await.unwrap();
    let (list, errors) = apitorrent_engine::ban::Blocklist::parse("test:127.0.0.3-127.0.0.3");
    assert!(errors.is_empty());
    leecher.set_blocklist(list);
    let pieces = u32::try_from(fx.torrent.info.piece_count()).unwrap();
    let leech = leecher
        .add_torrent(AddTorrent {
            have: Some(Bitfield::new(pieces)),
            peers: vec![blocked.listen_addr(), good.listen_addr()],
            ..AddTorrent::new(fx.torrent.clone(), leech_dir.path().to_path_buf())
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(60), leech.wait_finished())
        .await
        .expect("download timed out");
    assert_eq!(
        blocked_handle.stats().uploaded,
        0,
        "nothing came from the blocked peer"
    );
}

/// A torrent download rate limit holds: about 2 MB at 1 MB/s takes most of 2 seconds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::cast_precision_loss)]
async fn download_rate_limit_holds() {
    let fx = fixture(Version::V1);
    let seed_dir = tempfile::tempdir().unwrap();
    let leech_dir = tempfile::tempdir().unwrap();
    let (seeder, _seed) = seed_fixture(&fx, seed_dir.path()).await;
    let leecher = Session::start(local_config()).await.unwrap();
    let pieces = u32::try_from(fx.torrent.info.piece_count()).unwrap();
    let rate = 1_000_000;
    let start = std::time::Instant::now();
    let leech = leecher
        .add_torrent(AddTorrent {
            have: Some(Bitfield::new(pieces)),
            peers: vec![seeder.listen_addr()],
            limits: apitorrent_engine::limits::TorrentLimits {
                download_rate: rate,
                ..Default::default()
            },
            ..AddTorrent::new(fx.torrent.clone(), leech_dir.path().to_path_buf())
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(60), leech.wait_finished())
        .await
        .expect("download timed out");
    let elapsed = start.elapsed().as_secs_f64();
    let size = fx.torrent.info.content_length() as f64;
    // The burst lets a quarter second through for free.
    let floor = size / rate as f64 - 0.3;
    assert!(
        elapsed >= floor,
        "took {elapsed:.2}s, expected at least {floor:.2}s"
    );
    assert_files(leech_dir.path(), &fx);
}

/// A session upload limit on the seeder holds too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::cast_precision_loss)]
async fn upload_rate_limit_holds() {
    let fx = fixture(Version::V1);
    let seed_dir = tempfile::tempdir().unwrap();
    let leech_dir = tempfile::tempdir().unwrap();
    write_files(seed_dir.path(), &fx);
    let rate = 1_000_000;
    let seeder = Session::start(SessionConfig {
        upload_rate: rate,
        ..local_config()
    })
    .await
    .unwrap();
    let seed = seeder
        .add_torrent(AddTorrent::new(
            fx.torrent.clone(),
            seed_dir.path().to_path_buf(),
        ))
        .unwrap();
    seed.wait_finished().await;
    let leecher = Session::start(local_config()).await.unwrap();
    let pieces = u32::try_from(fx.torrent.info.piece_count()).unwrap();
    let start = std::time::Instant::now();
    let leech = leecher
        .add_torrent(AddTorrent {
            have: Some(Bitfield::new(pieces)),
            peers: vec![seeder.listen_addr()],
            ..AddTorrent::new(fx.torrent.clone(), leech_dir.path().to_path_buf())
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(60), leech.wait_finished())
        .await
        .expect("download timed out");
    let elapsed = start.elapsed().as_secs_f64();
    let floor = fx.torrent.info.content_length() as f64 / rate as f64 - 0.3;
    assert!(
        elapsed >= floor,
        "took {elapsed:.2}s, expected at least {floor:.2}s"
    );
}

/// With a session limit of one connection, two seeders are never connected at once, and
/// the download still completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_limit_holds() {
    let fx = fixture(Version::V1);
    let a_dir = tempfile::tempdir().unwrap();
    let b_dir = tempfile::tempdir().unwrap();
    let leech_dir = tempfile::tempdir().unwrap();
    let (a, _a) = seed_fixture(&fx, a_dir.path()).await;
    let (b, _b) = seed_fixture(&fx, b_dir.path()).await;
    let leecher = Session::start(SessionConfig {
        max_connections: 1,
        ..local_config()
    })
    .await
    .unwrap();
    let pieces = u32::try_from(fx.torrent.info.piece_count()).unwrap();
    let leech = leecher
        .add_torrent(AddTorrent {
            have: Some(Bitfield::new(pieces)),
            peers: vec![a.listen_addr(), b.listen_addr()],
            ..AddTorrent::new(fx.torrent.clone(), leech_dir.path().to_path_buf())
        })
        .unwrap();
    let peak = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sampler = {
        let (leecher, peak) = (leecher.clone(), peak.clone());
        tokio::spawn(async move {
            loop {
                peak.fetch_max(leecher.connections(), std::sync::atomic::Ordering::Relaxed);
                tokio::time::sleep(Duration::from_micros(200)).await;
            }
        })
    };
    tokio::time::timeout(Duration::from_secs(60), leech.wait_finished())
        .await
        .expect("download timed out");
    sampler.abort();
    assert_eq!(peak.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_files(leech_dir.path(), &fx);
}

async fn magnet_then_download(version: Version) {
    use apitorrent_metainfo::Magnet;

    let fx = fixture(version);
    let seed_dir = tempfile::tempdir().unwrap();
    let leech_dir = tempfile::tempdir().unwrap();
    let (seeder, _seed) = seed_fixture(&fx, seed_dir.path()).await;

    // A magnet link with only the v1 hash (btih) and the seeder as an `x.pe` peer, as
    // most real links look.
    let hashes = fx.torrent.info.info_hashes();
    let v1 = hashes.get_v1().unwrap().to_hex();
    let uri = format!(
        "magnet:?xt=urn:btih:{v1}&dn=pack&x.pe={}",
        seeder.listen_addr()
    );
    let magnet = Magnet::parse(&uri).unwrap();

    let leecher = Session::start(local_config()).await.unwrap();
    let mut fetch = leecher.fetch_metadata(&magnet);
    let fetched = tokio::time::timeout(Duration::from_secs(30), fetch.wait())
        .await
        .unwrap_or_else(|_| panic!("metadata fetch timed out: {:?}", fetch.status()))
        .expect("fetch finished");
    assert_eq!(&fetched.info[..], fx.torrent.info.raw(), "{version:?}");
    assert!(fetched.peers.contains(&seeder.listen_addr()));

    let bytes = apitorrent_engine::metadata::torrent_file(&fetched.info, &[], &[]);
    let torrent = Arc::new(Torrent::from_bytes(&bytes).unwrap());
    assert_eq!(torrent.info.info_hashes(), hashes);
    let pieces = u32::try_from(torrent.info.piece_count()).unwrap();
    let leech = leecher
        .add_torrent(AddTorrent {
            have: Some(Bitfield::new(pieces)),
            peers: fetched.peers,
            ..AddTorrent::new(torrent, leech_dir.path().to_path_buf())
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(60), leech.wait_finished())
        .await
        .unwrap_or_else(|_| panic!("{version:?} download timed out: {:?}", leech.stats()));
    assert_files(leech_dir.path(), &fx);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn magnet_v1() {
    magnet_then_download(Version::V1).await;
}

/// A hybrid torrent fetched by its v1 hash: the info dictionary has no piece layers (they
/// aren't part of it), so pieces verify by their v1 hashes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn magnet_hybrid() {
    magnet_then_download(Version::Hybrid).await;
}

/// A fetch that runs into a peer serving the wrong dictionary keeps going and gets the
/// right one from another peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn magnet_ignores_peers_without_the_torrent() {
    use apitorrent_metainfo::Magnet;

    let fx = fixture(Version::V1);
    let seed_dir = tempfile::tempdir().unwrap();
    let (seeder, _seed) = seed_fixture(&fx, seed_dir.path()).await;
    let v1 = fx.torrent.info.info_hashes().get_v1().unwrap().to_hex();
    // A port nobody listens on, then the real seeder.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_addr = dead.local_addr().unwrap();
    drop(dead);
    let uri = format!(
        "magnet:?xt=urn:btih:{v1}&x.pe={dead_addr}&x.pe={}",
        seeder.listen_addr()
    );
    let leecher = Session::start(local_config()).await.unwrap();
    let mut fetch = leecher.fetch_metadata(&Magnet::parse(&uri).unwrap());
    let fetched = tokio::time::timeout(Duration::from_secs(30), fetch.wait())
        .await
        .expect("metadata fetch timed out")
        .expect("fetch finished");
    assert_eq!(&fetched.info[..], fx.torrent.info.raw());
}

/// Peer exchange: B only knows A, A knows the seeder. B learns of the seeder from A and
/// connects to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pex_spreads_peers() {
    let fx = fixture(Version::V1);
    let seed_dir = tempfile::tempdir().unwrap();
    let a_dir = tempfile::tempdir().unwrap();
    let b_dir = tempfile::tempdir().unwrap();
    let (seeder, seed) = seed_fixture(&fx, seed_dir.path()).await;
    let fast_pex = || SessionConfig {
        pex_interval: Duration::from_secs(1),
        ..local_config()
    };
    let pieces = u32::try_from(fx.torrent.info.piece_count()).unwrap();
    let a_session = Session::start(fast_pex()).await.unwrap();
    // A downloads slowly, so it is still a leecher (and connected to the seeder) while
    // B arrives.
    let a = a_session
        .add_torrent(AddTorrent {
            have: Some(Bitfield::new(pieces)),
            peers: vec![seeder.listen_addr()],
            limits: apitorrent_engine::limits::TorrentLimits {
                download_rate: 100_000,
                ..Default::default()
            },
            ..AddTorrent::new(fx.torrent.clone(), a_dir.path().to_path_buf())
        })
        .unwrap();
    wait_for("A to connect to the seeder", || a.stats().peers > 0).await;
    let b_session = Session::start(fast_pex()).await.unwrap();
    let b = b_session
        .add_torrent(AddTorrent {
            have: Some(Bitfield::new(pieces)),
            peers: vec![a_session.listen_addr()],
            ..AddTorrent::new(fx.torrent.clone(), b_dir.path().to_path_buf())
        })
        .unwrap();
    // The seeder sees a second leecher only if B learned its address by PEX.
    let mut watch = seed.watch();
    tokio::time::timeout(Duration::from_secs(30), watch.wait_for(|s| s.peers >= 2))
        .await
        .unwrap_or_else(|_| {
            panic!(
                "B never reached the seeder: {:?} {:?}",
                b.stats(),
                seed.stats()
            )
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(60), b.wait_finished())
        .await
        .expect("B's download timed out");
    assert_files(b_dir.path(), &fx);
}

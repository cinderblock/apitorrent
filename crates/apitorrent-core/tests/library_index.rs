//! The content library beyond matching at add time: retroactive dedupe of identical files
//! that torrents downloaded separately, and library folders (files outside torrents).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;

use apitorrent_core::{Core, CoreConfig, CoreError, Key};
use apitorrent_engine::fileid::identity;
use apitorrent_engine::{AddTorrent, Session, SessionConfig};
use apitorrent_metainfo::{FileSource, Torrent, TorrentBuilder, Version};
use apitorrent_types::library::DedupeMode;
use apitorrent_types::spaces::DEFAULT_SPACE;
use apitorrent_types::{AddSource, AddTorrentRequest, TorrentState, UnwantedPolicy};

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

fn add_req(bytes: &[u8], save: &Path) -> AddTorrentRequest {
    AddTorrentRequest {
        source: AddSource::TorrentFile {
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        },
        save_path: Some(save.to_string_lossy().into_owned()),
        paused: false,
        label: None,
        file_priorities: None,
        unwanted_policy: UnwantedPolicy::Hold,
    }
}

async fn wait_for(core: &Core, id: &str, state: TorrentState) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if core.store().get(&k(id)).is_some_and(|s| s.state == state) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {state:?}: {:?}",
            core.store().get(&k(id))
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn data(len: u32, seed: u32) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from((i * 7 + seed) % 251).unwrap())
        .collect()
}

fn torrent(name: &str, version: Version, piece: u64, bytes: &[u8]) -> Vec<u8> {
    TorrentBuilder::new(name, version)
        .piece_length(piece)
        .file(FileSource::from_bytes(&[], bytes.to_vec()))
        .build()
        .unwrap()
}

/// A fresh engine checks a torrent's files from scratch: they must all verify.
async fn verifies(bytes: &[u8], save: &Path) {
    let session = Session::start(SessionConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ..SessionConfig::default()
    })
    .await
    .unwrap();
    let t = Arc::new(Torrent::from_bytes(bytes).unwrap());
    let h = session.add_torrent(AddTorrent::new(t, save)).unwrap();
    tokio::time::timeout(Duration::from_secs(20), h.wait_finished())
        .await
        .expect("the files should verify");
    h.shutdown().await;
}

/// Two torrents downloaded the same file separately (another name, another piece size).
/// Report-only finds the space to win and changes nothing; "deduplicate now" merges them
/// into one file; both torrents still verify and seed, across a restart; and a write to
/// one through the engine never reaches the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn separately_downloaded_copies_are_merged_and_both_still_seed() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let core = Core::start(cfg.clone()).await.unwrap();
    assert_eq!(
        core.settings().library.dedupe,
        DedupeMode::Report,
        "the default"
    );
    let movie = data(300_000, 1);
    let (dir_a, dir_b) = (cfg.download_dir.join("a"), cfg.download_dir.join("b"));
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();
    let (pa, pb) = (dir_a.join("Movie.mkv"), dir_b.join("movie.2020.mkv"));
    std::fs::write(&pa, &movie).unwrap();
    std::fs::write(&pb, &movie).unwrap();
    let ta = torrent("Movie.mkv", Version::V1, 16 * 1024, &movie);
    let tb = torrent("movie.2020.mkv", Version::V2, 32 * 1024, &movie);
    let a = core.add(DEFAULT_SPACE, add_req(&ta, &dir_a)).await.unwrap();
    let b = core.add(DEFAULT_SPACE, add_req(&tb, &dir_b)).await.unwrap();
    wait_for(&core, &a, TorrentState::Seeding).await;
    wait_for(&core, &b, TorrentState::Seeding).await;
    assert!(!identity(&pa).unwrap().same_file(&identity(&pb).unwrap()));

    let report = core.dedupe_report().await;
    assert_eq!(report.groups, 1, "{report:?}");
    assert_eq!(report.reclaimable_bytes, movie.len() as u64);
    assert_eq!(report.reclaimable_files, 1);
    assert_eq!(report.reclaimed_bytes, 0);
    assert!(
        !identity(&pa).unwrap().same_file(&identity(&pb).unwrap()),
        "report only changes nothing"
    );

    let done = core.dedupe_now().await.unwrap();
    assert_eq!(done.reclaimed_bytes, movie.len() as u64, "{done:?}");
    assert_eq!(done.reclaimed_files, 1);
    assert_eq!(done.reclaimable_bytes, 0);
    assert!(identity(&pa).unwrap().same_file(&identity(&pb).unwrap()));
    assert_eq!(std::fs::read(&pa).unwrap(), movie);
    assert_eq!(std::fs::read(&pb).unwrap(), movie);
    // A second pass finds nothing more.
    let again = core.dedupe_now().await.unwrap();
    assert_eq!(again.reclaimable_bytes, 0);
    assert_eq!(again.reclaimed_bytes, movie.len() as u64, "totals carry on");
    assert_eq!(
        core.store().get(&k(&a)).unwrap().state,
        TorrentState::Seeding
    );
    assert_eq!(
        core.store().get(&k(&b)).unwrap().state,
        TorrentState::Seeding
    );
    core.shutdown().await.unwrap();
    drop(core);

    // Both torrents' files verify from scratch, and both seed after a restart.
    verifies(&ta, &dir_a).await;
    verifies(&tb, &dir_b).await;
    let core = Core::start(cfg.clone()).await.unwrap();
    wait_for(&core, &a, TorrentState::Seeding).await;
    wait_for(&core, &b, TorrentState::Seeding).await;
    assert_eq!(
        core.dedupe_status().reclaimed_bytes,
        movie.len() as u64,
        "the totals survive a restart"
    );
    core.shutdown().await.unwrap();
}

/// A file that changes after it was hashed (same size, other bytes) is not replaced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_changed_after_hashing_is_not_replaced() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let core = Core::start(cfg.clone()).await.unwrap();
    let movie = data(200_000, 2);
    let (dir_a, dir_b) = (cfg.download_dir.join("a"), cfg.download_dir.join("b"));
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();
    let (pa, pb) = (dir_a.join("x.bin"), dir_b.join("y.bin"));
    std::fs::write(&pa, &movie).unwrap();
    std::fs::write(&pb, &movie).unwrap();
    let a = core
        .add(
            DEFAULT_SPACE,
            add_req(&torrent("x.bin", Version::V1, 16 * 1024, &movie), &dir_a),
        )
        .await
        .unwrap();
    let b = core
        .add(
            DEFAULT_SPACE,
            add_req(&torrent("y.bin", Version::V1, 16 * 1024, &movie), &dir_b),
        )
        .await
        .unwrap();
    wait_for(&core, &a, TorrentState::Seeding).await;
    wait_for(&core, &b, TorrentState::Seeding).await;
    assert_eq!(core.dedupe_report().await.reclaimable_files, 1);

    // Someone edits one copy in place after the hashing.
    tokio::time::sleep(Duration::from_millis(30)).await;
    let mut changed = movie.clone();
    changed[150_000] ^= 0x55;
    std::fs::write(&pb, &changed).unwrap();
    let before = (identity(&pa).unwrap(), identity(&pb).unwrap());

    let done = core.dedupe_now().await.unwrap();
    assert_eq!(done.reclaimed_files, 0, "{done:?}");
    assert_eq!(done.reclaimable_files, 0, "they differ now");
    assert_eq!(std::fs::read(&pb).unwrap(), changed, "left as it was");
    assert_eq!(std::fs::read(&pa).unwrap(), movie);
    assert!(identity(&pa).unwrap().same_file(&before.0));
    assert!(identity(&pb).unwrap().same_file(&before.1));
    core.shutdown().await.unwrap();
}

/// Dedupe set to off refuses "deduplicate now".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dedupe_off_refuses_to_run() {
    let root = tempfile::tempdir().unwrap();
    let core = Core::start(config(root.path())).await.unwrap();
    let mut s = core.settings();
    s.library.dedupe = DedupeMode::Off;
    core.set_settings(s).await.unwrap();
    assert!(matches!(
        core.dedupe_now().await,
        Err(CoreError::InvalidRequest(_))
    ));
    assert_eq!(core.dedupe_status().mode, DedupeMode::Off);
    core.shutdown().await.unwrap();
}

/// Walk a library folder now, waiting out a walk the background task started.
async fn scan(core: &Core, id: i64) -> usize {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match core.scan_folder_now(id).await {
            Ok(n) => return n,
            Err(CoreError::InvalidRequest(_)) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("{e}"),
        }
    }
}

fn stamp(p: &Path) -> (u64, std::time::SystemTime) {
    let m = std::fs::metadata(p).unwrap();
    (m.len(), m.modified().unwrap())
}

/// A file in a library folder is reused by a new torrent with nothing downloaded: a v1
/// torrent by checking pieces, a v2 torrent by its fingerprint. The folder file is never
/// modified, and removing the torrents with their files leaves it in place.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn library_folder_files_are_reused_and_never_modified() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let core = Core::start(cfg.clone()).await.unwrap();
    let media = root.path().join("media");
    std::fs::create_dir_all(media.join("Films")).unwrap();
    let film = data(250_000, 3);
    let source = media.join("Films").join("Film (2019).mkv");
    std::fs::write(&source, &film).unwrap();
    let before = (stamp(&source), identity(&source).unwrap());

    assert!(matches!(
        core.add_library_folder("relative/path").await,
        Err(CoreError::InvalidRequest(_))
    ));
    let folder = core
        .add_library_folder(&media.to_string_lossy())
        .await
        .unwrap();
    assert!(matches!(
        core.add_library_folder(&media.to_string_lossy()).await,
        Err(CoreError::InvalidRequest(_))
    ));
    assert_eq!(scan(&core, folder.id).await, 1);
    let st = core.library_status().await.unwrap();
    assert_eq!(st.folders.len(), 1);
    assert_eq!(
        (st.folders[0].files, st.folders[0].bytes),
        (1, film.len() as u64)
    );

    // v1, another name and alignment: found by length, checked by pieces.
    let dl = cfg.download_dir.clone();
    let t1 = torrent("film.mkv", Version::V1, 16 * 1024, &film);
    let id1 = core.add(DEFAULT_SPACE, add_req(&t1, &dl)).await.unwrap();
    wait_for(&core, &id1, TorrentState::Seeding).await;
    assert_eq!(core.store().get(&k(&id1)).unwrap().downloaded, 0);
    assert_eq!(std::fs::read(dl.join("film.mkv")).unwrap(), film);

    // Fingerprinted (here, or by the background task the walk woke), a v2 torrent finds
    // it by root.
    core.hash_folders_now().await;
    assert_eq!(core.hash_folders_now().await, 0, "nothing left");
    let st = core.library_status().await.unwrap();
    assert_eq!(st.folders[0].hashed_files, 1);
    let t2 = torrent("Film.2019.mkv", Version::V2, 64 * 1024, &film);
    let id2 = core.add(DEFAULT_SPACE, add_req(&t2, &dl)).await.unwrap();
    wait_for(&core, &id2, TorrentState::Seeding).await;
    assert_eq!(core.store().get(&k(&id2)).unwrap().downloaded, 0);

    // Dedupe never touches folder files.
    let d = core.dedupe_now().await.unwrap();
    assert_eq!(d.reclaimed_files, 0, "{d:?}");

    core.remove(&k(&id1), true).await.unwrap();
    core.remove(&k(&id2), true).await.unwrap();
    assert!(!dl.join("film.mkv").exists());
    assert_eq!(
        std::fs::read(&source).unwrap(),
        film,
        "the folder file is intact"
    );
    assert_eq!(stamp(&source), before.0, "and unmodified");
    assert!(
        identity(&source).unwrap().same_file(&before.1),
        "and not replaced"
    );

    core.remove_library_folder(folder.id).await.unwrap();
    assert!(core.library_status().await.unwrap().folders.is_empty());
    assert!(source.exists(), "forgetting a folder leaves its files");
    core.shutdown().await.unwrap();
}

/// A folder file that changed since it was fingerprinted isn't trusted: a v2 torrent of the
/// old content doesn't take it, and stays waiting for peers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_folder_fingerprint_is_ignored() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let core = Core::start(cfg.clone()).await.unwrap();
    let media = root.path().join("media");
    std::fs::create_dir_all(&media).unwrap();
    let film = data(200_000, 4);
    let source = media.join("film.mkv");
    std::fs::write(&source, &film).unwrap();
    let folder = core
        .add_library_folder(&media.to_string_lossy())
        .await
        .unwrap();
    scan(&core, folder.id).await;
    core.hash_folders_now().await;
    assert_eq!(
        core.library_status().await.unwrap().folders[0].hashed_files,
        1
    );

    // Changed in place: same size, other bytes throughout, a later time.
    tokio::time::sleep(Duration::from_millis(30)).await;
    let other = data(200_000, 99);
    std::fs::write(&source, &other).unwrap();

    let dl = cfg.download_dir.clone();
    let t = torrent("film.mkv", Version::V2, 16 * 1024, &film);
    let id = core.add(DEFAULT_SPACE, add_req(&t, &dl)).await.unwrap();
    wait_for(&core, &id, TorrentState::Downloading).await;
    let s = core.store().get(&k(&id)).unwrap();
    assert!(s.progress < 0.01, "{s:?}");
    assert_eq!(
        std::fs::read(&source).unwrap(),
        other,
        "the folder file is untouched"
    );
    core.shutdown().await.unwrap();
}

/// A torrent removed with "keep files" leaves its complete files under a library folder as
/// candidates at once, without waiting for a walk, and a later torrent reuses them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removed_torrents_kept_files_stay_candidates() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let core = Core::start(cfg.clone()).await.unwrap();
    let kept: PathBuf = root.path().join("kept");
    std::fs::create_dir_all(&kept).unwrap();
    let folder = core
        .add_library_folder(&kept.to_string_lossy())
        .await
        .unwrap();
    assert_eq!(scan(&core, folder.id).await, 0, "empty for now");

    let film = data(180_000, 5);
    std::fs::write(kept.join("ep.mkv"), &film).unwrap();
    let t = torrent("ep.mkv", Version::V2, 16 * 1024, &film);
    let id = core.add(DEFAULT_SPACE, add_req(&t, &kept)).await.unwrap();
    wait_for(&core, &id, TorrentState::Seeding).await;
    core.remove(&k(&id), false).await.unwrap();
    assert!(kept.join("ep.mkv").exists());
    let st = core.library_status().await.unwrap();
    assert_eq!(st.folders[0].files, 1, "recorded without a walk: {st:?}");

    let again = torrent("Episode 1.mkv", Version::V2, 32 * 1024, &film);
    let dl = cfg.download_dir.clone();
    let id2 = core.add(DEFAULT_SPACE, add_req(&again, &dl)).await.unwrap();
    wait_for(&core, &id2, TorrentState::Seeding).await;
    assert_eq!(core.store().get(&k(&id2)).unwrap().downloaded, 0);
    core.shutdown().await.unwrap();
}

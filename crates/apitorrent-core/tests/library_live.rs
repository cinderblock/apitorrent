//! The content library while torrents run: files linked in when another torrent finishes
//! them, pieces shared between torrents downloading the same file, bookend pieces built
//! from disk plus peers, and where each file's bytes came from.

use std::sync::Arc;
use std::time::Duration;

use apitorrent_core::{Core, CoreConfig, Key};
use apitorrent_engine::{AddTorrent, Session, SessionConfig};
use apitorrent_metainfo::{FileSource, Torrent, TorrentBuilder, Version};
use apitorrent_types::spaces::DEFAULT_SPACE;
use apitorrent_types::{AddSource, AddTorrentRequest, ReuseMethod, TorrentState, UnwantedPolicy};
use base64::Engine as _;

const K: usize = 1024;

fn config(root: &std::path::Path) -> CoreConfig {
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

fn add_bytes(bytes: &[u8]) -> AddTorrentRequest {
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

fn bytes(len: usize, seed: usize) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from((i * 7 + seed) % 251).unwrap())
        .collect()
}

async fn wait_until(what: &str, secs: u64, mut done: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while !done() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_state(core: &Core, id: &str, state: TorrentState, secs: u64) {
    wait_until(&format!("{id} {state:?}"), secs, || {
        core.store().get(&k(id)).is_some_and(|s| s.state == state)
    })
    .await;
}

/// A tracker that always returns one peer.
async fn tracker_with_peer(peer: std::net::SocketAddr) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let std::net::SocketAddr::V4(v4) = peer else {
        panic!("IPv4 only")
    };
    let mut compact = v4.ip().octets().to_vec();
    compact.extend_from_slice(&v4.port().to_be_bytes());
    let mut body = b"d8:intervali60e5:peers6:".to_vec();
    body.extend_from_slice(&compact);
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

/// A seeding session, uploading at most `rate` bytes per second (0: no limit).
async fn seeder(rate: u64) -> Session {
    Session::start(SessionConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        upload_rate: rate,
        ..SessionConfig::default()
    })
    .await
    .unwrap()
}

/// Seed `bytes` (a `.torrent`) from `dir`, where its files are written first.
async fn seed(session: &Session, bytes: &[u8], files: &[(&[&str], &[u8])], dir: &std::path::Path) {
    let torrent = Arc::new(Torrent::from_bytes(bytes).unwrap());
    for (path, data) in files {
        let p = path.iter().fold(dir.to_path_buf(), |p, c| p.join(c));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }
    let h = session
        .add_torrent(AddTorrent::new(torrent, dir.to_path_buf()))
        .unwrap();
    h.wait_finished().await;
}

/// Two torrents with the same file at different alignments, added together: one has a
/// (slow) peer, the other none. The second gets every piece from the first as it arrives,
/// so the file is downloaded once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_torrents_downloading_the_same_file_download_it_once() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let core = Core::start(cfg.clone()).await.unwrap();
    let show = bytes(2048 * K, 3);
    let intro = bytes(10_000, 9);

    // The pack: an intro first, so the show starts 10,000 bytes in, at 16 KiB pieces.
    let session = seeder(192 * 1024).await;
    let tracker = tracker_with_peer(session.listen_addr()).await;
    let pack = TorrentBuilder::new("Pack", Version::V1)
        .piece_length(16 * 1024)
        .file(FileSource::from_bytes(&["intro.nfo"], intro.clone()))
        .file(FileSource::from_bytes(&["show.mkv"], show.clone()))
        .tracker_tier(vec![tracker])
        .build()
        .unwrap();
    let seed_dir = tempfile::tempdir().unwrap();
    seed(
        &session,
        &pack,
        &[
            (&["Pack", "intro.nfo"], &intro),
            (&["Pack", "show.mkv"], &show),
        ],
        seed_dir.path(),
    )
    .await;
    // The single: the show alone, at 64 KiB pieces, with no peers at all.
    let single = TorrentBuilder::new("show.mkv", Version::V1)
        .piece_length(64 * 1024)
        .file(FileSource::from_bytes(&[], show.clone()))
        .build()
        .unwrap();

    let a = core.add(DEFAULT_SPACE, add_bytes(&pack)).await.unwrap();
    let b = core.add(DEFAULT_SPACE, add_bytes(&single)).await.unwrap();
    // Sharing starts while the pack is still downloading.
    wait_until("the single gets pieces", 30, || {
        core.store().get(&k(&b)).is_some_and(|s| s.pieces_have > 0)
    })
    .await;
    assert_ne!(
        core.store().get(&k(&a)).unwrap().state,
        TorrentState::Seeding,
        "the pack should still be downloading"
    );
    wait_state(&core, &a, TorrentState::Seeding, 60).await;
    wait_state(&core, &b, TorrentState::Seeding, 30).await;

    assert_eq!(
        std::fs::read(cfg.download_dir.join("show.mkv")).unwrap(),
        show
    );
    assert_eq!(core.store().get(&k(&b)).unwrap().downloaded, 0);
    let pack_size = (show.len() + intro.len()) as u64;
    let downloaded = core.store().get(&k(&a)).unwrap().downloaded;
    assert!(
        downloaded >= pack_size && downloaded < pack_size + 64 * K as u64,
        "the pack downloaded {downloaded} of {pack_size}"
    );

    let detail = core.detail(&k(&b)).await.unwrap();
    let reuse = detail.files[0].reused.clone().expect("recorded");
    assert_eq!(reuse.source_id, a);
    assert_eq!(reuse.source_name, "Pack");
    assert_eq!(reuse.method, ReuseMethod::PieceSharing);
    assert_eq!(reuse.bytes, show.len() as u64);
    assert_eq!(detail.library_bytes, show.len() as u64);
    assert!(core.session_stats().library_bytes >= show.len() as u64);
    core.shutdown().await.unwrap();
}

/// A torrent waits for a file with no peers; another torrent of the same file shows up
/// complete later. The waiting torrent gets it hard-linked, with nothing downloaded, and
/// the record of where it came from survives a restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_finished_later_is_linked_into_a_waiting_torrent() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let core = Core::start(cfg.clone()).await.unwrap();
    let movie = bytes(300 * K + 123, 5);
    let waiting = TorrentBuilder::new("Movie (2026)", Version::V1)
        .piece_length(32 * 1024)
        .file(FileSource::from_bytes(&["movie.mkv"], movie.clone()))
        .file(FileSource::from_bytes(&["movie.nfo"], b"tiny".to_vec()))
        .build()
        .unwrap();
    let b = core.add(DEFAULT_SPACE, add_bytes(&waiting)).await.unwrap();
    wait_state(&core, &b, TorrentState::Downloading, 20).await;

    // The movie turns up in another torrent, complete.
    std::fs::write(cfg.download_dir.join("movie.mkv"), &movie).unwrap();
    let single = TorrentBuilder::new("movie.mkv", Version::V1)
        .piece_length(16 * 1024)
        .file(FileSource::from_bytes(&[], movie.clone()))
        .build()
        .unwrap();
    let a = core.add(DEFAULT_SPACE, add_bytes(&single)).await.unwrap();
    wait_state(&core, &a, TorrentState::Seeding, 20).await;

    let target = cfg.download_dir.join("Movie (2026)").join("movie.mkv");
    wait_until("the movie is linked in", 20, || {
        std::fs::read(&target).is_ok_and(|d| d == movie)
    })
    .await;
    // Every piece but the last, which the missing .nfo shares.
    wait_until("the linked pieces count", 10, || {
        core.store()
            .get(&k(&b))
            .is_some_and(|s| s.pieces_have + 1 == s.pieces_total)
    })
    .await;
    assert_eq!(core.store().get(&k(&b)).unwrap().downloaded, 0);
    let check = |detail: &apitorrent_types::TorrentDetail| {
        let reuse = detail.files[0].reused.clone().expect("recorded");
        assert_eq!(reuse.source_id, a);
        assert_eq!(reuse.source_name, "movie.mkv");
        assert_eq!(reuse.method, ReuseMethod::HardLink);
        // The movie's bytes in every piece but the shared last one.
        assert_eq!(reuse.bytes, 9 * 32 * K as u64);
        assert!(detail.files[1].reused.is_none());
    };
    check(&core.detail(&k(&b)).await.unwrap());

    // Writing the torrent's own data never changes the other torrent's file.
    core.shutdown().await.unwrap();
    drop(core);
    let core = Core::start(cfg.clone()).await.unwrap();
    wait_state(&core, &b, TorrentState::Downloading, 20).await;
    check(&core.detail(&k(&b)).await.unwrap());
    assert!(core.session_stats().library_bytes >= 9 * 32 * K as u64);
    assert_eq!(
        std::fs::read(cfg.download_dir.join("movie.mkv")).unwrap(),
        movie
    );
    core.shutdown().await.unwrap();
}

/// A pack's first episode is linked from a single the library has; the piece it shares
/// with the second episode takes the first episode's blocks from disk and downloads only
/// the rest, instead of the whole piece.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bookend_piece_is_built_from_disk_and_peers() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let core = Core::start(cfg.clone()).await.unwrap();
    // 150,000 bytes: the pack's piece 4 (131,072..163,840) straddles both episodes. Its
    // first block (to 147,456) is all episode 1.
    let (e1, e2) = (bytes(150_000, 1), bytes(100_000, 2));
    std::fs::write(cfg.download_dir.join("e1.mkv"), &e1).unwrap();
    let single = TorrentBuilder::new("e1.mkv", Version::V1)
        .piece_length(16 * 1024)
        .file(FileSource::from_bytes(&[], e1.clone()))
        .build()
        .unwrap();
    let s1 = core.add(DEFAULT_SPACE, add_bytes(&single)).await.unwrap();
    wait_state(&core, &s1, TorrentState::Seeding, 20).await;

    let session = seeder(0).await;
    let tracker = tracker_with_peer(session.listen_addr()).await;
    let pack = TorrentBuilder::new("Season", Version::V1)
        .piece_length(32 * 1024)
        .file(FileSource::from_bytes(&["e1.mkv"], e1.clone()))
        .file(FileSource::from_bytes(&["e2.mkv"], e2.clone()))
        .tracker_tier(vec![tracker])
        .build()
        .unwrap();
    let seed_dir = tempfile::tempdir().unwrap();
    seed(
        &session,
        &pack,
        &[(&["Season", "e1.mkv"], &e1), (&["Season", "e2.mkv"], &e2)],
        seed_dir.path(),
    )
    .await;
    let p = core.add(DEFAULT_SPACE, add_bytes(&pack)).await.unwrap();
    wait_state(&core, &p, TorrentState::Seeding, 30).await;

    let season = cfg.download_dir.join("Season");
    assert_eq!(std::fs::read(season.join("e1.mkv")).unwrap(), e1);
    assert_eq!(std::fs::read(season.join("e2.mkv")).unwrap(), e2);
    let downloaded = core.store().get(&k(&p)).unwrap().downloaded;
    // Episode 2, plus the part of the shared piece's second block that is episode 1's.
    // Downloading the shared piece whole would have cost another 16 KiB.
    let expected = e2.len() as u64 + (150_000 - 147_456);
    assert_eq!(downloaded, expected);
    let detail = core.detail(&k(&p)).await.unwrap();
    let reuse = detail.files[0].reused.clone().expect("recorded");
    assert_eq!(reuse.source_id, s1);
    assert_eq!(reuse.method, ReuseMethod::HardLink);
    assert_eq!(
        reuse.bytes, 147_456,
        "four whole pieces and the bookend's block"
    );
    assert!(detail.files[1].reused.is_none());
    assert_eq!(
        std::fs::read(cfg.download_dir.join("e1.mkv")).unwrap(),
        e1,
        "the single's file is untouched"
    );
    core.shutdown().await.unwrap();
}

/// Removing the source torrent (with its files) while it shares pieces leaves the other
/// torrent running with only verified pieces; removing that one mid-way is fine too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removing_a_torrent_mid_share_is_safe() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let core = Core::start(cfg.clone()).await.unwrap();
    let data = bytes(2048 * K, 4);
    let session = seeder(256 * 1024).await;
    let tracker = tracker_with_peer(session.listen_addr()).await;
    let a_bytes = TorrentBuilder::new("a.bin", Version::V1)
        .piece_length(16 * 1024)
        .file(FileSource::from_bytes(&[], data.clone()))
        .tracker_tier(vec![tracker])
        .build()
        .unwrap();
    let seed_dir = tempfile::tempdir().unwrap();
    seed(&session, &a_bytes, &[(&["a.bin"], &data)], seed_dir.path()).await;
    let b_bytes = TorrentBuilder::new("B", Version::V1)
        .piece_length(32 * 1024)
        .file(FileSource::from_bytes(&["b.bin"], data.clone()))
        .build()
        .unwrap();
    let a = core.add(DEFAULT_SPACE, add_bytes(&a_bytes)).await.unwrap();
    let b = core.add(DEFAULT_SPACE, add_bytes(&b_bytes)).await.unwrap();
    wait_until("the second torrent gets pieces", 30, || {
        core.store().get(&k(&b)).is_some_and(|s| s.pieces_have > 2)
    })
    .await;
    core.remove(&k(&a), true).await.unwrap();
    assert!(!cfg.download_dir.join("a.bin").exists());
    let had = core.store().get(&k(&b)).unwrap().pieces_have;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let s = core.store().get(&k(&b)).unwrap();
    assert_eq!(s.state, TorrentState::Downloading, "{s:?}");
    assert!(s.pieces_have >= had);
    assert!(s.pieces_have < s.pieces_total, "nothing left to share from");
    let detail = core.detail(&k(&b)).await.unwrap();
    assert!(
        detail.files[0]
            .reused
            .as_ref()
            .is_some_and(|r| r.source_id == a)
    );
    core.remove(&k(&b), true).await.unwrap();
    assert_eq!(
        core.session_stats().library_bytes,
        0,
        "records go with the torrent"
    );
    core.shutdown().await.unwrap();
}

/// Removing the torrent that is being filled, while pieces are on their way to it, leaves
/// the source downloading normally.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removing_the_needing_torrent_mid_share_is_safe() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let core = Core::start(cfg.clone()).await.unwrap();
    let data = bytes(1024 * K, 6);
    let session = seeder(256 * 1024).await;
    let tracker = tracker_with_peer(session.listen_addr()).await;
    let a_bytes = TorrentBuilder::new("a.bin", Version::V1)
        .piece_length(16 * 1024)
        .file(FileSource::from_bytes(&[], data.clone()))
        .tracker_tier(vec![tracker])
        .build()
        .unwrap();
    let seed_dir = tempfile::tempdir().unwrap();
    seed(&session, &a_bytes, &[(&["a.bin"], &data)], seed_dir.path()).await;
    let b_bytes = TorrentBuilder::new("b.bin", Version::V1)
        .piece_length(32 * 1024)
        .file(FileSource::from_bytes(&[], data.clone()))
        .build()
        .unwrap();
    let a = core.add(DEFAULT_SPACE, add_bytes(&a_bytes)).await.unwrap();
    let b = core.add(DEFAULT_SPACE, add_bytes(&b_bytes)).await.unwrap();
    wait_until("the second torrent gets pieces", 30, || {
        core.store().get(&k(&b)).is_some_and(|s| s.pieces_have > 0)
    })
    .await;
    core.remove(&k(&b), true).await.unwrap();
    wait_state(&core, &a, TorrentState::Seeding, 60).await;
    assert_eq!(std::fs::read(cfg.download_dir.join("a.bin")).unwrap(), data);
    assert!(!cfg.download_dir.join("b.bin").exists());
    core.shutdown().await.unwrap();
}

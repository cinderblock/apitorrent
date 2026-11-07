//! A torrent's life through the core: add, check, persist, restart, restore, remove.

use std::time::Duration;

use apitorrent_core::{Core, CoreConfig, CoreError};
use apitorrent_engine::SessionConfig;
use apitorrent_metainfo::{FileSource, TorrentBuilder, Version};
use apitorrent_types::{
    AddSource, AddTorrentRequest, FilePlacement, FilePriority, QueueMove, TorrentState,
    UnwantedPolicy,
};
use base64::Engine as _;

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

async fn wait_for(core: &Core, id: &str, state: TorrentState) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if core.store().get(id).is_some_and(|s| s.state == state) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {state:?}: {:?}",
            core.store().get(id)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_restart_remove() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let data: Vec<u8> = (0..300_000u32)
        .map(|i| u8::try_from(i % 241).unwrap())
        .collect();
    let torrent = TorrentBuilder::new("payload", Version::Hybrid)
        .piece_length(32 * 1024)
        .file(FileSource::from_bytes(&["a.bin"], data.clone()))
        .file(FileSource::from_bytes(&["b.bin"], data[..1234].to_vec()))
        .build()
        .unwrap();
    // The files are already in place, so the check should find the torrent complete.
    let dir = cfg.download_dir.join("payload");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.bin"), &data).unwrap();
    std::fs::write(dir.join("b.bin"), &data[..1234]).unwrap();

    let core = Core::start(cfg.clone()).await.unwrap();
    let req = AddTorrentRequest {
        source: AddSource::TorrentFile {
            data: base64::engine::general_purpose::STANDARD.encode(&torrent),
        },
        save_path: None,
        paused: false,
        label: Some("test".into()),
        file_priorities: None,
        unwanted_policy: UnwantedPolicy::Hold,
    };
    let id = core.add(req.clone()).await.unwrap();
    assert!(matches!(core.add(req).await, Err(CoreError::Duplicate)));
    wait_for(&core, &id, TorrentState::Seeding).await;

    let detail = core.detail(&id).await.unwrap();
    assert_eq!(detail.files.len(), 2);
    assert!(
        detail
            .files
            .iter()
            .all(|f| (f.progress - 1.0).abs() < f64::EPSILON)
    );
    assert!(detail.magnet.starts_with("magnet:?xt=urn:btih:"));
    assert!(
        detail
            .files
            .iter()
            .all(|f| f.placement == FilePlacement::Save)
    );

    // Deselecting b.bin moves it to the held area in the data directory.
    let b = detail
        .files
        .iter()
        .find(|f| f.path.ends_with("b.bin"))
        .unwrap()
        .index;
    core.set_file_priorities(
        &id,
        &[FilePriority {
            index: b,
            priority: 0,
        }],
    )
    .await
    .unwrap();
    let held = cfg
        .data_dir
        .join("held")
        .join(&id)
        .join("payload")
        .join("b.bin");
    for _ in 0..200 {
        if held.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(held.exists(), "b.bin is held");
    assert!(
        !dir.join("b.bin").exists(),
        "and gone from the save directory"
    );

    core.pause(&id).await.unwrap();
    wait_for(&core, &id, TorrentState::Paused).await;
    core.shutdown().await.unwrap();
    drop(core);

    // Restart: the torrent comes back paused, from resume data rather than a recheck.
    let core = Core::start(cfg.clone()).await.unwrap();
    let s = core.store().get(&id).expect("restored");
    assert_eq!(s.state, TorrentState::Paused);
    assert_eq!(s.label.as_deref(), Some("test"));
    assert_eq!(s.pieces_have, s.pieces_total);
    let detail = core.detail(&id).await.unwrap();
    let held_file = detail.files.iter().find(|f| f.index == b).unwrap();
    assert_eq!(
        held_file.placement,
        FilePlacement::Held,
        "placements survive a restart"
    );
    core.resume(&id).await.unwrap();
    wait_for(&core, &id, TorrentState::Seeding).await;

    core.remove(&id, true).await.unwrap();
    assert!(core.store().get(&id).is_none());
    assert!(!dir.exists(), "files and the emptied directory are deleted");
    assert!(!held.exists(), "held copies are deleted with the torrent");
    assert!(matches!(core.pause(&id).await, Err(CoreError::NotFound)));
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_input_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let core = Core::start(config(root.path())).await.unwrap();
    let bad = |source| AddTorrentRequest {
        source,
        save_path: None,
        paused: false,
        label: None,
        file_priorities: None,
        unwanted_policy: UnwantedPolicy::Hold,
    };
    let e = core
        .add(bad(AddSource::TorrentFile { data: "!!!".into() }))
        .await
        .unwrap_err();
    assert_eq!(e.code(), "invalid_request");
    let e = core
        .add(bad(AddSource::TorrentFile {
            data: base64::engine::general_purpose::STANDARD.encode(b"d4:infoi1ee"),
        }))
        .await
        .unwrap_err();
    assert_eq!(e.code(), "invalid_torrent");
    core.shutdown().await.unwrap();
}

fn small_torrent(name: &str, fill: u8) -> Vec<u8> {
    TorrentBuilder::new(name, Version::V1)
        .piece_length(16 * 1024)
        .file(FileSource::from_bytes(&[], vec![fill; 40_000]))
        .build()
        .unwrap()
}

fn add_req(bytes: &[u8], label: Option<&str>) -> AddTorrentRequest {
    AddTorrentRequest {
        source: AddSource::TorrentFile {
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        },
        save_path: None,
        paused: false,
        label: label.map(str::to_owned),
        file_priorities: None,
        unwanted_policy: UnwantedPolicy::Hold,
    }
}

/// The queue lets only the first downloads run, follows queue moves, settings persist, and
/// labels pick groups.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_settings_and_groups() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let core = Core::start(cfg.clone()).await.unwrap();

    let mut settings = core.settings();
    settings.queue.max_active_downloads = 1;
    settings.download_rate = 5 * 1024 * 1024;
    settings.groups.push(apitorrent_types::GroupSettings {
        name: "linux".into(),
        labels: vec!["iso".into()],
        download_rate: 1024 * 1024,
        upload_rate: 0,
        max_connections: 10,
    });
    core.set_settings(settings.clone()).await.unwrap();

    // Neither has peers, so both stay downloading; only the first may run.
    let a = core
        .add(add_req(&small_torrent("a", 1), Some("iso")))
        .await
        .unwrap();
    let b = core
        .add(add_req(&small_torrent("b", 2), None))
        .await
        .unwrap();
    wait_for(&core, &b, TorrentState::Queued).await;
    wait_for(&core, &a, TorrentState::Downloading).await;
    assert_eq!(
        core.store().get(&a).unwrap().group.as_deref(),
        Some("linux")
    );
    assert_eq!(core.store().get(&b).unwrap().group, None);

    // Moving b to the top swaps them.
    core.queue_move(&b, QueueMove::Top).await.unwrap();
    wait_for(&core, &a, TorrentState::Queued).await;
    wait_for(&core, &b, TorrentState::Downloading).await;
    assert_eq!(core.store().get(&b).unwrap().queue_position, 0);

    // Relabelling moves a torrent between groups.
    core.set_label(&b, Some("iso".into())).await.unwrap();
    assert_eq!(
        core.store().get(&b).unwrap().group.as_deref(),
        Some("linux")
    );

    // A user pause takes a torrent out of the queue, freeing its slot.
    core.pause(&b).await.unwrap();
    wait_for(&core, &a, TorrentState::Downloading).await;
    wait_for(&core, &b, TorrentState::Paused).await;

    // Per-torrent limits are stored.
    let limits = apitorrent_types::TorrentLimits {
        download_rate: 1000,
        upload_rate: 2000,
        max_connections: Some(5),
    };
    core.set_torrent_limits(&a, limits).await.unwrap();
    core.shutdown().await.unwrap();
    drop(core);

    // Everything survives a restart.
    let core = Core::start(cfg).await.unwrap();
    assert_eq!(core.settings(), settings);
    assert_eq!(core.detail(&a).await.unwrap().limits, limits);
    assert_eq!(core.store().get(&b).unwrap().queue_position, 0);
    assert_eq!(core.store().get(&b).unwrap().state, TorrentState::Paused);
    wait_for(&core, &a, TorrentState::Downloading).await;

    // Invalid settings are refused.
    let mut bad = core.settings();
    bad.groups.push(bad.groups[0].clone());
    assert!(core.set_settings(bad).await.is_err());
    core.shutdown().await.unwrap();
}

/// A magnet link: the metadata comes from a peer, the torrent downloads, and after a
/// restart it is an ordinary torrent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn magnet_link_end_to_end() {
    use apitorrent_engine::{AddTorrent, Session};

    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let data: Vec<u8> = (0..200_000u32)
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect();
    let bytes = TorrentBuilder::new("linux.iso", Version::V1)
        .piece_length(16 * 1024)
        .file(FileSource::from_bytes(&[], data.clone()))
        .build()
        .unwrap();
    let torrent = std::sync::Arc::new(apitorrent_metainfo::Torrent::from_bytes(&bytes).unwrap());

    // An engine session seeds it.
    let seed_dir = tempfile::tempdir().unwrap();
    std::fs::write(seed_dir.path().join("linux.iso"), &data).unwrap();
    let seeder = Session::start(SessionConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ..SessionConfig::default()
    })
    .await
    .unwrap();
    let seed = seeder
        .add_torrent(AddTorrent::new(
            torrent.clone(),
            seed_dir.path().to_path_buf(),
        ))
        .unwrap();
    seed.wait_finished().await;

    let core = Core::start(cfg.clone()).await.unwrap();
    let v1 = torrent.info.info_hashes().get_v1().unwrap().to_hex();
    let uri = format!(
        "magnet:?xt=urn:btih:{v1}&dn=Linux&x.pe={}",
        seeder.listen_addr()
    );
    let id = core
        .add(AddTorrentRequest {
            source: AddSource::Magnet { uri },
            save_path: None,
            paused: false,
            label: Some("iso".into()),
            file_priorities: None,
            unwanted_policy: UnwantedPolicy::Hold,
        })
        .await
        .unwrap();
    assert_eq!(id, v1);
    let s = core.store().get(&id).unwrap();
    assert!(
        matches!(
            s.state,
            TorrentState::Metadata
                | TorrentState::Checking
                | TorrentState::Downloading
                | TorrentState::Seeding
        ),
        "{s:?}"
    );
    wait_for(&core, &id, TorrentState::Seeding).await;
    let s = core.store().get(&id).unwrap();
    assert_eq!(s.name, "linux.iso", "the real name replaces the magnet's");
    assert_eq!(s.label.as_deref(), Some("iso"));
    assert_eq!(
        std::fs::read(cfg.download_dir.join("linux.iso")).unwrap(),
        data
    );
    core.shutdown().await.unwrap();
    drop(core);

    let core = Core::start(cfg).await.unwrap();
    let s = core.store().get(&id).unwrap();
    assert_eq!(s.name, "linux.iso");
    wait_for(&core, &id, TorrentState::Seeding).await;
    core.shutdown().await.unwrap();
}

/// A magnet link with no reachable peers waits; pausing and removing it works.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_magnet_can_be_paused_and_removed() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let core = Core::start(cfg.clone()).await.unwrap();
    let uri = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=Nowhere";
    let id = core
        .add(AddTorrentRequest {
            source: AddSource::Magnet { uri: uri.into() },
            save_path: None,
            paused: false,
            label: None,
            file_priorities: None,
            unwanted_policy: UnwantedPolicy::Hold,
        })
        .await
        .unwrap();
    assert_eq!(core.store().get(&id).unwrap().state, TorrentState::Metadata);
    assert_eq!(core.store().get(&id).unwrap().name, "Nowhere");
    let detail = core.detail(&id).await.unwrap();
    assert_eq!(detail.magnet, uri);
    assert!(detail.files.is_empty());
    core.pause(&id).await.unwrap();
    assert_eq!(core.store().get(&id).unwrap().state, TorrentState::Paused);
    core.shutdown().await.unwrap();
    drop(core);

    // It comes back paused, still waiting for metadata.
    let core = Core::start(cfg).await.unwrap();
    assert_eq!(core.store().get(&id).unwrap().state, TorrentState::Paused);
    core.resume(&id).await.unwrap();
    assert_eq!(core.store().get(&id).unwrap().state, TorrentState::Metadata);
    core.remove(&id, true).await.unwrap();
    assert!(core.store().get(&id).is_none());
    core.shutdown().await.unwrap();
}

/// With native naming, names are kept wherever the save directory's filesystem allows.
/// On NTFS (Windows) the probe finds `:` refused, so the torrent falls back to portable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_naming_follows_the_filesystem() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let core = Core::start(cfg.clone()).await.unwrap();
    let mut settings = core.settings();
    settings.naming = apitorrent_types::NamingPolicy::Native;
    core.set_settings(settings).await.unwrap();

    let data = vec![9u8; 50_000];
    let bytes = TorrentBuilder::new("talk: part 1.mp3", Version::V1)
        .piece_length(16 * 1024)
        .file(FileSource::from_bytes(&[], data.clone()))
        .build()
        .unwrap();
    // The file is already there under the name the filesystem will get.
    let expected = if cfg!(windows) {
        "talk_ part 1.mp3"
    } else {
        "talk: part 1.mp3"
    };
    std::fs::write(cfg.download_dir.join(expected), &data).unwrap();
    let id = core
        .add(AddTorrentRequest {
            source: AddSource::TorrentFile {
                data: base64::engine::general_purpose::STANDARD.encode(&bytes),
            },
            save_path: None,
            paused: false,
            label: None,
            file_priorities: None,
            unwanted_policy: UnwantedPolicy::Hold,
        })
        .await
        .unwrap();
    wait_for(&core, &id, TorrentState::Seeding).await;
    let naming = core.detail(&id).await.unwrap().naming;
    if cfg!(windows) {
        assert_eq!(naming, apitorrent_types::NamingPolicy::Portable);
    } else {
        assert_eq!(naming, apitorrent_types::NamingPolicy::Native);
    }
    core.shutdown().await.unwrap();
}

/// A tracker that answers every announce with a failure: the torrent is unregistered.
async fn unregistered_tracker() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let body = b"d14:failure reason20:Unregistered torrente";
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body).await;
            });
        }
    });
    format!("http://{addr}/announce")
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

/// The pack case: single episodes are seeding, a season pack with the same files arrives,
/// and it reuses them instead of downloading. The singles are superseded, and one whose
/// tracker says "unregistered" is retired without touching its file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pack_reuses_and_supersedes_single_episodes() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let core = Core::start(cfg.clone()).await.unwrap();
    let ep = |seed: u32, len: u32| -> Vec<u8> {
        (0..len)
            .map(|i| u8::try_from((i * 7 + seed) % 251).unwrap())
            .collect()
    };
    let (e1, e2) = (ep(1, 150_000), ep(2, 90_000));
    let tracker = unregistered_tracker().await;

    let single = |name: &str, data: &[u8], tracker: Option<&str>| {
        let mut b = TorrentBuilder::new(name, Version::V1)
            .piece_length(16 * 1024)
            .file(FileSource::from_bytes(&[], data.to_vec()));
        if let Some(t) = tracker {
            b = b.tracker_tier(vec![t.to_owned()]);
        }
        b.build().unwrap()
    };
    std::fs::write(cfg.download_dir.join("Show.S01E01.mkv"), &e1).unwrap();
    std::fs::write(cfg.download_dir.join("Show.S01E02.mkv"), &e2).unwrap();
    let s1 = core
        .add(add_bytes(&single("Show.S01E01.mkv", &e1, Some(&tracker))))
        .await
        .unwrap();
    let s2 = core
        .add(add_bytes(&single("Show.S01E02.mkv", &e2, None)))
        .await
        .unwrap();
    wait_for(&core, &s1, TorrentState::Seeding).await;
    wait_for(&core, &s2, TorrentState::Seeding).await;

    // The pack: the same episodes, under a season directory, at a different alignment.
    let pack_bytes = TorrentBuilder::new("Show Season 1", Version::V1)
        .piece_length(32 * 1024)
        .file(FileSource::from_bytes(&["Show.S01E01.mkv"], e1.clone()))
        .file(FileSource::from_bytes(&["Show.S01E02.mkv"], e2.clone()))
        .build()
        .unwrap();
    let pack = core.add(add_bytes(&pack_bytes)).await.unwrap();
    // No peers at all: it can only finish from the library.
    wait_for(&core, &pack, TorrentState::Seeding).await;
    let season = cfg.download_dir.join("Show Season 1");
    assert_eq!(std::fs::read(season.join("Show.S01E01.mkv")).unwrap(), e1);
    assert_eq!(std::fs::read(season.join("Show.S01E02.mkv")).unwrap(), e2);
    assert_eq!(
        core.store().get(&pack).unwrap().downloaded,
        0,
        "nothing downloaded"
    );
    assert_eq!(
        core.store().get(&s1).unwrap().superseded_by.as_deref(),
        Some(pack.as_str())
    );
    assert_eq!(
        core.store().get(&s2).unwrap().superseded_by.as_deref(),
        Some(pack.as_str())
    );

    // Episode 1's tracker calls it unregistered: it is retired, its file stays.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while core.store().get(&s1).is_some() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "episode 1 was never retired"
        );
        core.retire_superseded().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        cfg.download_dir.join("Show.S01E01.mkv").exists(),
        "retiring keeps the file"
    );
    assert!(
        core.store().get(&s2).is_some(),
        "episode 2's tracker is fine"
    );
    assert_eq!(
        core.store().get(&pack).unwrap().state,
        TorrentState::Seeding
    );
    core.shutdown().await.unwrap();
}

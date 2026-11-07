//! A torrent's life through the core: add, check, persist, restart, restore, remove.

use std::time::Duration;

use apitorrent_core::{Core, CoreConfig, CoreError};
use apitorrent_engine::SessionConfig;
use apitorrent_metainfo::{FileSource, TorrentBuilder, Version};
use apitorrent_types::{
    AddSource, AddTorrentRequest, FilePlacement, FilePriority, TorrentState, UnwantedPolicy,
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
    let e = core
        .add(bad(AddSource::Magnet {
            uri: "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(e.code(), "unsupported");
    core.shutdown().await.unwrap();
}

//! `apitorrentd import deluge` (the core's `deluge_import::run_offline`): a dry run that
//! changes nothing, an import into a new space that the next start picks up, and a second
//! import that finds everything already there.

use std::time::Duration;

use apitorrent_bencode::OwnedValue;
use apitorrent_core::deluge_import::{OfflineImport, run_offline};
use apitorrent_core::{Core, CoreConfig, Key};
use apitorrent_engine::SessionConfig;
use apitorrent_metainfo::{FileSource, TorrentBuilder, Version};
use apitorrent_types::TorrentState;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_import_into_a_new_space() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = root.path().join("data");
    let downloads = root.path().join("downloads");
    let deluge = root.path().join("deluge");
    let state = deluge.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let deluge_save = "/mnt/old-server/tv";
    let real_save = root.path().join("tv");

    let data: Vec<u8> = (0..50_000u32)
        .map(|i| u8::try_from(i % 239).unwrap())
        .collect();
    let bytes = TorrentBuilder::new("episode.mkv", Version::V1)
        .piece_length(16 * 1024)
        .file(FileSource::from_bytes(&[], data.clone()))
        .build()
        .unwrap();
    let t = apitorrent_metainfo::Torrent::from_bytes(&bytes).unwrap();
    let id = t.info.info_hashes().get_v1().unwrap().to_hex();
    std::fs::write(state.join(format!("{id}.torrent")), &bytes).unwrap();
    std::fs::create_dir_all(&real_save).unwrap();
    std::fs::write(real_save.join("episode.mkv"), &data).unwrap();
    let pieces = vec![1u8; usize::try_from(t.info.piece_count()).unwrap()];
    let resume = OwnedValue::dict()
        .insert("save_path", deluge_save)
        .insert("pieces", pieces)
        .insert("total_downloaded", 50_000i64)
        .insert("total_uploaded", 7i64)
        .to_bytes();
    std::fs::write(
        state.join("torrents.fastresume"),
        OwnedValue::dict().insert(id.as_str(), resume).to_bytes(),
    )
    .unwrap();
    std::fs::write(
        deluge.join("label.conf"),
        format!(
            r#"{{"file": 1, "format": 1}}{{"torrent_labels": {{"{id}": "shows"}},
               "labels": {{"shows": {{"move_completed": true, "move_completed_path": "/done"}}}}}}"#
        ),
    )
    .unwrap();
    let req = |dry_run| OfflineImport {
        config_dir: deluge.clone(),
        space: "tv".into(),
        dry_run,
        map: vec![(
            deluge_save.to_owned(),
            real_save.to_string_lossy().into_owned(),
        )],
        import_settings: false,
        paused: false,
    };

    // A dry run reports and changes nothing: not even the database is made.
    let report = run_offline(&data_dir, &downloads, &req(true)).unwrap();
    println!("{report}");
    assert!(report.contains("DRY RUN"), "{report}");
    assert!(report.contains("would be created"), "{report}");
    assert!(report.contains("would import:"), "{report}");
    assert!(
        report.contains(&real_save.to_string_lossy().into_owned()),
        "{report}"
    );
    assert!(report.contains("shows: 1"), "{report}");
    assert!(
        report.contains("label shows: move completed downloads to /done"),
        "{report}"
    );
    assert!(
        !data_dir.join("apitorrent.db").exists(),
        "a dry run writes nothing"
    );

    // The import makes the space and writes the torrent.
    let report = run_offline(&data_dir, &downloads, &req(false)).unwrap();
    assert!(report.contains("written:                  1"), "{report}");

    // The daemon starts it there, with Deluge's progress.
    let config = CoreConfig {
        data_dir: data_dir.clone(),
        download_dir: downloads.clone(),
        session: SessionConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            ..SessionConfig::default()
        },
    };
    let core = Core::start(config).await.unwrap();
    let tv = core.space_by_slug("tv").expect("the space was created").id;
    let key = Key::new(tv, id.clone());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while core
        .store()
        .get(&key)
        .is_none_or(|s| s.state != TorrentState::Seeding)
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{:?}",
            core.store().get(&key)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let s = core.store().get(&key).unwrap();
    assert_eq!(s.label.as_deref(), Some("shows"));
    assert_eq!(s.downloaded, 50_000);
    assert!(
        core.list(Some(apitorrent_types::spaces::DEFAULT_SPACE))
            .is_empty()
    );
    core.shutdown().await.unwrap();
    drop(core);

    // Importing again finds it already there.
    let report = run_offline(&data_dir, &downloads, &req(false)).unwrap();
    assert!(report.contains("already in this space:    1"), "{report}");
    assert!(report.contains("written:                  0"), "{report}");
}

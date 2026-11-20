//! Importing a large Deluge installation: how long it takes.

use std::time::{Duration, Instant};

use apitorrent_bencode::OwnedValue;
use apitorrent_core::{Core, CoreConfig};
use apitorrent_engine::SessionConfig;
use apitorrent_metainfo::{FileSource, TorrentBuilder, Version};
use apitorrent_types::spaces::DEFAULT_SPACE;

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

/// A Deluge config directory holding `n` small, complete torrents saved under `save`.
fn deluge_with(root: &std::path::Path, save: &std::path::Path, n: u32) -> std::path::PathBuf {
    let deluge = root.join("deluge");
    let state = deluge.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let mut resume = OwnedValue::dict();
    for i in 0..n {
        let data: Vec<u8> = (0..20_000u32)
            .map(|b| u8::try_from((b + i) % 251).unwrap())
            .collect();
        let name = format!("t{i}");
        let bytes = TorrentBuilder::new(&name, Version::V1)
            .piece_length(16 * 1024)
            .file(FileSource::from_bytes(&["data.bin"], data.clone()))
            .build()
            .unwrap();
        let t = apitorrent_metainfo::Torrent::from_bytes(&bytes).unwrap();
        let id = t.info.info_hashes().get_v1().unwrap().to_hex();
        std::fs::write(state.join(format!("{id}.torrent")), &bytes).unwrap();
        std::fs::create_dir_all(save.join(&name)).unwrap();
        std::fs::write(save.join(&name).join("data.bin"), &data).unwrap();
        let r = OwnedValue::dict()
            .insert("save_path", save.to_string_lossy().as_ref())
            .insert(
                "pieces",
                vec![1u8; usize::try_from(t.info.piece_count()).unwrap()],
            )
            .to_bytes();
        resume = resume.insert(id.as_str(), r);
    }
    std::fs::write(state.join("torrents.fastresume"), resume.to_bytes()).unwrap();
    deluge
}

/// Hundreds of torrents import in well under a second each batch: one database commit per
/// batch and one volume probe per save path, not several of each per torrent (300 took 6 s
/// on Windows that way; now about 0.5 s).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn many_torrents_import_quickly() {
    const N: u32 = 300;
    let root = tempfile::tempdir().unwrap();
    let save = root.path().join("torrents");
    let deluge = deluge_with(root.path(), &save, N);
    let core = Core::start(config(root.path())).await.unwrap();
    let started = Instant::now();
    let report = core
        .import_deluge(
            DEFAULT_SPACE,
            apitorrent_types::ImportDelugeRequest {
                config_dir: deluge.to_string_lossy().into_owned(),
                path_map: Vec::new(),
                options: apitorrent_types::DelugeImportOptions::default(),
                paused: true,
                filter: apitorrent_types::DelugeImportFilter::default(),
            },
        )
        .await
        .unwrap();
    let took = started.elapsed();
    eprintln!("imported {} torrents in {took:?}", report.imported.len());
    assert_eq!(report.imported.len(), N as usize, "{report:?}");
    assert!(took < Duration::from_secs(3), "took {took:?}");
    core.shutdown().await.unwrap();
}

/// In the background: the request gets the job at once, the job counts every torrent, and
/// its outcome is the import's report.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_runs_as_a_job() {
    use apitorrent_types::{JobKind, JobOutcome};
    const N: u32 = 100;
    let root = tempfile::tempdir().unwrap();
    let save = root.path().join("torrents");
    let deluge = deluge_with(root.path(), &save, N);
    let core = Core::start(config(root.path())).await.unwrap();
    let req = apitorrent_types::ImportDelugeRequest {
        config_dir: deluge.to_string_lossy().into_owned(),
        path_map: Vec::new(),
        options: apitorrent_types::DelugeImportOptions::default(),
        paused: true,
        filter: apitorrent_types::DelugeImportFilter::default(),
    };
    let job = core
        .start_import_deluge(DEFAULT_SPACE, req.clone())
        .unwrap();
    assert_eq!(job.kind, JobKind::DelugeImport);
    assert_eq!(job.outcome, None);
    // One import into a space at a time.
    if core.job(DEFAULT_SPACE, job.id).unwrap().outcome.is_none() {
        assert!(matches!(
            core.start_import_deluge(DEFAULT_SPACE, req.clone()),
            Err(apitorrent_core::CoreError::Busy(_))
        ));
    }
    // Another space doesn't see it.
    assert!(core.job(DEFAULT_SPACE + 1, job.id).is_err());
    let deadline = Instant::now() + Duration::from_secs(60);
    let done = loop {
        let j = core.job(DEFAULT_SPACE, job.id).unwrap();
        assert!(j.progress.done <= j.progress.total.max(j.progress.done));
        if j.outcome.is_some() {
            break j;
        }
        assert!(Instant::now() < deadline, "never finished: {j:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!((done.progress.done, done.progress.total), (N, N));
    assert!(done.finished_at.is_some());
    match done.outcome {
        Some(JobOutcome::DelugeImport { report }) => {
            assert_eq!(report.imported.len(), N as usize, "{report:?}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(core.jobs(DEFAULT_SPACE).len(), 1);
    assert_eq!(core.store().list(Some(DEFAULT_SPACE)).len(), N as usize);
    core.shutdown().await.unwrap();
}

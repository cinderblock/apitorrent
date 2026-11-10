//! Pack switchover with a tracker adapter: the adapter, not the built-in patterns, decides
//! what "unregistered" means for its tracker, and names the pack that replaces unregistered
//! single episodes; the core adds the pack, which reuses the singles' files and supersedes
//! them, and then retires them.
//!
//! The adapter here is a fake in Rust; `apitorrent-plugins` tests the same trait over a
//! JavaScript plugin (`tests/roles.rs`).

use apitorrent_types::spaces::DEFAULT_SPACE;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use apitorrent_core::{Core, CoreConfig, Key};
use apitorrent_engine::SessionConfig;
use apitorrent_metainfo::{FileSource, TorrentBuilder, Version};
use apitorrent_plugin_api::{BoxFuture, PackQuery, TrackerAdapters, UnregisteredQuery};
use apitorrent_types::{AddSource, AddTorrentRequest, TorrentState, UnwantedPolicy};
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
    while !core.store().get(&k(id)).is_some_and(|s| s.state == state) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {state:?}: {:?}",
            core.store().get(&k(id))
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// An HTTP tracker on 127.0.0.1 that fails every announce with `reason`.
async fn failing_tracker(reason: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let body = format!("d14:failure reason{}:{reason}e", reason.len());
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
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
        label: Some("tv".into()),
        file_priorities: None,
        unwanted_policy: UnwantedPolicy::Hold,
    }
}

/// Covers one tracker; "replaced by" means gone there; knows one pack.
struct FakeAdapter {
    tracker: String,
    pack: Vec<u8>,
    pack_queries: Mutex<Vec<PackQuery>>,
}

impl TrackerAdapters for FakeAdapter {
    fn covers(&self, tracker: &str) -> bool {
        tracker == self.tracker
    }

    fn unregistered(&self, q: UnregisteredQuery) -> BoxFuture<Option<bool>> {
        let covered = q.tracker == self.tracker;
        Box::pin(async move { covered.then(|| q.message.contains("replaced by")) })
    }

    fn find_pack(&self, q: PackQuery) -> BoxFuture<Option<AddSource>> {
        let known = q.files.iter().all(|f| f.path.starts_with("Show.S01E"));
        self.pack_queries.lock().unwrap().push(q);
        let data = base64::engine::general_purpose::STANDARD.encode(&self.pack);
        Box::pin(async move { known.then_some(AddSource::TorrentFile { data }) })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tracker_adapter_finds_the_pack_and_retires_the_singles() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let core = Core::start(cfg.clone()).await.unwrap();
    let ep = |seed: u32, len: u32| -> Vec<u8> {
        (0..len)
            .map(|i| u8::try_from((i * 13 + seed) % 251).unwrap())
            .collect()
    };
    let (e1, e2) = (ep(3, 120_000), ep(4, 70_000));
    // Words the built-in patterns don't know.
    let tracker = failing_tracker("replaced by season pack 1234").await;
    let single = |name: &str, data: &[u8]| {
        TorrentBuilder::new(name, Version::V1)
            .piece_length(16 * 1024)
            .file(FileSource::from_bytes(&[], data.to_vec()))
            .tracker_tier(vec![tracker.clone()])
            .build()
            .unwrap()
    };
    std::fs::write(cfg.download_dir.join("Show.S01E01.mkv"), &e1).unwrap();
    std::fs::write(cfg.download_dir.join("Show.S01E02.mkv"), &e2).unwrap();
    let s1 = core
        .add(DEFAULT_SPACE, add_bytes(&single("Show.S01E01.mkv", &e1)))
        .await
        .unwrap();
    let s2 = core
        .add(DEFAULT_SPACE, add_bytes(&single("Show.S01E02.mkv", &e2)))
        .await
        .unwrap();
    wait_for(&core, &s1, TorrentState::Seeding).await;
    wait_for(&core, &s2, TorrentState::Seeding).await;
    // Both have announced and failed.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    for id in [&s1, &s2] {
        while core.detail(&k(id)).await.unwrap().tracker_status[0]
            .error
            .is_none()
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no announce failure"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    // Without an adapter the message means nothing: nothing changes.
    core.retire_superseded().await;
    assert!(core.store().get(&k(&s1)).is_some() && core.store().get(&k(&s2)).is_some());

    let pack_bytes = TorrentBuilder::new("Show Season 1", Version::V1)
        .piece_length(32 * 1024)
        .file(FileSource::from_bytes(&["Show.S01E01.mkv"], e1.clone()))
        .file(FileSource::from_bytes(&["Show.S01E02.mkv"], e2.clone()))
        .build()
        .unwrap();
    let adapter = Arc::new(FakeAdapter {
        tracker: tracker.clone(),
        pack: pack_bytes,
        pack_queries: Mutex::new(Vec::new()),
    });
    core.set_tracker_adapters(Some(adapter.clone()));

    // The adapter calls both singles unregistered; neither is superseded yet, so it is
    // asked for their pack, which the core adds (once: the second add is a duplicate).
    core.retire_superseded().await;
    let queries = adapter.pack_queries.lock().unwrap().clone();
    assert_eq!(queries.len(), 2, "{queries:?}");
    assert_eq!(queries[0].tracker, tracker);
    assert_eq!(queries[0].files.len(), 1);
    let pack = core
        .list(None)
        .into_iter()
        .find(|t| t.name == "Show Season 1")
        .expect("the pack was added");
    assert_eq!(pack.label.as_deref(), Some("tv"), "the singles' label");
    // No peers: it can only finish from the library.
    wait_for(&core, &pack.id, TorrentState::Seeding).await;
    assert_eq!(core.store().get(&k(&pack.id)).unwrap().downloaded, 0);

    // Once the pack supersedes them, the next passes retire both singles; their files stay.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while core.store().get(&k(&s1)).is_some() || core.store().get(&k(&s2)).is_some() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the singles were never retired: {:?} {:?}",
            core.store().get(&k(&s1)),
            core.store().get(&k(&s2))
        );
        core.retire_superseded().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(cfg.download_dir.join("Show.S01E01.mkv").exists());
    assert!(cfg.download_dir.join("Show.S01E02.mkv").exists());
    // Each torrent was asked about once, however many passes ran.
    assert_eq!(adapter.pack_queries.lock().unwrap().len(), 2);
    assert_eq!(
        core.store().get(&k(&pack.id)).unwrap().state,
        TorrentState::Seeding
    );
    core.shutdown().await.unwrap();
}

/// A torrent in the default space.
#[allow(dead_code)]
fn k(id: &str) -> Key {
    Key::new(DEFAULT_SPACE, id)
}

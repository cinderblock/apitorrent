//! How apitorrent copes with many torrents: the "10,000 loaded torrents" and "startup under
//! 5 s" targets in `docs/architecture.md`, "Performance targets".
//!
//! ```sh
//! cargo run --release -p apitorrent-core --example scale -- 10000 <empty scratch dir>
//! ```
//!
//! It makes N single-file torrents with their data on disk (so they load as seeding), adds
//! them, restarts the core, and reports how long each step took, how big the UI's first
//! sync snapshot is, and the process's memory. Peers stay off: no DHT, LSD or trackers, and
//! the peer port is on loopback, so nothing reaches the network.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use apitorrent_core::{Core, CoreConfig};
use apitorrent_engine::SessionConfig;
use apitorrent_metainfo::{FileSource, TorrentBuilder, Version};
use apitorrent_types::{AddSource, AddTorrentRequest, TorrentState, UnwantedPolicy};
use base64::Engine as _;

const FILE_SIZE: usize = 64 * 1024;

fn mib(bytes: usize) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let b = bytes as f64;
    b / (1024.0 * 1024.0)
}

fn memory() -> String {
    memory_stats::memory_stats().map_or_else(
        || "unknown".to_owned(),
        |m| format!("{:.0} MiB physical", mib(m.physical_mem)),
    )
}

fn config(root: &std::path::Path) -> CoreConfig {
    CoreConfig {
        data_dir: root.join("data"),
        download_dir: root.join("downloads"),
        session: SessionConfig {
            listen: "127.0.0.1:0".parse().expect("address"),
            ..SessionConfig::default()
        },
    }
}

async fn wait_all_seeding(core: &Core, n: usize, limit: Duration) -> Duration {
    let start = Instant::now();
    loop {
        let list = core.list();
        let seeding = list
            .iter()
            .filter(|t| t.state == TorrentState::Seeding)
            .count();
        if seeding == n {
            return start.elapsed();
        }
        assert!(
            start.elapsed() < limit,
            "only {seeding} of {n} seeding after {limit:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let n: usize = args.next().map_or(10_000, |a| a.parse().expect("a count"));
    let root = PathBuf::from(args.next().expect("a scratch directory"));
    assert!(
        !root.exists() || std::fs::read_dir(&root).expect("readable").next().is_none(),
        "{} must be empty or missing",
        root.display()
    );
    let cfg = config(&root);
    std::fs::create_dir_all(&cfg.download_dir).expect("download dir");

    println!("making {n} torrents of about {} KiB each", FILE_SIZE / 1024);
    let t = Instant::now();
    let mut torrents = Vec::with_capacity(n);
    for i in 0..n {
        let name = format!("t{i:05}.bin");
        // Lengths differ, as they do in a real library; the content library only compares
        // files of equal length.
        let mut data = vec![0u8; FILE_SIZE + i];
        data[..8].copy_from_slice(&u64::try_from(i).expect("fits").to_le_bytes());
        std::fs::write(cfg.download_dir.join(&name), &data).expect("write data");
        let bytes = TorrentBuilder::new(&name, Version::Hybrid)
            .piece_length(16 * 1024)
            .file(FileSource::from_bytes(&[], data))
            .build()
            .expect("build");
        torrents.push(bytes);
    }
    println!("  made in {:.1?}", t.elapsed());

    let core = Core::start(cfg.clone()).await.expect("start");
    let t = Instant::now();
    for bytes in &torrents {
        core.add(AddTorrentRequest {
            source: AddSource::TorrentFile {
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
            },
            save_path: None,
            paused: false,
            label: None,
            file_priorities: None,
            unwanted_policy: UnwantedPolicy::Hold,
        })
        .await
        .expect("add");
    }
    let added = t.elapsed();
    let checked = wait_all_seeding(&core, n, Duration::from_secs(600)).await;
    println!(
        "added {n} in {added:.1?} ({:.2} ms each), all checked and seeding {checked:.1?} later; {}",
        added.as_secs_f64() * 1000.0 / f64::from(u32::try_from(n).expect("a sane count")),
        memory()
    );
    let t = Instant::now();
    core.shutdown().await.expect("shutdown");
    println!("  shutdown (saving resume state) in {:.1?}", t.elapsed());
    drop(core);

    let t = Instant::now();
    let core = Core::start(cfg).await.expect("restart");
    let started = t.elapsed();
    let listed = core.list().len();
    let seeding = wait_all_seeding(&core, n, Duration::from_secs(600)).await;
    println!(
        "restart: Core::start returned in {started:.1?} with {listed} listed; all seeding \
         {seeding:.1?} later; {}",
        memory()
    );

    let t = Instant::now();
    let list = core.list();
    let list_time = t.elapsed();
    let snapshot = serde_json::to_vec(&list).expect("serialize");
    println!(
        "list of {} in {list_time:.1?}; as a sync snapshot {:.1} MiB of JSON",
        list.len(),
        mib(snapshot.len())
    );
    core.shutdown().await.expect("shutdown");
}

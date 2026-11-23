//! Transfer speed over loopback, for comparing engine changes. Ignored by default; run in
//! release, on an otherwise idle machine, and compare runs from the same machine only:
//!
//! ```text
//! cargo test --release -p apitorrent-engine --test throughput -- --ignored --nocapture --test-threads 1
//! ```

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use apitorrent_engine::bitfield::Bitfield;
use apitorrent_engine::{AddTorrent, Session, SessionConfig};
use apitorrent_metainfo::{FileSource, Torrent, TorrentBuilder, Version};

const SIZE: usize = 1024 * 1024 * 1024;
const ROUNDS: usize = 3;
const PIECE: u64 = 1024 * 1024;

fn data(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len);
    out
}

fn fixture(root: &Path) -> Arc<Torrent> {
    let bytes = data(SIZE);
    std::fs::write(root.join("big.bin"), &bytes).unwrap();
    let built = TorrentBuilder::new("big.bin", Version::V1)
        .piece_length(PIECE)
        .file(FileSource::from_bytes(&[], bytes))
        .build()
        .unwrap();
    Arc::new(Torrent::from_bytes(&built).unwrap())
}

fn config(utp: bool) -> SessionConfig {
    SessionConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        utp,
        prefer_utp: utp,
        encryption: apitorrent_mse::Policy::Disabled,
        ..SessionConfig::default()
    }
}

/// One seeder, `leechers` leechers that know only the seeder: MiB/s of the seeder's upload
/// until every leecher is done.
async fn run(seed_dir: &Path, torrent: &Arc<Torrent>, utp: bool, leechers: usize) -> f64 {
    let seeder = Session::start(config(utp)).await.unwrap();
    let seed = seeder
        .add_torrent(AddTorrent::new(torrent.clone(), seed_dir.to_path_buf()))
        .unwrap();
    seed.wait_finished().await;
    let pieces = u32::try_from(torrent.info.piece_count()).unwrap();
    let mut sessions = Vec::new();
    let mut dirs = Vec::new();
    let start = Instant::now();
    for _ in 0..leechers {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::start(config(utp)).await.unwrap();
        let handle = session
            .add_torrent(AddTorrent {
                have: Some(Bitfield::new(pieces)),
                peers: vec![seeder.listen_addr()],
                ..AddTorrent::new(torrent.clone(), dir.path().to_path_buf())
            })
            .unwrap();
        sessions.push((session, handle));
        dirs.push(dir);
    }
    for (_, handle) in &sessions {
        tokio::time::timeout(Duration::from_secs(600), handle.wait_finished())
            .await
            .expect("download timed out");
    }
    let secs = start.elapsed().as_secs_f64();
    for (_, handle) in sessions {
        handle.shutdown().await;
    }
    seed.shutdown().await;
    f64::from(u32::try_from(SIZE * leechers / (1024 * 1024)).unwrap()) / secs
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "a benchmark: run in release"]
async fn throughput() {
    let seed_dir = tempfile::tempdir().unwrap();
    let torrent = fixture(seed_dir.path());
    for (utp, leechers) in [(false, 1), (false, 4), (true, 1), (true, 4)] {
        let mut rates = Vec::new();
        for _ in 0..ROUNDS {
            rates.push(run(seed_dir.path(), &torrent, utp, leechers).await);
        }
        rates.sort_by(f64::total_cmp);
        let transport = if utp { "uTP" } else { "TCP" };
        let all: Vec<String> = rates.iter().map(|r| format!("{r:.0}")).collect();
        println!(
            "{transport}, {leechers} leecher(s): median {:.0} MiB/s ({})",
            rates[rates.len() / 2],
            all.join(", ")
        );
    }
}

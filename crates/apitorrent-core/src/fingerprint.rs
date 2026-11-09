//! Fingerprints: a file's BEP 52 `pieces root` and full SHA-256, worked out in idle time
//! for files that came from v1 torrents.
//!
//! A v1 torrent says nothing about a file's v2 identity, so a later v2 or hybrid torrent of
//! the same file can only find it by length and then by reading pieces. With a fingerprint
//! it matches by root, with no I/O at all. The `pieces root` doesn't depend on the piece
//! length (the tree is built over 16 KiB blocks), so one fingerprint serves every future
//! torrent of the file.
//!
//! A fingerprint is tied to the file's size and modification time. If either changes it is
//! stale, and it is neither used nor kept.

use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use apitorrent_metainfo::Sha256Hash;
use apitorrent_metainfo::merkle::FileHasher;
use sha2::{Digest, Sha256};

/// What is known about one file on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    pub size: u64,
    /// Modification time, nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    pub pieces_root: Sha256Hash,
    pub sha256: [u8; 32],
}

/// A file's size and modification time, which a fingerprint must still match.
pub fn stamp(path: &Path) -> std::io::Result<(u64, i64)> {
    let meta = std::fs::metadata(path)?;
    let mtime = meta
        .modified()?
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    Ok((
        meta.len(),
        i64::try_from(mtime.as_nanos()).unwrap_or(i64::MAX),
    ))
}

impl Fingerprint {
    /// Whether the file at `path` is still the file this fingerprint describes.
    pub fn current(&self, path: &Path) -> bool {
        stamp(path).is_ok_and(|s| s == (self.size, self.mtime_ns))
    }
}

/// Why fingerprinting a file stopped short.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// It changed while being read; try again later.
    #[error("the file changed while it was read")]
    Changed,
    #[error("stopped")]
    Stopped,
}

const CHUNK: usize = 1024 * 1024;

/// Read the whole file once, at no more than `rate` bytes per second (0 means no limit),
/// and fingerprint it. Gives up when `stop` is set.
pub fn compute(path: &Path, rate: u64, stop: &AtomicBool) -> Result<Fingerprint, Error> {
    let before = stamp(path)?;
    let mut file = std::fs::File::open(path)?;
    let mut tree = FileHasher::new();
    let mut sha = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let started = Instant::now();
    let mut done: u64 = 0;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(Error::Stopped);
        }
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        tree.update(&buf[..n]);
        sha.update(&buf[..n]);
        done += n as u64;
        if rate > 0 {
            // Sleep until the average rate is back under the limit.
            #[allow(clippy::cast_precision_loss)]
            let due = Duration::from_secs_f64(done as f64 / rate as f64);
            if let Some(wait) = due.checked_sub(started.elapsed()) {
                std::thread::sleep(wait);
            }
        }
    }
    if stamp(path)? != before || done != before.0 {
        return Err(Error::Changed);
    }
    // The layer isn't wanted, only the root: a piece length past the end skips it.
    let (pieces_root, _) = tree.finish(u64::MAX);
    Ok(Fingerprint {
        size: before.0,
        mtime_ns: before.1,
        pieces_root,
        sha256: sha.finalize().into(),
    })
}

/// Seconds since the Unix epoch, for "computed at".
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use apitorrent_metainfo::{FileSource, Torrent, TorrentBuilder, Version};

    #[test]
    fn the_root_is_the_one_a_v2_torrent_of_the_file_has() {
        let dir = tempfile::tempdir().unwrap();
        // Not a whole number of blocks, and more than one piece at either piece length.
        let data: Vec<u8> = (0..300_001u32)
            .map(|i| u8::try_from(i % 241).unwrap())
            .collect();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, &data).unwrap();
        let fp = compute(&path, 0, &AtomicBool::new(false)).unwrap();
        assert_eq!(fp.size, data.len() as u64);
        assert_eq!(fp.sha256, <[u8; 32]>::from(Sha256::digest(&data)));
        for piece_length in [16 * 1024, 64 * 1024] {
            let bytes = TorrentBuilder::new("f.bin", Version::V2)
                .piece_length(piece_length)
                .file(FileSource::from_bytes(&[], data.clone()))
                .build()
                .unwrap();
            let t = Torrent::from_bytes(&bytes).unwrap();
            assert_eq!(t.info.files[0].pieces_root, Some(fp.pieces_root));
        }
        assert!(fp.current(&path));
        std::fs::write(&path, b"changed").unwrap();
        assert!(!fp.current(&path));
    }

    #[test]
    fn rate_limit_and_stop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, vec![7u8; 3 * CHUNK]).unwrap();
        // 3 MiB at 6 MiB/s takes about half a second.
        let t = Instant::now();
        compute(&path, 6 * 1024 * 1024, &AtomicBool::new(false)).unwrap();
        assert!(
            t.elapsed() >= Duration::from_millis(400),
            "{:?}",
            t.elapsed()
        );
        assert!(matches!(
            compute(&path, 0, &AtomicBool::new(true)),
            Err(Error::Stopped)
        ));
    }
}

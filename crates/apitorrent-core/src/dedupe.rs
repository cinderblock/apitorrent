//! Retroactive dedupe: identical files that torrents downloaded separately are merged into
//! hard links to one copy, freeing the space of the others.
//!
//! A pass (driven by `library_index`) gathers the complete files of every torrent, keeps
//! the sizes that occur more than once, and fingerprints those files (full SHA-256, in idle
//! time, rate-limited; see `fingerprint`). [`plan`] groups equal size and hash on the same
//! volume, and picks one copy to keep. Each other copy is then replaced:
//!
//! 1. [`verify`] reads both files in full and compares them byte for byte, and checks the
//!    target's SHA-256, size and modification time against what was planned.
//! 2. [`link_over`] runs with the torrent's engine holding the file (no reads or writes on
//!    it): it checks the size and time again, hard-links the keeper to a temporary name
//!    beside the target, and renames that over the target. A rename replaces the name
//!    atomically, so the path always names a complete, valid file: the old one or the new
//!    one. Any failure removes the temporary link and leaves the original in place.
//!
//! Both sides are marked `linked` afterwards, so the engine's copy-on-write keeps one
//! torrent from ever changing the other's file through the link.
//!
//! See `docs/architecture.md`, "Content library".

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use apitorrent_engine::fileid::{FileIdentity, identity};
use sha2::{Digest, Sha256};

use crate::fingerprint::stamp;
use crate::library::Linked;

/// Files smaller than this aren't worth merging.
pub const MIN_SIZE: u64 = 64 * 1024;

/// The suffix of the temporary link made beside a file being replaced.
pub const TEMP_SUFFIX: &str = ".apitorrent-dedupe";

/// One complete file of a torrent, as a pass found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copy {
    pub path: PathBuf,
    /// The torrent id and file index that own the path.
    pub owner: (String, usize),
    pub size: u64,
    /// Modification time (ns) when it was hashed.
    pub mtime_ns: i64,
    pub sha256: [u8; 32],
    pub id: FileIdentity,
}

/// One set of identical files on one volume, and what to do with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merge {
    pub size: u64,
    pub sha256: [u8; 32],
    /// The copy kept: indices (into the pass's copies) of its paths.
    pub keeper: Vec<usize>,
    /// The copies replaced by links to the keeper, each a list of its paths.
    pub targets: Vec<Vec<usize>>,
}

/// What [`plan`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub merges: Vec<Merge>,
    /// Sets of identical content held in at least two separate copies.
    pub groups: u64,
    pub reclaimable_bytes: u64,
    pub reclaimable_files: u64,
    /// Separate copies that can't be merged: alone on their volume, or with a link
    /// outside the files the pass knows.
    pub unmergeable_files: u64,
}

/// Group identical copies and choose what to merge. A copy is replaced only when every one
/// of its links is a path the pass knows: otherwise a link elsewhere (another space's copy,
/// a user's own link) keeps it alive, and replacing ours frees nothing. The keeper is the
/// copy with the most links already, then the one whose first path sorts first.
pub fn plan(copies: &[Copy]) -> Plan {
    // (size, hash) → volume → file → paths.
    type ByFile = BTreeMap<u128, Vec<usize>>;
    let mut sets: BTreeMap<(u64, [u8; 32]), BTreeMap<u64, ByFile>> = BTreeMap::new();
    for (i, c) in copies.iter().enumerate() {
        sets.entry((c.size, c.sha256))
            .or_default()
            .entry(c.id.volume)
            .or_default()
            .entry(c.id.index)
            .or_default()
            .push(i);
    }
    let mut out = Plan::default();
    for ((size, sha256), volumes) in sets {
        let separate: usize = volumes.values().map(BTreeMap::len).sum();
        if separate < 2 {
            continue;
        }
        out.groups += 1;
        for files in volumes.into_values() {
            if files.len() < 2 {
                out.unmergeable_files += files.values().map(|p| p.len() as u64).sum::<u64>();
                continue;
            }
            let mut files: Vec<Vec<usize>> = files.into_values().collect();
            for paths in &mut files {
                paths.sort_by(|&a, &b| copies[a].path.cmp(&copies[b].path));
            }
            let links = |paths: &[usize]| paths.iter().map(|&i| copies[i].id.links).max();
            files.sort_by(|a, b| {
                links(b)
                    .cmp(&links(a))
                    .then_with(|| copies[a[0]].path.cmp(&copies[b[0]].path))
            });
            let keeper = files.remove(0);
            let mut targets = Vec::new();
            for paths in files {
                let known = paths.len() as u64;
                if links(&paths).is_some_and(|l| l == known) {
                    out.reclaimable_bytes += size;
                    out.reclaimable_files += known;
                    targets.push(paths);
                } else {
                    out.unmergeable_files += known;
                }
            }
            if !targets.is_empty() {
                out.merges.push(Merge {
                    size,
                    sha256,
                    keeper,
                    targets,
                });
            }
        }
    }
    out
}

/// Why a copy wasn't replaced.
#[derive(Debug, thiserror::Error)]
pub enum SwapError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Its size or time changed since it was hashed, or while it was being read.
    #[error("the file changed")]
    Changed,
    /// The bytes differ from the keeper's, or from the hash planned.
    #[error("the files differ")]
    Different,
    /// The two paths are already the same file, or on different volumes.
    #[error("the files can't be linked")]
    NotLinkable,
    #[error("stopped")]
    Stopped,
}

const CHUNK: usize = 1024 * 1024;

/// Read `keeper` and `target` in full, at no more than `rate` bytes per second together
/// (0 means no limit), and check that they are byte for byte the same, that the target's
/// SHA-256 is `sha256`, and that both still have the sizes and times planned (before and
/// after reading).
pub fn verify(
    keeper: &Path,
    keeper_stamp: (u64, i64),
    target: &Path,
    target_stamp: (u64, i64),
    sha256: &[u8; 32],
    rate: u64,
    stop: &AtomicBool,
) -> Result<(), SwapError> {
    if stamp(keeper)? != keeper_stamp || stamp(target)? != target_stamp {
        return Err(SwapError::Changed);
    }
    let (ki, ti) = (identity(keeper)?, identity(target)?);
    if ki.volume != ti.volume || ki.same_file(&ti) {
        return Err(SwapError::NotLinkable);
    }
    let mut a = std::fs::File::open(keeper)?;
    let mut b = std::fs::File::open(target)?;
    let (mut ba, mut bb) = (vec![0u8; CHUNK], vec![0u8; CHUNK]);
    let mut sha = Sha256::new();
    let started = Instant::now();
    let mut done: u64 = 0;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(SwapError::Stopped);
        }
        let n = read_full(&mut a, &mut ba)?;
        let m = read_full(&mut b, &mut bb)?;
        if n != m || ba[..n] != bb[..m] {
            return Err(SwapError::Different);
        }
        if n == 0 {
            break;
        }
        sha.update(&bb[..n]);
        done += 2 * n as u64;
        if rate > 0 {
            #[allow(clippy::cast_precision_loss)]
            let due = Duration::from_secs_f64(done as f64 / rate as f64);
            if let Some(wait) = due.checked_sub(started.elapsed()) {
                std::thread::sleep(wait);
            }
        }
    }
    if <[u8; 32]>::from(sha.finalize()) != *sha256 {
        return Err(SwapError::Different);
    }
    if stamp(keeper)? != keeper_stamp || stamp(target)? != target_stamp {
        return Err(SwapError::Changed);
    }
    Ok(())
}

/// Fill `buf` as far as the file allows; fewer bytes only at its end.
fn read_full(f: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// The temporary name beside `target` that its replacement is made under.
pub fn temp_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(TEMP_SUFFIX);
    target.with_file_name(name)
}

/// Replace `target` with a hard link to `keeper` (or, where hard links fail, a
/// copy-on-write clone of it): link to a temporary name beside the target, check that the
/// target still has `target_stamp`, run `before_rename` (a last check of the caller's; an
/// error stops the swap), and rename the link over the target. Any failure removes the
/// temporary link and leaves the target as it was. Call it with the file's torrent holding
/// it (`TorrentHandle::replace_file`), after [`verify`].
pub fn link_over(
    keeper: &Path,
    target: &Path,
    target_stamp: (u64, i64),
    before_rename: &mut dyn FnMut() -> std::io::Result<()>,
) -> Result<Linked, SwapError> {
    let tmp = temp_path(target);
    // A leftover of an interrupted swap: our own name, so it is ours to remove.
    match std::fs::remove_file(&tmp) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    let result = (|| {
        if stamp(target)? != target_stamp {
            return Err(SwapError::Changed);
        }
        let how = if std::fs::hard_link(keeper, &tmp).is_ok() {
            if !identity(&tmp)?.same_file(&identity(keeper)?) {
                return Err(SwapError::NotLinkable);
            }
            Linked::HardLink
        } else if reflink_copy::reflink(keeper, &tmp).is_ok() {
            Linked::Reflink
        } else {
            return Err(SwapError::NotLinkable);
        };
        if stamp(target)? != target_stamp {
            return Err(SwapError::Changed);
        }
        before_rename()?;
        std::fs::rename(&tmp, target)?;
        Ok(how)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// The files of a pass, by size, of which only sizes held by two or more separate files
/// need hashing. Returns the indices worth hashing, one per separate file.
pub fn needing_hash<T>(
    files: &[T],
    size: impl Fn(&T) -> u64,
    id: impl Fn(&T) -> FileIdentity,
) -> Vec<usize> {
    let mut by_size: HashMap<u64, HashMap<(u64, u128), usize>> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        let id = id(f);
        by_size
            .entry(size(f))
            .or_default()
            .entry((id.volume, id.index))
            .or_insert(i);
    }
    let mut out: Vec<usize> = by_size
        .into_values()
        .filter(|ids| ids.len() > 1)
        .flat_map(HashMap::into_values)
        .collect();
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    fn copy(path: &Path, owner: &str, sha: u8) -> Copy {
        let (size, mtime_ns) = stamp(path).unwrap();
        Copy {
            path: path.to_path_buf(),
            owner: (owner.into(), 0),
            size,
            mtime_ns,
            sha256: [sha; 32],
            id: identity(path).unwrap(),
        }
    }

    #[test]
    #[allow(clippy::many_single_char_names)]
    fn plan_merges_separate_copies_only() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, c, d, e) = (
            dir.path().join("a"),
            dir.path().join("b"),
            dir.path().join("c"),
            dir.path().join("d"),
            dir.path().join("e"),
        );
        for p in [&a, &b, &d, &e] {
            write(p, &[1; 100]);
        }
        // c is already a link of a: no space to win there.
        std::fs::hard_link(&a, &c).unwrap();
        // e has a link the pass doesn't know about.
        std::fs::hard_link(&e, dir.path().join("elsewhere")).unwrap();
        let copies = vec![
            copy(&a, "t1", 7),
            copy(&b, "t2", 7),
            copy(&c, "t3", 7),
            copy(&d, "t4", 9),
            copy(&e, "t5", 7),
        ];
        let p = plan(&copies);
        assert_eq!(p.groups, 1, "d is alone with its hash");
        assert_eq!(p.merges.len(), 1);
        let m = &p.merges[0];
        // a/c has two links: it is kept, and b is replaced.
        assert_eq!(m.keeper, vec![0, 2]);
        assert_eq!(m.targets, vec![vec![1]]);
        assert_eq!((p.reclaimable_bytes, p.reclaimable_files), (100, 1));
        assert_eq!(p.unmergeable_files, 1, "e");
    }

    #[test]
    fn needing_hash_skips_unique_sizes_and_links() {
        #[derive(Clone, Copy)]
        struct F(u64, u128);
        let id = |f: &F| FileIdentity {
            volume: 1,
            index: f.1,
            links: 1,
        };
        let files = [F(10, 1), F(10, 1), F(20, 2), F(30, 3), F(30, 4)];
        assert_eq!(needing_hash(&files, |f| f.0, id), vec![3, 4]);
    }

    #[test]
    fn verify_and_swap() {
        let dir = tempfile::tempdir().unwrap();
        let keeper = dir.path().join("one").join("x.bin");
        let target = dir.path().join("two").join("x.bin");
        let bytes: Vec<u8> = (0..300_000u32).map(|i| (i % 253) as u8).collect();
        write(&keeper, &bytes);
        write(&target, &bytes);
        let sha: [u8; 32] = Sha256::digest(&bytes).into();
        let (ks, ts) = (stamp(&keeper).unwrap(), stamp(&target).unwrap());
        let stop = AtomicBool::new(false);
        verify(&keeper, ks, &target, ts, &sha, 0, &stop).unwrap();
        assert!(matches!(
            verify(&keeper, ks, &target, ts, &[0; 32], 0, &stop),
            Err(SwapError::Different)
        ));
        let how = link_over(&keeper, &target, ts, &mut || Ok(())).unwrap();
        assert_eq!(how, Linked::HardLink);
        assert!(
            identity(&keeper)
                .unwrap()
                .same_file(&identity(&target).unwrap())
        );
        assert_eq!(std::fs::read(&target).unwrap(), bytes);
        assert!(!temp_path(&target).exists());
        // Now the same file: nothing more to do.
        let ts = stamp(&target).unwrap();
        assert!(matches!(
            verify(&keeper, ks, &target, ts, &sha, 0, &stop),
            Err(SwapError::NotLinkable)
        ));
    }

    /// A file that changes between hashing and the swap is never replaced.
    #[test]
    fn a_changed_file_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let keeper = dir.path().join("k.bin");
        let target = dir.path().join("t.bin");
        let bytes = vec![3u8; 200_000];
        write(&keeper, &bytes);
        write(&target, &bytes);
        let sha: [u8; 32] = Sha256::digest(&bytes).into();
        let (ks, ts) = (stamp(&keeper).unwrap(), stamp(&target).unwrap());
        // Same size, other bytes, later time.
        std::thread::sleep(Duration::from_millis(20));
        let mut changed = bytes.clone();
        changed[100_000] = 4;
        std::fs::write(&target, &changed).unwrap();
        let stop = AtomicBool::new(false);
        assert!(matches!(
            verify(&keeper, ks, &target, ts, &sha, 0, &stop),
            Err(SwapError::Changed)
        ));
        assert!(matches!(
            link_over(&keeper, &target, ts, &mut || Ok(())),
            Err(SwapError::Changed)
        ));
        assert_eq!(std::fs::read(&target).unwrap(), changed);
        assert!(
            !identity(&keeper)
                .unwrap()
                .same_file(&identity(&target).unwrap())
        );
        assert!(!temp_path(&target).exists());
    }

    /// An interrupted swap leaves a valid file at the path: a failure at the last moment
    /// keeps the original (and removes the temporary link), and a temporary link a crash
    /// left behind is cleared by the next swap.
    #[test]
    fn an_interrupted_swap_leaves_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let keeper = dir.path().join("k.bin");
        let target = dir.path().join("t.bin");
        let bytes = vec![5u8; 100_000];
        write(&keeper, &bytes);
        write(&target, &bytes);
        let before = identity(&target).unwrap();
        let ts = stamp(&target).unwrap();

        let err = link_over(&keeper, &target, ts, &mut || {
            // The link exists beside the target, and the target is still the original.
            assert!(temp_path(&target).exists());
            assert!(identity(&target).unwrap().same_file(&before));
            Err(std::io::Error::other("power cut"))
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "I/O error: power cut");
        assert!(identity(&target).unwrap().same_file(&before));
        assert_eq!(std::fs::read(&target).unwrap(), bytes);
        assert!(!temp_path(&target).exists());

        // A crash after linking leaves the temporary link; the path is still the original.
        std::fs::hard_link(&keeper, temp_path(&target)).unwrap();
        assert!(identity(&target).unwrap().same_file(&before));
        link_over(&keeper, &target, ts, &mut || Ok(())).unwrap();
        assert!(
            identity(&target)
                .unwrap()
                .same_file(&identity(&keeper).unwrap())
        );
        assert!(!temp_path(&target).exists());
        assert_eq!(std::fs::read(&target).unwrap(), bytes);
    }
}

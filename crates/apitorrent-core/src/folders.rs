//! Library folders: files outside torrents (a media folder, the files a removed torrent
//! left behind) that new torrents can reuse instead of downloading.
//!
//! A server admin adds folders. A rate-limited walk records each file's size and
//! modification time in the database (`db::library`), and in idle time each file is
//! fingerprinted (v2 `pieces root` and SHA-256, see `fingerprint`). Walks repeat on a
//! schedule, so new, changed and removed files are noticed; a changed file loses its
//! fingerprint, and a fingerprint is only ever trusted while the file still has the size
//! and time it was taken at.
//!
//! When a torrent is added, [`find_matches`] offers folder files as candidates for the
//! files no torrent had: by root with no I/O, else by length (same name first) and then by
//! checking sampled pieces at the new torrent's alignment, as for torrents' files. A match
//! is hard-linked into the torrent's save path (the engine treats it as copy-on-write), or
//! cloned or copied. Folder files are read-only sources: apitorrent never writes, renames
//! or deletes them.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use apitorrent_engine::layout::Layout;
use apitorrent_engine::verify::PieceHashes;
use apitorrent_metainfo::Torrent;

use crate::db::FolderFile;
use crate::library::{Fit, MAX_CANDIDATES, Match, MatchedBy, check_pieces};

/// The walk pauses this long after every [`WALK_BATCH`] entries, so a large folder doesn't
/// flood the disk with metadata reads.
const WALK_PAUSE: Duration = Duration::from_millis(10);
const WALK_BATCH: usize = 200;

/// Names apitorrent gives its own temporary files; never library files.
const TEMP_SUFFIXES: [&str; 3] = [
    ".apitorrent-copy",
    ".apitorrent-unshare",
    crate::dedupe::TEMP_SUFFIX,
];

/// Why a walk stopped.
#[derive(Debug, thiserror::Error)]
pub enum WalkError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("stopped")]
    Stopped,
}

/// Every regular file under `root` with a size above 0: absolute path, size and
/// modification time (ns). Symbolic links aren't followed. Unreadable subfolders are
/// skipped; an unreadable `root` is an error.
pub fn walk(root: &Path, stop: &AtomicBool) -> Result<Vec<(String, u64, i64)>, WalkError> {
    let mut out = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    let mut seen = 0usize;
    let mut first = true;
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if first => return Err(e.into()),
            Err(e) => {
                tracing::debug!(dir = %dir.display(), error = %e, "library folder: skipped a folder");
                continue;
            }
        };
        first = false;
        for entry in entries.flatten() {
            seen += 1;
            if seen.is_multiple_of(WALK_BATCH) {
                if stop.load(Ordering::Relaxed) {
                    return Err(WalkError::Stopped);
                }
                std::thread::sleep(WALK_PAUSE);
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                stack.push(path);
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if TEMP_SUFFIXES.iter().any(|s| name.ends_with(s)) {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.len() == 0 {
                continue;
            }
            let mtime = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
            out.push((path.to_string_lossy().into_owned(), meta.len(), mtime));
        }
    }
    out.sort();
    Ok(out)
}

/// What changed between a folder's last walk (`old`) and this one (`new`): files to add
/// or update, and files gone.
pub fn diff(
    old: &[(String, u64, i64)],
    new: &[(String, u64, i64)],
) -> (Vec<(String, u64, i64)>, Vec<String>) {
    let before: std::collections::HashMap<&str, (u64, i64)> =
        old.iter().map(|(p, s, t)| (p.as_str(), (*s, *t))).collect();
    let now: HashSet<&str> = new.iter().map(|(p, _, _)| p.as_str()).collect();
    let upserts = new
        .iter()
        .filter(|(p, s, t)| before.get(p.as_str()) != Some(&(*s, *t)))
        .cloned()
        .collect();
    let deletes = old
        .iter()
        .filter(|(p, _, _)| !now.contains(p.as_str()))
        .map(|(p, _, _)| p.clone())
        .collect();
    (upserts, deletes)
}

/// Whether the folder file is still as recorded (size and time).
fn current(f: &FolderFile) -> bool {
    crate::fingerprint::stamp(Path::new(&f.path)).is_ok_and(|s| s == (f.size, f.mtime_ns))
}

/// The id a folder file's match carries as its source.
pub fn source_id(folder_id: i64) -> String {
    format!("folder:{folder_id}")
}

/// Find folder files for the new torrent's files that `taken` (matched elsewhere) leaves.
/// As in `library::find_matches`, files already at their target and files with priority 0
/// are left alone, and with `partial` files that match only in places are returned too.
pub fn find_matches<S: std::hash::BuildHasher>(
    new: &Torrent,
    targets: &[PathBuf],
    priorities: &[u8],
    files: &[FolderFile],
    partial: bool,
    taken: &HashSet<usize, S>,
) -> Vec<Match> {
    if files.is_empty() {
        return Vec::new();
    }
    let layout = Layout::from_info(&new.info);
    let hashes = PieceHashes::new(new);
    let mut out = Vec::new();
    for (i, f) in new.info.files.iter().enumerate() {
        if f.is_pad()
            || f.length == 0
            || taken.contains(&i)
            || priorities.get(i) == Some(&0)
            || targets[i].exists()
        {
            continue;
        }
        let mut cands: Vec<&FolderFile> = files.iter().filter(|c| c.size == f.length).collect();
        if cands.is_empty() {
            continue;
        }
        let found = |c: &FolderFile, full: bool, by: MatchedBy| Match {
            file: i,
            source_id: source_id(c.folder_id),
            source_file: 0,
            from: PathBuf::from(&c.path),
            full,
            by,
        };
        // By root: no reading, but only while the fingerprint is current.
        if let Some(root) = f.pieces_root
            && let Some(c) = cands
                .iter()
                .find(|c| c.pieces_root == Some(root.0) && current(c))
        {
            out.push(found(c, true, MatchedBy::Fingerprint));
            continue;
        }
        let name = f.path.last().map(String::as_str);
        let name_of = |c: &FolderFile| {
            Path::new(&c.path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        };
        cands.sort_by_key(|c| name_of(c).as_deref() != name);
        let mut best: Option<(&FolderFile, f64)> = None;
        let mut hit = None;
        for c in cands.into_iter().take(MAX_CANDIDATES) {
            // A file whose root is known and differs can't be this one (v2 torrents only).
            if let (Some(root), Some(theirs)) = (f.pieces_root, c.pieces_root)
                && theirs != root.0
                && current(c)
            {
                continue;
            }
            let same_name = name_of(c).as_deref() == name;
            match check_pieces(&hashes, &layout, f.offset, f.length, i, Path::new(&c.path)) {
                Fit::Full => {
                    hit = Some(found(c, true, MatchedBy::Pieces));
                    break;
                }
                Fit::Unknown if same_name => {
                    hit = Some(found(c, true, MatchedBy::Name));
                    break;
                }
                Fit::Partial { passed, checked } if partial => {
                    #[allow(clippy::cast_precision_loss)]
                    let score = passed as f64 / checked as f64;
                    if best.is_none_or(|(_, b)| score > b) {
                        best = Some((c, score));
                    }
                }
                _ => {}
            }
        }
        if let Some(m) = hit.or_else(|| best.map(|(c, _)| found(c, false, MatchedBy::Pieces))) {
            out.push(m);
        }
    }
    out
}

/// The library folder `path` is under, if any: the longest folder path that contains it.
pub fn folder_of<'a>(path: &Path, folders: &'a [(i64, PathBuf)]) -> Option<&'a (i64, PathBuf)> {
    folders
        .iter()
        .filter(|(_, root)| path.starts_with(root))
        .max_by_key(|(_, root)| root.as_os_str().len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use apitorrent_metainfo::{FileSource, TorrentBuilder, Version};

    fn data(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| {
                u8::try_from(i % 251)
                    .unwrap()
                    .wrapping_mul(7)
                    .wrapping_add(seed)
            })
            .collect()
    }

    fn record(folder: i64, p: &Path, fp: Option<&crate::fingerprint::Fingerprint>) -> FolderFile {
        let (size, mtime_ns) = crate::fingerprint::stamp(p).unwrap();
        FolderFile {
            folder_id: folder,
            path: p.to_string_lossy().into_owned(),
            size,
            mtime_ns,
            pieces_root: fp.map(|f| f.pieces_root.0),
            sha256: fp.map(|f| f.sha256),
        }
    }

    #[test]
    fn walk_and_diff() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a").join("b")).unwrap();
        std::fs::write(dir.path().join("a").join("b").join("x.mkv"), b"xx").unwrap();
        std::fs::write(dir.path().join("y.mkv"), b"y").unwrap();
        std::fs::write(dir.path().join("empty"), b"").unwrap();
        std::fs::write(dir.path().join("y.mkv.apitorrent-copy"), b"tmp").unwrap();
        let stop = AtomicBool::new(false);
        let found = walk(dir.path(), &stop).unwrap();
        let names: Vec<&str> = found
            .iter()
            .map(|(p, _, _)| Path::new(p).file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.contains(&"x.mkv") && names.contains(&"y.mkv"));
        assert!(walk(&dir.path().join("missing"), &stop).is_err());

        let mut changed = found.clone();
        changed[0].2 += 1;
        changed.pop();
        let (up, del) = diff(&found, &changed);
        assert_eq!(up, vec![changed[0].clone()]);
        assert_eq!(del, vec![found[1].0.clone()]);
        assert_eq!(diff(&found, &found), (Vec::new(), Vec::new()));
    }

    #[test]
    fn folder_files_match_by_root_or_pieces_and_stale_roots_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = data(200_000, 3);
        let file = dir.path().join("media").join("Movie (2020).mkv");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, &bytes).unwrap();
        let fp = crate::fingerprint::compute(&file, 0, &AtomicBool::new(false)).unwrap();

        let v2 = Torrent::from_bytes(
            &TorrentBuilder::new("other-name.mkv", Version::V2)
                .piece_length(32 * 1024)
                .file(FileSource::from_bytes(&[], bytes.clone()))
                .build()
                .unwrap(),
        )
        .unwrap();
        let targets = vec![dir.path().join("dl").join("other-name.mkv")];
        let none = HashSet::new();
        let m = find_matches(
            &v2,
            &targets,
            &[4],
            &[record(1, &file, Some(&fp))],
            true,
            &none,
        );
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].by, m[0].full), (MatchedBy::Fingerprint, true));
        assert_eq!(m[0].source_id, "folder:1");
        assert!(
            find_matches(
                &v2,
                &targets,
                &[4],
                &[record(1, &file, Some(&fp))],
                true,
                &HashSet::from([0])
            )
            .is_empty(),
            "a file matched elsewhere is left alone"
        );

        // A v1 torrent at another alignment: found by length and checked pieces.
        let v1 = Torrent::from_bytes(
            &TorrentBuilder::new("x", Version::V1)
                .piece_length(16 * 1024)
                .file(FileSource::from_bytes(&["pad.bin"], data(5_000, 9)))
                .file(FileSource::from_bytes(&["Movie (2020).mkv"], bytes.clone()))
                .build()
                .unwrap(),
        )
        .unwrap();
        let targets1 = vec![
            dir.path().join("dl").join("pad.bin"),
            dir.path().join("dl").join("Movie (2020).mkv"),
        ];
        let m = find_matches(
            &v1,
            &targets1,
            &[4, 4],
            &[record(1, &file, None)],
            true,
            &none,
        );
        assert_eq!(m.len(), 1);
        assert_eq!(
            (m[0].file, m[0].by, m[0].full),
            (1, MatchedBy::Pieces, true)
        );

        // The file changes (same size, other bytes): the stale root isn't trusted, and the
        // pieces no longer check out.
        let stale = record(1, &file, Some(&fp));
        std::thread::sleep(Duration::from_millis(20));
        let mut changed = bytes.clone();
        changed[100_000] ^= 0xff;
        for b in &mut changed[..50_000] {
            *b = b.wrapping_add(1);
        }
        std::fs::write(&file, &changed).unwrap();
        assert!(find_matches(&v2, &targets, &[4], &[stale], false, &none).is_empty());
    }

    #[test]
    fn the_innermost_folder_owns_a_path() {
        let folders = vec![
            (1, PathBuf::from("/m")),
            (2, PathBuf::from("/m/tv")),
            (3, PathBuf::from("/x")),
        ];
        assert_eq!(folder_of(Path::new("/m/tv/a.mkv"), &folders).unwrap().0, 2);
        assert_eq!(folder_of(Path::new("/m/a.mkv"), &folders).unwrap().0, 1);
        assert!(folder_of(Path::new("/mtv/a.mkv"), &folders).is_none());
    }
}

//! The content library: reusing files other torrents already have.
//!
//! When a torrent is added, each of its files is looked for among the files of the
//! torrents already here. A match is linked into the new torrent's save path (a hard link,
//! or a copy across volumes), so it isn't downloaded again, and the old torrent keeps
//! seeding its own copy. The new torrent then checks its files as usual: pieces that
//! verify are kept, and any that don't are downloaded. Linked files are copy-on-write in
//! the engine, so a torrent never changes another torrent's file through a link.
//!
//! How a candidate is trusted:
//!
//! - **v2 and hybrid files**: the same `pieces root` and length mean the same content.
//! - **v1 files**: the same length, preferring the same name, and one piece that lies
//!   wholly inside the file is hashed at the new torrent's alignment. Files too small to
//!   hold a whole piece need a matching name as well; the check after linking settles it.
//!
//! See `docs/architecture.md`, "Content library".

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use apitorrent_engine::bitfield::Bitfield;
use apitorrent_engine::layout::Layout;
use apitorrent_metainfo::{Sha1Hash, Torrent};

/// A torrent whose files may be reused.
#[derive(Debug, Clone)]
pub struct Source {
    pub id: String,
    pub torrent: Arc<Torrent>,
    /// Where each file is on disk, or `None` where it isn't in the save path (deselected).
    pub paths: Vec<Option<PathBuf>>,
    pub have: Bitfield,
}

impl Source {
    /// Whether file `i` is completely downloaded: every piece it touches is had.
    fn complete(&self, layout: &Layout, i: usize) -> bool {
        layout.file_pieces(i).all(|p| self.have.get(p))
    }
}

/// One file of the new torrent and where it can come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub file: usize,
    pub source_id: String,
    pub source_file: usize,
    pub from: PathBuf,
}

/// How a match was put in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Linked {
    HardLink,
    Copy,
}

/// Find existing complete copies of the new torrent's files. `targets` is where each file
/// of the new torrent would go; files already there, and files with priority 0, are left
/// alone.
pub fn find_matches(
    new: &Torrent,
    targets: &[PathBuf],
    priorities: &[u8],
    sources: &[Source],
) -> Vec<Match> {
    let layout = Layout::from_info(&new.info);
    let source_layouts: Vec<Layout> = sources
        .iter()
        .map(|s| Layout::from_info(&s.torrent.info))
        .collect();
    // Index the sources' complete files by root and by length.
    let mut by_root = HashMap::new();
    let mut by_length: HashMap<u64, Vec<(usize, usize)>> = HashMap::new();
    for (si, s) in sources.iter().enumerate() {
        for (fi, f) in s.torrent.info.files.iter().enumerate() {
            if f.is_pad() || f.length == 0 || s.paths.get(fi).is_none_or(Option::is_none) {
                continue;
            }
            if !s.complete(&source_layouts[si], fi) {
                continue;
            }
            if let Some(root) = f.pieces_root {
                by_root.entry((root, f.length)).or_insert((si, fi));
            }
            by_length.entry(f.length).or_default().push((si, fi));
        }
    }

    let mut out = Vec::new();
    for (i, f) in new.info.files.iter().enumerate() {
        if f.is_pad() || f.length == 0 || priorities.get(i) == Some(&0) || targets[i].exists() {
            continue;
        }
        let found = f
            .pieces_root
            .and_then(|r| by_root.get(&(r, f.length)).copied())
            .or_else(|| {
                let name = f.path.last();
                let mut cands = by_length.get(&f.length).cloned().unwrap_or_default();
                // Same name first.
                cands.sort_by_key(|&(si, fi)| {
                    sources[si].torrent.info.files[fi].path.last() != name
                });
                cands.into_iter().find(|&(si, fi)| {
                    let same_name = sources[si].torrent.info.files[fi].path.last() == name;
                    let from = sources[si].paths[fi]
                        .as_ref()
                        .expect("indexed only with a path");
                    match sample_check(new, &layout, i, from) {
                        Some(ok) => ok,
                        None => same_name,
                    }
                })
            });
        if let Some((si, fi)) = found {
            out.push(Match {
                file: i,
                source_id: sources[si].id.clone(),
                source_file: fi,
                from: sources[si].paths[fi]
                    .clone()
                    .expect("indexed only with a path"),
            });
        }
    }
    out
}

/// Hash one piece of the new torrent that lies wholly inside file `i`, reading it from the
/// candidate. `None` when there is no such piece (or no v1 hashes to check against).
fn sample_check(new: &Torrent, layout: &Layout, i: usize, candidate: &Path) -> Option<bool> {
    let f = &new.info.files[i];
    let pl = layout.piece_length();
    let first_whole = f.offset.div_ceil(pl);
    let piece_end = (first_whole + 1) * pl;
    if new.info.v1_pieces.is_empty() || piece_end > f.offset + f.length {
        return None;
    }
    let piece = u32::try_from(first_whole).ok()?;
    let expected = *new.info.v1_pieces.get(piece as usize)?;
    let mut buf = vec![0u8; usize::try_from(pl).ok()?];
    let at = first_whole * pl - f.offset;
    let ok = read_at(candidate, at, &mut buf).is_ok() && Sha1Hash::digest(&buf) == expected;
    Some(ok)
}

fn read_at(path: &Path, at: u64, buf: &mut [u8]) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(at))?;
    f.read_exact(buf)
}

/// Put a match in place: a hard link when the volume allows, otherwise a copy (when
/// `allow_copy`). Returns `None` if neither worked.
pub fn materialize(from: &Path, to: &Path, allow_copy: bool) -> Option<Linked> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir).ok()?;
    }
    if std::fs::hard_link(from, to).is_ok() {
        return Some(Linked::HardLink);
    }
    if !allow_copy {
        return None;
    }
    let tmp = to.with_extension("apitorrent-copy");
    let copied = std::fs::copy(from, &tmp).is_ok() && std::fs::rename(&tmp, to).is_ok();
    if copied {
        Some(Linked::Copy)
    } else {
        let _ = std::fs::remove_file(&tmp);
        None
    }
}

/// The sources every one of whose files was matched: the new torrent supersedes them.
pub fn superseded(matches: &[Match], sources: &[Source]) -> Vec<String> {
    let mut used: HashMap<&str, HashSet<usize>> = HashMap::new();
    for m in matches {
        used.entry(m.source_id.as_str())
            .or_default()
            .insert(m.source_file);
    }
    sources
        .iter()
        .filter(|s| {
            let files: HashSet<usize> = s
                .torrent
                .info
                .files
                .iter()
                .enumerate()
                .filter(|(_, f)| !f.is_pad() && f.length > 0)
                .map(|(i, _)| i)
                .collect();
            !files.is_empty() && used.get(s.id.as_str()).is_some_and(|u| files.is_subset(u))
        })
        .map(|s| s.id.clone())
        .collect()
}

/// Whether a tracker's error says the torrent is gone from it. Trackers word this
/// differently; these cover the common ones. Tracker-adapter plugins will be able to
/// decide instead.
pub fn is_unregistered(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    [
        "unregistered",
        "not registered",
        "torrent not found",
        "unknown torrent",
        "torrent does not exist",
        "info hash not found",
        "infohash not found",
        "torrent has been deleted",
        "trumped",
    ]
    .iter()
    .any(|p| m.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use apitorrent_metainfo::{FileSource, TorrentBuilder, Version};

    fn data(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| {
                u8::try_from(i % 256)
                    .unwrap()
                    .wrapping_mul(31)
                    .wrapping_add(seed)
            })
            .collect()
    }

    fn build(name: &str, version: Version, files: &[(&str, &[u8])]) -> Arc<Torrent> {
        let mut b = TorrentBuilder::new(name, version).piece_length(16 * 1024);
        for (path, bytes) in files {
            b = b.file(FileSource::from_bytes(&[path], bytes.to_vec()));
        }
        Arc::new(Torrent::from_bytes(&b.build().unwrap()).unwrap())
    }

    fn source(id: &str, t: &Arc<Torrent>, dir: &Path) -> Source {
        let paths = t
            .info
            .files
            .iter()
            .map(|f| {
                (!f.is_pad()).then(|| {
                    let mut p = dir.to_path_buf();
                    for c in &f.path {
                        p.push(c);
                    }
                    p
                })
            })
            .collect::<Vec<_>>();
        for p in paths.iter().flatten() {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        }
        Source {
            id: id.into(),
            torrent: t.clone(),
            paths,
            have: Bitfield::full(u32::try_from(t.info.piece_count()).unwrap()),
        }
    }

    #[test]
    fn v1_single_episodes_match_a_pack() {
        let dir = tempfile::tempdir().unwrap();
        let e1 = data(100_000, 1);
        let e2 = data(70_000, 2);
        let s1 = build("e1.mkv", Version::V1, &[("e1.mkv", &e1)]);
        let s2 = build("e2.mkv", Version::V1, &[("e2.mkv", &e2)]);
        let pack = build("Season 1", Version::V1, &[("e1.mkv", &e1), ("e2.mkv", &e2)]);
        let src1 = source("s1", &s1, &dir.path().join("a"));
        let src2 = source("s2", &s2, &dir.path().join("b"));
        std::fs::write(src1.paths[0].as_ref().unwrap(), &e1).unwrap();
        std::fs::write(src2.paths[0].as_ref().unwrap(), &e2).unwrap();
        let target_dir = dir.path().join("pack");
        let targets: Vec<PathBuf> = pack
            .info
            .files
            .iter()
            .map(|f| f.path.iter().fold(target_dir.clone(), |p, c| p.join(c)))
            .collect();
        let matches = find_matches(&pack, &targets, &[4, 4], &[src1.clone(), src2.clone()]);
        assert_eq!(matches.len(), 2, "{matches:?}");
        assert_eq!(superseded(&matches, &[src1, src2]), ["s1", "s2"]);
    }

    #[test]
    fn a_different_file_of_the_same_length_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let real = data(100_000, 1);
        let other = data(100_000, 9);
        let s = build("x.bin", Version::V1, &[("x.bin", &other)]);
        let src = source("s", &s, dir.path());
        std::fs::write(src.paths[0].as_ref().unwrap(), &other).unwrap();
        let new = build("x.bin", Version::V1, &[("x.bin", &real)]);
        let targets = vec![dir.path().join("new").join("x.bin")];
        assert!(find_matches(&new, &targets, &[4], &[src]).is_empty());
    }

    #[test]
    fn v2_matches_by_root_alone() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = data(50_000, 3);
        let s = build("renamed.bin", Version::V2, &[("renamed.bin", &bytes)]);
        let src = source("s", &s, dir.path());
        std::fs::write(src.paths[0].as_ref().unwrap(), &bytes).unwrap();
        let new = build("original.bin", Version::V2, &[("original.bin", &bytes)]);
        let targets = vec![dir.path().join("new").join("original.bin")];
        let m = find_matches(&new, &targets, &[4], &[src]);
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn incomplete_sources_and_skipped_files_are_not_used() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = data(100_000, 4);
        let s = build("x.bin", Version::V1, &[("x.bin", &bytes)]);
        let mut src = source("s", &s, dir.path());
        std::fs::write(src.paths[0].as_ref().unwrap(), &bytes).unwrap();
        let new = build("x.bin", Version::V1, &[("x.bin", &bytes)]);
        let targets = vec![dir.path().join("new").join("x.bin")];
        assert!(find_matches(&new, &targets, &[0], std::slice::from_ref(&src)).is_empty());
        src.have.clear(2);
        assert!(find_matches(&new, &targets, &[4], &[src]).is_empty());
    }

    #[test]
    fn hard_links_share_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("a.bin");
        std::fs::write(&from, b"hello").unwrap();
        let to = dir.path().join("sub").join("b.bin");
        assert_eq!(materialize(&from, &to, false), Some(Linked::HardLink));
        assert_eq!(std::fs::read(&to).unwrap(), b"hello");
    }

    #[test]
    fn unregistered_messages() {
        assert!(is_unregistered("Unregistered torrent"));
        assert!(is_unregistered("Torrent not registered with this tracker"));
        assert!(is_unregistered("torrent not found"));
        assert!(!is_unregistered("connection timed out"));
    }
}

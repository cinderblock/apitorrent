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
//! - **Otherwise**: the same length, preferring the same name. Up to [`SAMPLES`] pieces
//!   that lie wholly inside the file are read from the candidate at the new torrent's
//!   alignment and checked against its hashes (v1 or v2). All passing is a full match,
//!   which is hard-linked. Some passing is a **partial** match (a repack that changed one
//!   part, say): the file is cloned or copied, never linked, and the check after that
//!   keeps the pieces that verify, so only the rest is downloaded. Files too small to hold
//!   a whole piece need a matching name; the check after linking settles it.
//!
//! See `docs/architecture.md`, "Content library".

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use apitorrent_engine::bitfield::Bitfield;
use apitorrent_engine::layout::Layout;
use apitorrent_engine::verify::{PieceHashes, Verdict};
use apitorrent_metainfo::Torrent;

/// How many pieces of a candidate are checked before trusting it.
pub const SAMPLES: usize = 8;
/// How many same-length candidates are read for one file, same-named ones first.
pub const MAX_CANDIDATES: usize = 32;

/// A torrent whose files may be reused.
#[derive(Debug, Clone)]
pub struct Source {
    pub id: String,
    pub torrent: Arc<Torrent>,
    /// Where each file is on disk, or `None` where it isn't in the save path (deselected).
    pub paths: Vec<Option<PathBuf>>,
    pub have: Bitfield,
    /// Fingerprints of the files (see `crate::fingerprint`), giving a `pieces root` to
    /// files whose torrent has none. Empty when there are none. One is used only if the
    /// file still has the size and time it was fingerprinted at.
    pub fingerprints: Vec<Option<crate::fingerprint::Fingerprint>>,
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
    /// Every checked piece passed (or the roots matched), so it is hard-linked. A partial
    /// match is cloned or copied instead. Either way, only the new torrent's own check
    /// proves the bytes (see [`supersede_candidates`]).
    pub full: bool,
    pub by: MatchedBy,
}

/// What a match rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchedBy {
    /// The torrents' `pieces root`s agree. No I/O.
    Root,
    /// The new torrent's root agrees with a fingerprint of the file. No I/O.
    Fingerprint,
    /// Pieces read from the file checked out against the new torrent's hashes.
    Pieces,
    /// The same name and length, and no whole piece to check. The check after linking
    /// settles it.
    Name,
}

/// How a match was put in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Linked {
    /// Shares the source's file: copy-on-write in the engine.
    HardLink,
    /// A copy-on-write clone (Btrfs, XFS, APFS, ReFS): a private file sharing extents.
    Reflink,
    /// A private copy.
    Copy,
}

impl Linked {
    /// Whether the file is shared with the source, so writes must not go through it.
    pub fn shared(self) -> bool {
        matches!(self, Self::HardLink)
    }
}

/// What checking a candidate's pieces found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fit {
    /// Every checked piece passed.
    Full,
    /// Some passed: `passed` of `checked`.
    Partial { passed: usize, checked: usize },
    /// None passed.
    Different,
    /// No piece could be checked (the file holds no whole piece).
    Unknown,
}

/// Find existing complete copies of the new torrent's files. `targets` is where each file
/// of the new torrent would go; files already there, and files with priority 0, are left
/// alone. With `partial`, files that match only in places are returned too (see
/// [`Match::full`]).
pub fn find_matches(
    new: &Torrent,
    targets: &[PathBuf],
    priorities: &[u8],
    sources: &[Source],
    partial: bool,
) -> Vec<Match> {
    let layout = Layout::from_info(&new.info);
    let hashes = PieceHashes::new(new);
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
                by_root.entry((root, f.length)).or_insert((si, fi, false));
            } else if let Some(fp) = s.fingerprints.get(fi).copied().flatten()
                && fp.size == f.length
            {
                by_root
                    .entry((fp.pieces_root, f.length))
                    .or_insert((si, fi, true));
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
            .filter(|&(si, fi, from_fingerprint)| {
                // A fingerprint is only good while the file is unchanged since.
                !from_fingerprint
                    || sources[si].fingerprints[fi].is_some_and(|fp| {
                        fp.current(sources[si].paths[fi].as_ref().expect("indexed with a path"))
                    })
            })
            .map(|(si, fi, fp)| {
                let by = if fp {
                    MatchedBy::Fingerprint
                } else {
                    MatchedBy::Root
                };
                ((si, fi), true, by)
            })
            .or_else(|| {
                let name = f.path.last();
                let mut cands = by_length.get(&f.length).cloned().unwrap_or_default();
                // Same name first.
                cands.sort_by_key(|&(si, fi)| {
                    sources[si].torrent.info.files[fi].path.last() != name
                });
                let mut best: Option<((usize, usize), f64)> = None;
                // Each candidate costs reads, so a length shared by many files (a library of
                // same-size samples, say) is capped. Roots and fingerprints aren't.
                for (si, fi) in cands.into_iter().take(MAX_CANDIDATES) {
                    let same_name = sources[si].torrent.info.files[fi].path.last() == name;
                    let from = sources[si].paths[fi]
                        .as_ref()
                        .expect("indexed only with a path");
                    match check_pieces(&hashes, &layout, f.offset, f.length, i, from) {
                        Fit::Full => return Some(((si, fi), true, MatchedBy::Pieces)),
                        Fit::Unknown if same_name => {
                            return Some(((si, fi), true, MatchedBy::Name));
                        }
                        Fit::Partial { passed, checked } if partial => {
                            #[allow(clippy::cast_precision_loss)]
                            let score = passed as f64 / checked as f64;
                            if best.is_none_or(|(_, b)| score > b) {
                                best = Some(((si, fi), score));
                            }
                        }
                        _ => {}
                    }
                }
                best.map(|(c, _)| (c, false, MatchedBy::Pieces))
            });
        if let Some(((si, fi), full, by)) = found {
            out.push(Match {
                file: i,
                source_id: sources[si].id.clone(),
                source_file: fi,
                from: sources[si].paths[fi]
                    .clone()
                    .expect("indexed only with a path"),
                full,
                by,
            });
        }
    }
    out
}

/// Check up to [`SAMPLES`] pieces of the new torrent that lie wholly inside its file `i`
/// (at `offset`, `length` bytes), reading them from the candidate, spread evenly from the
/// first such piece to the last.
fn check_pieces(
    hashes: &PieceHashes,
    layout: &Layout,
    offset: u64,
    length: u64,
    i: usize,
    candidate: &Path,
) -> Fit {
    let inside: Vec<u32> = layout
        .file_pieces(i)
        .filter(|&p| {
            let start = layout.piece_offset(p);
            start >= offset && start + u64::from(layout.piece_size(p)) <= offset + length
        })
        .collect();
    if inside.is_empty() {
        return Fit::Unknown;
    }
    let picks: Vec<u32> = if inside.len() <= SAMPLES {
        inside
    } else {
        (0..SAMPLES)
            .map(|k| inside[k * (inside.len() - 1) / (SAMPLES - 1)])
            .collect()
    };
    let Ok(mut file) = std::fs::File::open(candidate) else {
        return Fit::Different;
    };
    let (mut passed, mut checked) = (0, 0);
    for p in picks {
        let mut buf = vec![0u8; layout.piece_size(p) as usize];
        if read_at(&mut file, layout.piece_offset(p) - offset, &mut buf).is_err() {
            return Fit::Different;
        }
        match hashes.verify(p, &buf) {
            Verdict::Valid => {
                passed += 1;
                checked += 1;
            }
            Verdict::Invalid => checked += 1,
            // A v2 piece whose layer isn't here yet tells us nothing.
            Verdict::Unknown => {}
        }
    }
    match (passed, checked) {
        (_, 0) => Fit::Unknown,
        (p, c) if p == c => Fit::Full,
        (0, _) => Fit::Different,
        (passed, checked) => Fit::Partial { passed, checked },
    }
}

fn read_at(f: &mut std::fs::File, at: u64, buf: &mut [u8]) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    f.seek(SeekFrom::Start(at))?;
    f.read_exact(buf)
}

/// Put a match in place. A full match is hard-linked when the volume allows; a partial one
/// never is, since its bytes will change. Otherwise a copy-on-write clone, and failing
/// that a copy (when `allow_copy`). Returns `None` if nothing worked.
pub fn materialize(from: &Path, to: &Path, full: bool, allow_copy: bool) -> Option<Linked> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir).ok()?;
    }
    if full && std::fs::hard_link(from, to).is_ok() {
        return Some(Linked::HardLink);
    }
    // Clone or copy beside the target, then rename, so a half-made file never sits at the
    // target path.
    let tmp = to.with_extension("apitorrent-copy");
    let _ = std::fs::remove_file(&tmp);
    let how = if reflink_copy::reflink(from, &tmp).is_ok() {
        Linked::Reflink
    } else if allow_copy && std::fs::copy(from, &tmp).is_ok() {
        Linked::Copy
    } else {
        let _ = std::fs::remove_file(&tmp);
        return None;
    };
    if std::fs::rename(&tmp, to).is_ok() {
        Some(how)
    } else {
        let _ = std::fs::remove_file(&tmp);
        None
    }
}

/// A source every one of whose files was matched, and the new torrent's files that took
/// them over. The new torrent supersedes it once its check proves those files
/// ([`files_verified`]): sampling a few pieces can miss a changed region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupersedeCandidate {
    pub source_id: String,
    pub files: Vec<usize>,
}

/// The sources every one of whose files was matched (see [`SupersedeCandidate`]).
pub fn supersede_candidates(matches: &[Match], sources: &[Source]) -> Vec<SupersedeCandidate> {
    let mut used: HashMap<&str, HashMap<usize, usize>> = HashMap::new();
    for m in matches {
        used.entry(m.source_id.as_str())
            .or_default()
            .insert(m.source_file, m.file);
    }
    sources
        .iter()
        .filter_map(|s| {
            let files: HashSet<usize> = s
                .torrent
                .info
                .files
                .iter()
                .enumerate()
                .filter(|(_, f)| !f.is_pad() && f.length > 0)
                .map(|(i, _)| i)
                .collect();
            let u = used.get(s.id.as_str())?;
            (!files.is_empty() && files.iter().all(|f| u.contains_key(f))).then(|| {
                let mut ours: Vec<usize> = u.values().copied().collect();
                ours.sort_unstable();
                SupersedeCandidate {
                    source_id: s.id.clone(),
                    files: ours,
                }
            })
        })
        .collect()
}

/// Whether the check proved files `files` of a torrent: every piece wholly inside each one
/// is had. Pieces shared with a neighbouring file aren't required, since that neighbour
/// may still be downloading (v2 files share no pieces, so for them this is every piece).
/// A file too small to hold a whole piece needs all the pieces it touches.
pub fn files_verified(layout: &Layout, have: &Bitfield, files: &[usize]) -> bool {
    files.iter().all(|&i| {
        let span = &layout.files()[i];
        let (start, end) = (span.offset, span.offset + span.length);
        let pieces: Vec<u32> = layout.file_pieces(i).collect();
        let inside: Vec<u32> = pieces
            .iter()
            .copied()
            .filter(|&p| {
                let at = layout.piece_offset(p);
                at >= start && at + u64::from(layout.piece_size(p)) <= end
            })
            .collect();
        let needed = if inside.is_empty() { pieces } else { inside };
        needed.iter().all(|&p| have.get(p))
    })
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
            fingerprints: Vec::new(),
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
        let matches = find_matches(
            &pack,
            &targets,
            &[4, 4],
            &[src1.clone(), src2.clone()],
            true,
        );
        assert_eq!(matches.len(), 2, "{matches:?}");
        let ids: Vec<String> = supersede_candidates(&matches, &[src1, src2])
            .into_iter()
            .map(|c| c.source_id)
            .collect();
        assert_eq!(ids, ["s1", "s2"]);
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
        assert!(find_matches(&new, &targets, &[4], &[src], true).is_empty());
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
        let m = find_matches(&new, &targets, &[4], &[src], true);
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
        assert!(find_matches(&new, &targets, &[0], std::slice::from_ref(&src), true).is_empty());
        src.have.clear(2);
        assert!(find_matches(&new, &targets, &[4], &[src], true).is_empty());
    }

    /// A repack that changed one region: the file matches only in places, so it is
    /// offered as a partial match, which never supersedes the source.
    fn repack_case(version: Version) {
        let dir = tempfile::tempdir().unwrap();
        let old = data(16 * 1024 * 20, 5);
        let mut new_bytes = old.clone();
        // Change one piece in the middle, one the sampling checks (of 20 pieces it reads
        // 0, 2, 5, 8, 10, 13, 16 and 19).
        for b in &mut new_bytes[16 * 1024 * 10..16 * 1024 * 11] {
            *b = b.wrapping_add(1);
        }
        let s = build("show.mkv", version, &[("show.mkv", &old)]);
        let src = source("s", &s, dir.path());
        std::fs::write(src.paths[0].as_ref().unwrap(), &old).unwrap();
        let new = build("show.mkv", version, &[("show.mkv", &new_bytes)]);
        let targets = vec![dir.path().join("new").join("show.mkv")];

        let m = find_matches(&new, &targets, &[4], std::slice::from_ref(&src), true);
        assert_eq!(m.len(), 1, "{version:?}");
        assert!(!m[0].full, "{version:?}");
        assert!(
            find_matches(&new, &targets, &[4], std::slice::from_ref(&src), false).is_empty(),
            "partial reuse can be turned off"
        );

        // It is cloned or copied, never linked, so changing it leaves the source alone.
        let how = materialize(&m[0].from, &targets[0], false, true).unwrap();
        assert!(!how.shared(), "{how:?}");
        std::fs::write(&targets[0], &new_bytes).unwrap();
        assert_eq!(std::fs::read(src.paths[0].as_ref().unwrap()).unwrap(), old);
    }

    #[test]
    fn a_repack_is_a_partial_match_v1() {
        repack_case(Version::V1);
    }

    #[test]
    fn a_repack_is_a_partial_match_v2() {
        // The roots differ, so v2 pieces are checked against the piece layers.
        repack_case(Version::V2);
    }

    /// A v2 torrent of a file a v1 torrent has, under another name: a fingerprint finds it
    /// without reading the file, and a stale one isn't trusted.
    #[test]
    fn a_fingerprint_lets_a_v2_torrent_match_a_v1_file() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = data(10_000, 6);
        let s = build("old-name.bin", Version::V1, &[("old-name.bin", &bytes)]);
        let mut src = source("s", &s, dir.path());
        std::fs::write(src.paths[0].as_ref().unwrap(), &bytes).unwrap();
        let new = build("new-name.bin", Version::V2, &[("new-name.bin", &bytes)]);
        let targets = vec![dir.path().join("new").join("new-name.bin")];
        // Without a fingerprint the file is found by reading it.
        let m = find_matches(&new, &targets, &[4], std::slice::from_ref(&src), true);
        assert_eq!(m[0].by, MatchedBy::Pieces);
        let fp = crate::fingerprint::compute(
            src.paths[0].as_ref().unwrap(),
            0,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        src.fingerprints = vec![Some(fp)];
        let m = find_matches(&new, &targets, &[4], std::slice::from_ref(&src), true);
        assert_eq!(m.len(), 1);
        assert!(m[0].full);
        assert_eq!(
            m[0].by,
            MatchedBy::Fingerprint,
            "with one, without reading it"
        );

        // Once the file changes, the fingerprint is stale and not trusted.
        let mut changed = bytes.clone();
        changed[0] ^= 1;
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(src.paths[0].as_ref().unwrap(), &changed).unwrap();
        assert!(find_matches(&new, &targets, &[4], &[src], true).is_empty());
    }

    #[test]
    fn hard_links_share_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("a.bin");
        std::fs::write(&from, b"hello").unwrap();
        let to = dir.path().join("sub").join("b.bin");
        assert_eq!(materialize(&from, &to, true, false), Some(Linked::HardLink));
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

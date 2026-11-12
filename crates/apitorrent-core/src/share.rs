//! Sharing data between running torrents: the content library's live half.
//!
//! `library` matches a new torrent's files once, when it is added, against complete files
//! of the torrents already here. This module keeps looking while torrents run:
//!
//! - **Re-matching.** When a file a torrent still needs is complete in another torrent (it
//!   finished downloading there, or got a fingerprint), and the needing torrent has no piece
//!   touching it yet, the file is adopted as at add time: hard-linked for a full match,
//!   cloned or copied for a partial one. The engine then re-hashes the pieces touching it
//!   ([`apitorrent_engine::TorrentHandle::adopt_file`]). A file the needing torrent already
//!   has verified pieces of is never replaced; piece sharing fills it instead.
//! - **Piece sharing.** A file of one torrent believed to hold the same bytes as a file of
//!   another is a *pair*: the same length and `pieces root` (from the torrents, or from a
//!   fingerprint) is trusted; for v1, the same length is a *candidate*, confirmed by the
//!   first piece that verifies and dropped if that piece fails. Whenever the source
//!   verifies a piece, each piece of the needing torrent whose bytes are now all verified
//!   in sources is read from their files and offered to the needing torrent
//!   ([`apitorrent_engine::TorrentHandle::offer_piece`]), which checks it against its own
//!   hash before writing it. Different piece sizes and alignments don't matter: pieces
//!   are mapped through byte ranges of the files.
//! - **Provenance.** Every byte supplied this way (and at add time, and in bookend pieces)
//!   is recorded per file, with its source and method ([`ReuseBook`], table `file_reuse`).
//!
//! It is bounded: one piece is read at a time, off the async runtime, at no more than
//! `library.share_rate` bytes per second. Each step looks torrents up again by id, so
//! removing one mid-way only drops its work. Nothing unverified is ever written: a wrong
//! guess costs one read and one hash.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use apitorrent_engine::bitfield::Bitfield;
use apitorrent_engine::layout::Layout;
use apitorrent_engine::{
    AdoptMode, AdoptRefused, Adopted, Offered, PieceEvent, PieceOrigin, TorrentHandle,
    TorrentState as EngineState,
};
use apitorrent_metainfo::Torrent;
use apitorrent_types::{FileReuse, LibrarySettings, ReuseMethod};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};

use crate::Core;
use crate::db::ReuseRow;
use crate::library::{self, Linked, SampleFit};

/// How often the loop wakes without events.
const TICK: Duration = Duration::from_millis(500);
/// Looking for new pairs costs a round trip to each candidate torrent's actor, so it runs
/// at most this often while pieces keep arriving…
const DISCOVER_MIN: Duration = Duration::from_secs(2);
/// …and at least this often regardless.
const DISCOVER_MAX: Duration = Duration::from_secs(15);
/// Same-length candidates per needed file, same-named first.
const MAX_CANDIDATES: usize = 8;
/// A trusted or confirmed pair is dropped after this many pieces fail.
const MAX_FAILURES: u32 = 4;
/// How long one turn of the loop spends offering pieces before looking at events again.
const SLICE: Duration = Duration::from_secs(1);

// ----- provenance -----------------------------------------------------------------------

/// A file linked at add time, recorded once the torrent's check says how much of it held.
#[derive(Debug, Clone)]
pub(crate) struct PendingReuse {
    pub file: usize,
    pub source_id: String,
    pub source_name: String,
    pub how: Linked,
}

/// Where the content library supplied each file's bytes from, by torrent id and file
/// index, mirrored in the database (`file_reuse`).
#[derive(Debug, Default)]
pub(crate) struct ReuseBook {
    files: RwLock<HashMap<String, BTreeMap<usize, FileReuse>>>,
    /// Add-time matches waiting for the torrent's check (see [`Core::settle_reuse`]).
    pending: Mutex<HashMap<String, Vec<PendingReuse>>>,
    /// Rows not yet in the database.
    unsaved: Mutex<Vec<ReuseRow>>,
}

fn method_to_byte(m: ReuseMethod) -> u8 {
    match m {
        ReuseMethod::HardLink => 0,
        ReuseMethod::Clone => 1,
        ReuseMethod::Copy => 2,
        ReuseMethod::PieceSharing => 3,
    }
}

fn method_from_byte(b: u8) -> ReuseMethod {
    match b {
        0 => ReuseMethod::HardLink,
        1 => ReuseMethod::Clone,
        2 => ReuseMethod::Copy,
        _ => ReuseMethod::PieceSharing,
    }
}

impl ReuseBook {
    pub(crate) fn load(&self, rows: Vec<ReuseRow>) {
        let mut files = self.files.write().expect("reuse lock poisoned");
        for r in rows {
            files.entry(r.torrent).or_default().insert(
                r.file,
                FileReuse {
                    source_id: r.source,
                    source_name: r.source_name,
                    method: method_from_byte(r.method),
                    bytes: r.bytes,
                },
            );
        }
    }

    /// Count `bytes` of a torrent's file as reused. A file already recorded keeps its
    /// source and method.
    pub(crate) fn add(
        &self,
        torrent: &str,
        file: usize,
        source: (&str, &str),
        method: ReuseMethod,
        bytes: u64,
    ) {
        if bytes == 0 {
            return;
        }
        self.files
            .write()
            .expect("reuse lock poisoned")
            .entry(torrent.to_owned())
            .or_default()
            .entry(file)
            .and_modify(|r| r.bytes += bytes)
            .or_insert_with(|| FileReuse {
                source_id: source.0.to_owned(),
                source_name: source.1.to_owned(),
                method,
                bytes,
            });
        self.unsaved
            .lock()
            .expect("reuse lock poisoned")
            .push(ReuseRow {
                torrent: torrent.to_owned(),
                file,
                source: source.0.to_owned(),
                source_name: source.1.to_owned(),
                method: method_to_byte(method),
                bytes,
            });
    }

    /// One torrent's records, by file.
    pub(crate) fn of(&self, torrent: &str) -> BTreeMap<usize, FileReuse> {
        self.files
            .read()
            .expect("reuse lock poisoned")
            .get(torrent)
            .cloned()
            .unwrap_or_default()
    }

    /// Bytes reused across every torrent here.
    pub(crate) fn total(&self) -> u64 {
        self.files
            .read()
            .expect("reuse lock poisoned")
            .values()
            .flat_map(BTreeMap::values)
            .map(|r| r.bytes)
            .sum()
    }

    /// The files of a torrent's add-time matches still waiting for its check.
    pub(crate) fn pending_files(&self, torrent: &str) -> Vec<usize> {
        self.pending
            .lock()
            .expect("reuse lock poisoned")
            .get(torrent)
            .map(|f| f.iter().map(|p| p.file).collect())
            .unwrap_or_default()
    }

    pub(crate) fn set_pending(&self, torrent: &str, files: Vec<PendingReuse>) {
        if !files.is_empty() {
            self.pending
                .lock()
                .expect("reuse lock poisoned")
                .insert(torrent.to_owned(), files);
        }
    }

    /// Forget a torrent (its last entry was removed).
    pub(crate) fn forget(&self, torrent: &str) {
        self.files
            .write()
            .expect("reuse lock poisoned")
            .remove(torrent);
        self.pending
            .lock()
            .expect("reuse lock poisoned")
            .remove(torrent);
        self.unsaved
            .lock()
            .expect("reuse lock poisoned")
            .retain(|r| r.torrent != torrent);
    }
}

/// How a file was put in place, as recorded.
pub(crate) fn linked_method(how: Linked) -> ReuseMethod {
    match how {
        Linked::HardLink => ReuseMethod::HardLink,
        Linked::Reflink => ReuseMethod::Clone,
        Linked::Copy => ReuseMethod::Copy,
    }
}

fn adopted_method(how: Adopted) -> ReuseMethod {
    match how {
        Adopted::HardLink => ReuseMethod::HardLink,
        Adopted::Reflink => ReuseMethod::Clone,
        Adopted::Copy => ReuseMethod::Copy,
    }
}

/// Bytes of file `file` inside `pieces`.
fn file_bytes_in(layout: &Layout, file: usize, pieces: impl IntoIterator<Item = u32>) -> u64 {
    let span = &layout.files()[file];
    let (start, end) = (span.offset, span.offset + span.length);
    pieces
        .into_iter()
        .map(|p| {
            let at = layout.piece_offset(p);
            let to = at + u64::from(layout.piece_size(p));
            to.min(end).saturating_sub(at.max(start))
        })
        .sum()
}

// ----- live sharing ---------------------------------------------------------------------

/// A torrent's file, by torrent id and file index.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FileRef {
    id: String,
    file: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trust {
    /// The same `pieces root` (from the torrents or a fingerprint).
    Root,
    /// The same length only; not yet tested.
    Candidate,
    /// A piece read from the source verified in the needing torrent.
    Confirmed,
}

#[derive(Debug, Clone)]
struct Pair {
    need: FileRef,
    src: FileRef,
    trust: Trust,
    failures: u32,
}

/// What the loop knows of a torrent that needs or supplies files.
struct Known {
    torrent: Arc<Torrent>,
    layout: Layout,
    paths: Vec<Option<PathBuf>>,
    have: Bitfield,
}

/// One piece to read from sources and offer.
struct Job {
    need: String,
    piece: u32,
    /// `(path, offset in file, offset in piece, length)`; the gaps are pad bytes (zeros).
    reads: Vec<(PathBuf, u64, usize, usize)>,
    /// The pairs supplying it, by index into the pair list when planned (re-found by key).
    pairs: Vec<(FileRef, FileRef)>,
}

#[derive(Default)]
struct State {
    known: HashMap<String, Known>,
    pairs: Vec<Pair>,
    /// Pairs found not to hold the same bytes.
    rejected: HashSet<(FileRef, FileRef)>,
    /// Adoptions already tried, so a refused one isn't read again every pass.
    adopt_tried: HashSet<(FileRef, FileRef)>,
    /// Pieces verified since the last plan, by torrent id.
    dirty: HashMap<String, Vec<u32>>,
    /// Pairs whose needing file should be scanned whole (new pairs).
    rescan: HashSet<(FileRef, FileRef)>,
    queue: VecDeque<Job>,
    queued: HashSet<(String, u32)>,
    /// Something changed that may make new pairs.
    changed: bool,
    torrents_seen: usize,
    /// Piece events were missed: every pair's file is scanned again.
    lagged: bool,
}

impl State {
    fn clear_work(&mut self) {
        self.pairs.clear();
        self.queue.clear();
        self.queued.clear();
        self.dirty.clear();
        self.rescan.clear();
    }
}

impl Core {
    /// Runs live sharing until shutdown (see the module docs).
    pub(crate) async fn sharer(self) {
        let mut events = self.inner.session.piece_events();
        // Restored torrents: their reused files make bookend pieces again.
        let reused: Vec<(String, Vec<usize>)> = self
            .inner
            .reuse
            .files
            .read()
            .expect("reuse lock poisoned")
            .iter()
            .map(|(id, files)| (id.clone(), files.keys().copied().collect()))
            .collect();
        for (id, files) in reused {
            if let Some(handle) = self.swarm_handle(&id) {
                handle.set_reused(files).await;
            }
        }
        let mut st = State {
            changed: true,
            ..State::default()
        };
        let mut tick = tokio::time::interval(TICK);
        let mut discovered: Option<Instant> = None;
        loop {
            tokio::select! {
                ev = events.recv() => match ev {
                    Ok(e) => self.note(&mut st, e),
                    Err(RecvError::Lagged(_)) => {
                        st.changed = true;
                        st.lagged = true;
                    }
                    Err(RecvError::Closed) => return,
                },
                _ = tick.tick() => {}
                () = self.inner.stop.notified() => return,
            }
            loop {
                match events.try_recv() {
                    Ok(e) => self.note(&mut st, e),
                    Err(TryRecvError::Lagged(_)) => {
                        st.changed = true;
                        st.lagged = true;
                    }
                    Err(_) => break,
                }
            }
            if self
                .inner
                .stopping
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                return;
            }
            let lib = self.settings().library;
            if !lib.enabled || lib.share_rate == 0 {
                st.clear_work();
                continue;
            }
            let count = self
                .inner
                .torrents
                .read()
                .expect("torrents lock poisoned")
                .len();
            if count != st.torrents_seen {
                st.torrents_seen = count;
                st.changed = true;
            }
            let due = discovered.is_none_or(|t| {
                let age = t.elapsed();
                age >= DISCOVER_MAX || (st.changed && age >= DISCOVER_MIN)
            });
            if due {
                st.changed = false;
                self.discover(&mut st, lib).await;
                discovered = Some(Instant::now());
            }
            plan(&mut st);
            self.run_jobs(&mut st, lib).await;
            self.flush_reuse().await;
        }
    }

    /// Take note of a verified piece.
    fn note(&self, st: &mut State, e: PieceEvent) {
        let id = e.torrent.id_string();
        if let Some(k) = st.known.get_mut(&id) {
            k.have.set(e.piece);
        }
        if let PieceOrigin::Bookend { from_disk } = e.origin {
            self.record_bookend(&id, e.piece, from_disk);
        }
        st.dirty.entry(id).or_default().push(e.piece);
        st.changed = true;
    }

    /// Credit a bookend piece's library bytes to the reused file it straddles.
    fn record_bookend(&self, id: &str, piece: u32, from_disk: u32) {
        let torrent = self
            .inner
            .torrents
            .read()
            .expect("torrents lock poisoned")
            .iter()
            .find(|(k, _)| k.id == id)
            .map(|(_, e)| e.torrent.clone());
        let Some(torrent) = torrent else { return };
        let layout = Layout::from_info(&torrent.info);
        let segments = layout.segments(
            layout.piece_offset(piece),
            layout.piece_size(piece) as usize,
        );
        let book = &self.inner.reuse;
        let recorded = book.of(id);
        // A file linked at add time may not be recorded yet (its check just ended).
        let pending = book
            .pending
            .lock()
            .expect("reuse lock poisoned")
            .get(id)
            .cloned()
            .unwrap_or_default();
        let source = segments.iter().filter(|s| !s.pad).find_map(|s| {
            recorded
                .get(&s.file)
                .map(|r| (s.file, r.source_id.clone(), r.source_name.clone(), r.method))
                .or_else(|| {
                    pending.iter().find(|p| p.file == s.file).map(|p| {
                        (
                            s.file,
                            p.source_id.clone(),
                            p.source_name.clone(),
                            linked_method(p.how),
                        )
                    })
                })
        });
        if let Some((file, source_id, source_name, method)) = source {
            book.add(
                id,
                file,
                (&source_id, &source_name),
                method,
                u64::from(from_disk),
            );
        }
    }

    /// The engine handle of a torrent's swarm.
    fn swarm_handle(&self, id: &str) -> Option<TorrentHandle> {
        self.inner
            .swarms
            .lock()
            .expect("swarms lock poisoned")
            .get(id)
            .map(|s| s.handle.clone())
    }

    /// Torrents still downloading, with the highest priority any of their entries gives
    /// each file.
    fn needy(&self) -> HashMap<String, (Arc<Torrent>, Vec<u8>)> {
        let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
        let mut out: HashMap<String, (Arc<Torrent>, Vec<u8>)> = HashMap::new();
        for (k, e) in torrents.iter() {
            let stats = e.handle.stats();
            if stats.finished || stats.state != EngineState::Downloading {
                continue;
            }
            let entry = out
                .entry(k.id.clone())
                .or_insert_with(|| (e.torrent.clone(), vec![0; e.torrent.info.files.len()]));
            for (m, &p) in entry.1.iter_mut().zip(&e.file_priorities) {
                *m = (*m).max(p);
            }
        }
        out
    }

    /// Find pairs, and adopt complete files into torrents that have none of them yet.
    async fn discover(&self, st: &mut State, lib: LibrarySettings) {
        let needy = self.needy();
        if needy.is_empty() {
            st.clear_work();
            st.known.clear();
            return;
        }
        let lengths: HashSet<u64> = needy
            .values()
            .flat_map(|(t, prios)| {
                t.info
                    .files
                    .iter()
                    .zip(prios)
                    .filter(|(f, p)| **p > 0 && !f.is_pad() && f.length > 0)
                    .map(|(f, _)| f.length)
            })
            .collect();
        let sources = self
            .library_sources(None, |t| {
                t.info.files.iter().any(|f| lengths.contains(&f.length))
            })
            .await;
        st.known = sources
            .iter()
            .map(|s| {
                (
                    s.id.clone(),
                    Known {
                        torrent: s.torrent.clone(),
                        layout: Layout::from_info(&s.torrent.info),
                        paths: s.paths.clone(),
                        have: s.have.clone(),
                    },
                )
            })
            .collect();
        // Files by length, for candidates.
        let mut by_length: HashMap<u64, Vec<(usize, usize)>> = HashMap::new();
        for (si, s) in sources.iter().enumerate() {
            for (fi, f) in s.torrent.info.files.iter().enumerate() {
                if !f.is_pad() && lengths.contains(&f.length) && s.paths[fi].is_some() {
                    by_length.entry(f.length).or_default().push((si, fi));
                }
            }
        }
        let mut adoptions = Vec::new();
        let mut keep: HashSet<(FileRef, FileRef)> = HashSet::new();
        for b in &sources {
            let Some((_, prios)) = needy.get(&b.id) else {
                continue;
            };
            let lb = &st.known[&b.id].layout;
            for (fb, f) in b.torrent.info.files.iter().enumerate() {
                if f.is_pad() || f.length == 0 || prios.get(fb).copied().unwrap_or(0) == 0 {
                    continue;
                }
                let touched: Vec<u32> = lb.file_pieces(fb).collect();
                if touched.iter().all(|&p| b.have.get(p)) {
                    continue;
                }
                let untouched = touched.iter().all(|&p| !b.have.get(p));
                let name = f.path.last();
                let mut cands: Vec<(usize, usize)> = by_length
                    .get(&f.length)
                    .into_iter()
                    .flatten()
                    .copied()
                    .filter(|&(si, _)| sources[si].id != b.id)
                    .collect();
                cands.sort_by_key(|&(si, fi)| {
                    sources[si].torrent.info.files[fi].path.last() != name
                });
                let need = FileRef {
                    id: b.id.clone(),
                    file: fb,
                };
                let mut adopting = false;
                for (si, fa) in cands.into_iter().take(MAX_CANDIDATES) {
                    let a = &sources[si];
                    let src = FileRef {
                        id: a.id.clone(),
                        file: fa,
                    };
                    let key = (need.clone(), src.clone());
                    if st.rejected.contains(&key) {
                        continue;
                    }
                    let fp = a.fingerprints.get(fa).copied().flatten();
                    let trust = match (f.pieces_root, a.torrent.info.files[fa].pieces_root) {
                        (Some(x), Some(y)) if x != y => continue,
                        (Some(_), Some(_)) => Trust::Root,
                        (Some(x), None)
                            if fp.is_some_and(|fp| fp.pieces_root == x && fp.size == f.length) =>
                        {
                            Trust::Root
                        }
                        _ => Trust::Candidate,
                    };
                    let la = &st.known[&a.id].layout;
                    let complete = la.file_pieces(fa).all(|p| a.have.get(p));
                    if complete && untouched && !adopting && !st.adopt_tried.contains(&key) {
                        st.adopt_tried.insert(key.clone());
                        adopting = true;
                        let same_name = a.torrent.info.files[fa].path.last() == name;
                        adoptions.push((
                            need.clone(),
                            src.clone(),
                            trust,
                            same_name,
                            a.paths[fa].clone().expect("indexed with a path"),
                            fp,
                        ));
                    }
                    keep.insert(key.clone());
                    if !st.pairs.iter().any(|p| p.need == need && p.src == src) {
                        st.pairs.push(Pair {
                            need: need.clone(),
                            src,
                            trust,
                            failures: 0,
                        });
                        st.rescan.insert(key);
                    }
                }
            }
        }
        // Pairs whose torrents are gone, finished or no longer want the file.
        st.pairs
            .retain(|p| keep.contains(&(p.need.clone(), p.src.clone())));
        // Verdicts about torrents that are gone.
        let here = |(need, src): &(FileRef, FileRef)| {
            st.known.contains_key(&need.id) && st.known.contains_key(&src.id)
        };
        st.rejected.retain(here);
        st.adopt_tried.retain(here);
        if std::mem::take(&mut st.lagged) {
            st.rescan
                .extend(st.pairs.iter().map(|p| (p.need.clone(), p.src.clone())));
        }
        st.queue.retain(|j| needy.contains_key(&j.need));
        st.queued.retain(|(id, _)| needy.contains_key(id));
        for (need, src, trust, same_name, path, fp) in adoptions {
            let torrent = needy[&need.id].0.clone();
            self.adopt(st, &torrent, &need, &src, trust, same_name, path, fp, lib)
                .await;
        }
    }

    /// Link (or clone, or copy) a complete source file into a torrent with nothing of it
    /// yet.
    #[allow(clippy::too_many_arguments)]
    async fn adopt(
        &self,
        st: &mut State,
        torrent: &Arc<Torrent>,
        need: &FileRef,
        src: &FileRef,
        trust: Trust,
        same_name: bool,
        path: PathBuf,
        fp: Option<crate::fingerprint::Fingerprint>,
        lib: LibrarySettings,
    ) {
        let (t, file, p) = (torrent.clone(), need.file, path.clone());
        let fit = tokio::task::spawn_blocking(move || {
            if trust == Trust::Root {
                // A root from a fingerprint is good only while the file is unchanged.
                if fp.is_none_or(|fp| fp.current(&p)) {
                    SampleFit::Full
                } else {
                    library::sample_fit(&t, file, &p)
                }
            } else {
                library::sample_fit(&t, file, &p)
            }
        })
        .await
        .unwrap_or(SampleFit::Different);
        let full = match fit {
            SampleFit::Full => true,
            SampleFit::Unknown if same_name => true,
            SampleFit::Partial if lib.partial_reuse => false,
            SampleFit::Different => {
                st.rejected.insert((need.clone(), src.clone()));
                return;
            }
            _ => return,
        };
        let Some(handle) = self.swarm_handle(&need.id) else {
            return;
        };
        let mode = AdoptMode {
            hard_link: full,
            allow_copy: lib.allow_copy,
        };
        match handle.adopt_file(need.file, path, mode).await {
            Ok(a) => {
                let layout = Layout::from_info(&torrent.info);
                let bytes = file_bytes_in(&layout, need.file, a.gained.iter().copied());
                let name = st
                    .known
                    .get(&src.id)
                    .map_or_else(String::new, |k| k.torrent.info.name.clone());
                tracing::info!(
                    torrent = %need.id,
                    file = need.file,
                    from = %src.id,
                    how = ?a.how,
                    bytes,
                    "linked in a file another torrent finished"
                );
                self.inner.reuse.add(
                    &need.id,
                    need.file,
                    (&src.id, &name),
                    adopted_method(a.how),
                    bytes,
                );
                if let Some(k) = st.known.get_mut(&need.id) {
                    for p in a.gained {
                        k.have.set(p);
                    }
                }
            }
            Err(AdoptRefused::InUse) => {}
            Err(AdoptRefused::Io(e)) => {
                tracing::debug!(torrent = %need.id, file = need.file, error = %e, "could not adopt a file");
            }
        }
    }

    /// Offer queued pieces, one at a time, for up to [`SLICE`].
    async fn run_jobs(&self, st: &mut State, lib: LibrarySettings) {
        let started = Instant::now();
        let mut bytes: u64 = 0;
        while started.elapsed() < SLICE {
            let Some(job) = st.queue.pop_front() else {
                break;
            };
            st.queued.remove(&(job.need.clone(), job.piece));
            let Some(known) = st.known.get(&job.need) else {
                continue;
            };
            if known.have.get(job.piece) {
                continue;
            }
            let size = known.layout.piece_size(job.piece) as usize;
            let Some(handle) = self.swarm_handle(&job.need) else {
                continue;
            };
            let reads = job.reads.clone();
            let data = tokio::task::spawn_blocking(move || read_piece(size, &reads))
                .await
                .ok()
                .and_then(Result::ok);
            let Some(data) = data else {
                continue;
            };
            bytes += size as u64;
            let answer = handle.offer_piece(job.piece, data).await;
            self.settle_job(st, &job, &answer);
            // Stay under the rate: sleep until the average is back below it.
            #[allow(clippy::cast_precision_loss)]
            let due = Duration::from_secs_f64(bytes as f64 / lib.share_rate.max(1) as f64);
            if let Some(wait) = due.checked_sub(started.elapsed()) {
                tokio::time::sleep(wait).await;
            }
        }
    }

    fn settle_job(&self, st: &mut State, job: &Job, answer: &Offered) {
        match answer {
            Offered::Taken => {
                if let Some(k) = st.known.get_mut(&job.need) {
                    k.have.set(job.piece);
                }
                for (need, src) in &job.pairs {
                    if let Some(p) = st
                        .pairs
                        .iter_mut()
                        .find(|p| &p.need == need && &p.src == src)
                        && p.trust == Trust::Candidate
                    {
                        tracing::debug!(torrent = %need.id, file = need.file, from = %src.id, "confirmed a shared file");
                        p.trust = Trust::Confirmed;
                    }
                }
                let Some(k) = st.known.get(&job.need) else {
                    return;
                };
                for (need, src) in &job.pairs {
                    let bytes = file_bytes_in(&k.layout, need.file, [job.piece]);
                    let name = st
                        .known
                        .get(&src.id)
                        .map_or_else(String::new, |s| s.torrent.info.name.clone());
                    self.inner.reuse.add(
                        &need.id,
                        need.file,
                        (&src.id, &name),
                        ReuseMethod::PieceSharing,
                        bytes,
                    );
                }
            }
            Offered::Mismatch => {
                for (need, src) in &job.pairs {
                    let Some(i) = st
                        .pairs
                        .iter()
                        .position(|p| &p.need == need && &p.src == src)
                    else {
                        continue;
                    };
                    let p = &mut st.pairs[i];
                    p.failures += 1;
                    if p.trust == Trust::Candidate || p.failures >= MAX_FAILURES {
                        tracing::debug!(torrent = %need.id, file = need.file, from = %src.id, "files differ; not sharing");
                        st.rejected.insert((need.clone(), src.clone()));
                        st.pairs.swap_remove(i);
                    }
                }
            }
            Offered::NotNeeded | Offered::Failed(_) => {}
        }
    }

    /// Write reuse records not yet in the database.
    pub(crate) async fn flush_reuse(&self) {
        let rows = std::mem::take(
            &mut *self
                .inner
                .reuse
                .unsaved
                .lock()
                .expect("reuse lock poisoned"),
        );
        if rows.is_empty() {
            return;
        }
        if let Err(e) = self.db(move |db| db.add_reuse(&rows)).await {
            tracing::warn!(error = %e, "could not save file reuse records");
        }
    }

    /// Record the files linked when torrents were added, once their checks say how much
    /// of each held, and mark them reused in the engine (for bookend pieces).
    pub(crate) async fn settle_reuse(&self) {
        let ready: Vec<(String, TorrentHandle, Arc<Torrent>, Vec<PendingReuse>)> = {
            let mut pending = self
                .inner
                .reuse
                .pending
                .lock()
                .expect("reuse lock poisoned");
            if pending.is_empty() {
                return;
            }
            let swarms = self.inner.swarms.lock().expect("swarms lock poisoned");
            let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
            let ids: Vec<String> = pending.keys().cloned().collect();
            let mut ready = Vec::new();
            for id in ids {
                let Some(swarm) = swarms.get(&id) else {
                    pending.remove(&id);
                    continue;
                };
                if swarm.handle.checked().is_none() {
                    continue;
                }
                let Some(torrent) = torrents
                    .iter()
                    .find(|(k, _)| k.id == id)
                    .map(|(_, e)| e.torrent.clone())
                else {
                    pending.remove(&id);
                    continue;
                };
                let files = pending.remove(&id).expect("listed");
                ready.push((id, swarm.handle.clone(), torrent, files));
            }
            ready
        };
        for (id, handle, torrent, files) in ready {
            let Some(have) = handle.checked() else {
                continue;
            };
            let layout = Layout::from_info(&torrent.info);
            for f in &files {
                let pieces = layout.file_pieces(f.file).filter(|&p| have.get(p));
                let bytes = file_bytes_in(&layout, f.file, pieces);
                self.inner.reuse.add(
                    &id,
                    f.file,
                    (&f.source_id, &f.source_name),
                    linked_method(f.how),
                    bytes,
                );
            }
            handle
                .set_reused(files.iter().map(|f| f.file).collect())
                .await;
        }
        self.flush_reuse().await;
    }
}

/// Turn new pairs and newly verified pieces into jobs.
fn plan(st: &mut State) {
    // The needing torrents' pieces to try: every missing piece of a new pair's file, and
    // the ones overlapping what a source just verified.
    let mut want: HashMap<String, HashSet<u32>> = HashMap::new();
    for (need, _) in std::mem::take(&mut st.rescan) {
        if let Some(k) = st.known.get(&need.id) {
            want.entry(need.id.clone())
                .or_default()
                .extend(k.layout.file_pieces(need.file).filter(|&p| !k.have.get(p)));
        }
    }
    let dirty = std::mem::take(&mut st.dirty);
    for (src_id, pieces) in &dirty {
        let Some(a) = st.known.get(src_id) else {
            continue;
        };
        for pair in st.pairs.iter().filter(|p| &p.src.id == src_id) {
            let Some(b) = st.known.get(&pair.need.id) else {
                continue;
            };
            let fa = &a.layout.files()[pair.src.file];
            let fb = &b.layout.files()[pair.need.file];
            for &p in pieces {
                // The piece's bytes within the source file, in file coordinates…
                let at = a.layout.piece_offset(p);
                let to = at + u64::from(a.layout.piece_size(p));
                let (s, e) = (at.max(fa.offset), to.min(fa.offset + fa.length));
                if s >= e {
                    continue;
                }
                // …and the needing torrent's pieces over the same bytes.
                let (s, e) = (s - fa.offset + fb.offset, e - fa.offset + fb.offset);
                let pl = b.layout.piece_length();
                let first = u32::try_from(s / pl).unwrap_or(u32::MAX);
                let last = u32::try_from((e - 1) / pl).unwrap_or(u32::MAX);
                want.entry(pair.need.id.clone())
                    .or_default()
                    .extend((first..=last).filter(|&q| !b.have.get(q)));
            }
        }
    }
    for (id, pieces) in want {
        let mut pieces: Vec<u32> = pieces.into_iter().collect();
        pieces.sort_unstable();
        for q in pieces {
            if st.queued.contains(&(id.clone(), q)) {
                continue;
            }
            if let Some(job) = job_for(st, &id, q) {
                st.queued.insert((id.clone(), q));
                st.queue.push_back(job);
            }
        }
    }
}

/// A job for piece `q` of torrent `id`, if every byte of it is verified in some source.
fn job_for(st: &State, id: &str, q: u32) -> Option<Job> {
    let b = st.known.get(id)?;
    let segments = b
        .layout
        .segments(b.layout.piece_offset(q), b.layout.piece_size(q) as usize);
    let mut reads = Vec::new();
    let mut pairs = Vec::new();
    for s in segments {
        if s.pad || s.len == 0 {
            continue;
        }
        let pair = st.pairs.iter().find(|p| {
            p.need.id == id
                && p.need.file == s.file
                && st.known.get(&p.src.id).is_some_and(|a| {
                    a.paths.get(p.src.file).is_some_and(Option::is_some)
                        && covered(a, p.src.file, s.file_offset, s.len as u64)
                })
        })?;
        let a = &st.known[&pair.src.id];
        let path = a.paths[pair.src.file].clone().expect("checked above");
        reads.push((path, s.file_offset, s.buf_offset, s.len));
        pairs.push((pair.need.clone(), pair.src.clone()));
    }
    (!reads.is_empty()).then(|| Job {
        need: id.to_owned(),
        piece: q,
        reads,
        pairs,
    })
}

/// Whether bytes `[at, at + len)` of file `file` are in pieces the torrent has.
fn covered(k: &Known, file: usize, at: u64, len: u64) -> bool {
    if len == 0 {
        return true;
    }
    let f = &k.layout.files()[file];
    let start = f.offset + at;
    let pl = k.layout.piece_length();
    let first = u32::try_from(start / pl).unwrap_or(u32::MAX);
    let last = u32::try_from((start + len - 1) / pl).unwrap_or(u32::MAX);
    last < k.layout.piece_count() && (first..=last).all(|p| k.have.get(p))
}

/// Read a piece's bytes from the sources' files.
fn read_piece(size: usize, reads: &[(PathBuf, u64, usize, usize)]) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; size];
    for (path, at, into, len) in reads {
        read_at(path, *at, &mut buf[*into..*into + *len])?;
    }
    Ok(buf)
}

fn read_at(path: &Path, at: u64, buf: &mut [u8]) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(at))?;
    f.read_exact(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use apitorrent_metainfo::{FileSource, TorrentBuilder, Version};

    fn known(files: &[(&str, usize)], piece: u64, have: &[u32]) -> Known {
        let mut b = TorrentBuilder::new("t", Version::V1).piece_length(piece);
        for (name, len) in files {
            b = b.file(FileSource::from_bytes(&[name], vec![0; *len]));
        }
        let torrent = Arc::new(Torrent::from_bytes(&b.build().unwrap()).unwrap());
        let layout = Layout::from_info(&torrent.info);
        let mut bits = Bitfield::new(layout.piece_count());
        for &p in have {
            bits.set(p);
        }
        Known {
            paths: torrent
                .info
                .files
                .iter()
                .map(|f| Some(PathBuf::from(f.path.join("/"))))
                .collect(),
            torrent,
            layout,
            have: bits,
        }
    }

    /// A source with 32 KiB pieces and a needing torrent with 16 KiB pieces, the shared
    /// file at a different offset in each: verifying the source's piece 1 covers exactly
    /// the needing torrent's pieces wholly inside those bytes.
    #[test]
    fn pieces_map_across_alignments() {
        const K: usize = 1024;
        // Source: just the file, 100 KiB. Needing: a 10 KiB file first, then the same file.
        let a = known(&[("x", 100 * K)], 32 * 1024, &[]);
        let b = known(&[("intro", 10 * K), ("x", 100 * K)], 16 * 1024, &[]);
        let mut st = State::default();
        st.known.insert("a".into(), a);
        st.known.insert("b".into(), b);
        st.pairs.push(Pair {
            need: FileRef {
                id: "b".into(),
                file: 1,
            },
            src: FileRef {
                id: "a".into(),
                file: 0,
            },
            trust: Trust::Candidate,
            failures: 0,
        });
        // The source verifies its piece 1: bytes 32..64 KiB of x, which the needing
        // torrent holds at 42..74 KiB of its piece space: pieces 2 to 4.
        st.known.get_mut("a").unwrap().have.set(1);
        st.dirty.insert("a".into(), vec![1]);
        plan(&mut st);
        let planned: Vec<u32> = st.queue.iter().map(|j| j.piece).collect();
        // Piece 2 (32..48 KiB) starts in bytes 22..38 of x: before 32, not covered. Piece 3
        // (48..64) is x's 38..54: covered. Piece 4 (64..80) is 54..70: not covered.
        assert_eq!(planned, [3]);
        let job = &st.queue[0];
        assert_eq!(job.reads.len(), 1);
        assert_eq!(
            job.reads[0].1,
            38 * K as u64,
            "read from x at its own offset"
        );
        // Once the source has the rest, every piece of the needing file is covered except
        // the one shared with the intro, which needs the intro too.
        for p in 0..4 {
            st.known.get_mut("a").unwrap().have.set(p);
        }
        st.queue.clear();
        st.queued.clear();
        st.dirty.insert("a".into(), vec![0, 1, 2, 3]);
        plan(&mut st);
        let mut planned: Vec<u32> = st.queue.iter().map(|j| j.piece).collect();
        planned.sort_unstable();
        assert_eq!(planned, [1, 2, 3, 4, 5, 6]);
    }
}

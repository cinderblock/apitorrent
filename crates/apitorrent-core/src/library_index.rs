//! The core's side of retroactive dedupe (`dedupe`) and library folders (`folders`): the
//! background passes, the admin actions, and keeping a removed torrent's files as
//! candidates.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use apitorrent_engine::TorrentHandle;
use apitorrent_engine::fileid::{FileIdentity, identity};
use apitorrent_engine::layout::Layout;
use apitorrent_types::library::{DedupeMode, DedupeStatus, LibraryFolder, LibraryStatus};
use tokio::sync::Notify;

use crate::db::{FolderFile, FolderRow};
use crate::fingerprint::{self, Fingerprint};
use crate::{Core, CoreError, EngineState, Key, dedupe, folders, now_ms};

/// The meta key of the dedupe totals over the instance's life (JSON `[bytes, files]`).
const TOTALS_KEY: &str = "library_dedupe_totals";
/// How often a dedupe pass runs (report or merge), the first a few minutes after start.
const DEDUPE_EVERY: Duration = Duration::from_secs(60 * 60);
const DEDUPE_FIRST: Duration = Duration::from_secs(3 * 60);
/// How often the folder walker looks for folders due a walk.
const SCAN_TICK: Duration = Duration::from_secs(60);
/// How often the folder hasher looks for files to fingerprint.
const HASH_TICK: Duration = Duration::from_secs(5 * 60);

/// Dedupe and library-folder state shared by the background tasks and the API.
#[derive(Debug, Default)]
pub(crate) struct LibraryState {
    /// What the last pass found, and the totals so far.
    status: Mutex<DedupeStatus>,
    /// A dedupe pass is running.
    running: AtomicBool,
    /// Folders being walked now.
    scanning: Mutex<HashSet<i64>>,
    /// Folders asked to be walked.
    scan_wanted: Mutex<HashSet<i64>>,
    /// Folder files that failed to fingerprint, skipped until their folder's next walk.
    hash_failed: Mutex<HashSet<String>>,
    wake_scan: Notify,
    wake_hash: Notify,
    wake_dedupe: Notify,
}

/// A complete torrent file a dedupe pass looks at.
#[derive(Debug, Clone)]
struct FileRef {
    path: PathBuf,
    owner: (String, usize),
    size: u64,
}

impl Core {
    fn lib(&self) -> &LibraryState {
        &self.inner.library
    }

    fn stopping(&self) -> bool {
        self.inner.stopping.load(Ordering::Relaxed)
    }

    /// Whether any torrent is being checked (which reads the disk hard; idle work waits).
    fn any_checking(&self) -> bool {
        self.inner
            .torrents
            .read()
            .expect("torrents lock poisoned")
            .values()
            .any(|e| e.handle.stats().state == EngineState::Checking)
    }

    /// Start the library's background tasks: dedupe passes, folder walks and folder
    /// fingerprinting.
    pub(crate) async fn start_library(&self) {
        match self.db(|db| db.meta(TOTALS_KEY)).await {
            Ok(Some(json)) => {
                if let Ok((bytes, files)) = serde_json::from_str::<(u64, u64)>(&json) {
                    let mut s = self.lib().status.lock().expect("library lock poisoned");
                    s.reclaimed_bytes = bytes;
                    s.reclaimed_files = files;
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "could not load dedupe totals"),
        }
        tokio::spawn(self.clone().dedupe_loop());
        tokio::spawn(self.clone().scan_loop());
        tokio::spawn(self.clone().hash_loop());
    }

    /// Wait for `d`, a wake-up, or shutdown. False on shutdown.
    async fn idle(&self, d: Duration, wake: &Notify) -> bool {
        tokio::select! {
            () = tokio::time::sleep(d) => {}
            () = wake.notified() => {}
            () = self.inner.stop.notified() => return false,
        }
        !self.stopping()
    }

    async fn dedupe_loop(self) {
        let mut wait = DEDUPE_FIRST;
        loop {
            if !self.idle(wait, &self.inner.library.wake_dedupe).await {
                return;
            }
            wait = DEDUPE_EVERY;
            let lib = self.settings().library;
            if lib.dedupe == DedupeMode::Off || self.any_checking() {
                continue;
            }
            self.dedupe_pass(lib.dedupe == DedupeMode::On).await;
        }
    }

    async fn scan_loop(self) {
        loop {
            if !self.idle(SCAN_TICK, &self.inner.library.wake_scan).await {
                return;
            }
            self.scan_due_folders().await;
        }
    }

    async fn hash_loop(self) {
        loop {
            if !self.idle(HASH_TICK, &self.inner.library.wake_hash).await {
                return;
            }
            let lib = self.settings().library;
            if lib.fingerprint_rate == 0 || self.any_checking() {
                continue;
            }
            self.hash_folders_now().await;
        }
    }

    // ---------------------------------------------------------------- dedupe

    /// Dedupe now: a full pass that merges what it finds, even in "report only" (an
    /// admin's explicit action). Waits for it; returns what it did. Refused while dedupe
    /// is off, and while another pass runs.
    pub async fn dedupe_now(&self) -> Result<DedupeStatus, CoreError> {
        if self.settings().library.dedupe == DedupeMode::Off {
            return Err(CoreError::InvalidRequest(
                "dedupe is off in the library settings".into(),
            ));
        }
        if self.lib().running.load(Ordering::Acquire) {
            return Err(CoreError::InvalidRequest(
                "a dedupe pass is already running".into(),
            ));
        }
        Ok(self.dedupe_pass(true).await)
    }

    /// A report-only pass now: find identical copies and the space merging them would free,
    /// changing nothing. Waits for it. While another pass runs, returns the last status.
    pub async fn dedupe_report(&self) -> DedupeStatus {
        self.dedupe_pass(false).await
    }

    /// Start [`Self::dedupe_now`] in the background.
    pub fn start_dedupe(&self) -> Result<(), CoreError> {
        if self.settings().library.dedupe == DedupeMode::Off {
            return Err(CoreError::InvalidRequest(
                "dedupe is off in the library settings".into(),
            ));
        }
        if self.lib().running.load(Ordering::Acquire) {
            return Ok(());
        }
        let core = self.clone();
        tokio::spawn(async move {
            core.dedupe_pass(true).await;
        });
        Ok(())
    }

    /// The torrent handle owning files of torrent `id` (its swarm's), unless it is being
    /// checked.
    fn idle_handle(&self, id: &str) -> Option<TorrentHandle> {
        let h = self
            .inner
            .swarms
            .lock()
            .expect("swarms lock poisoned")
            .get(id)
            .map(|s| s.handle.clone())?;
        (h.stats().state != EngineState::Checking).then_some(h)
    }

    /// One pass: find identical copies (fingerprinting same-size files as needed), report
    /// them, and with `apply` merge them.
    async fn dedupe_pass(&self, apply: bool) -> DedupeStatus {
        if self
            .lib()
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return self.dedupe_status();
        }
        let result = self.dedupe_pass_inner(apply).await;
        {
            let mut s = self.lib().status.lock().expect("library lock poisoned");
            s.groups = result.groups;
            s.reclaimable_bytes = result.reclaimable_bytes;
            s.reclaimable_files = result.reclaimable_files;
            s.unmergeable_files = result.unmergeable_files;
            s.pending_hash_bytes = result.pending_hash_bytes;
            s.skipped_files = result.skipped_files;
            s.reclaimed_bytes += result.reclaimed_bytes;
            s.reclaimed_files += result.reclaimed_files;
            s.last_pass_at = Some(now_ms());
        }
        if result.reclaimed_files > 0 {
            let totals = {
                let s = self.lib().status.lock().expect("library lock poisoned");
                serde_json::to_string(&(s.reclaimed_bytes, s.reclaimed_files))
                    .expect("totals serialize")
            };
            if let Err(e) = self.db(move |db| db.set_meta(TOTALS_KEY, &totals)).await {
                tracing::warn!(error = %e, "could not save dedupe totals");
            }
        }
        self.lib().running.store(false, Ordering::Release);
        self.dedupe_status()
    }

    /// The complete torrent files worth comparing: at least [`dedupe::MIN_SIZE`], in a
    /// save path, of torrents not being checked.
    async fn dedupe_files(&self) -> Vec<FileRef> {
        let sources = self.library_sources(None, |_| true).await;
        let mut out = Vec::new();
        for s in &sources {
            if self.idle_handle(&s.id).is_none() {
                continue;
            }
            let layout = Layout::from_info(&s.torrent.info);
            for (i, (f, p)) in s.torrent.info.files.iter().zip(&s.paths).enumerate() {
                let Some(p) = p else { continue };
                if f.is_pad() || f.length < dedupe::MIN_SIZE {
                    continue;
                }
                if !layout.file_pieces(i).all(|piece| s.have.get(piece)) {
                    continue;
                }
                out.push(FileRef {
                    path: p.clone(),
                    owner: (s.id.clone(), i),
                    size: f.length,
                });
            }
        }
        out
    }

    /// A current fingerprint of `path` at `stamp`, if one is known.
    fn known_fingerprint(&self, path: &Path, stamp: (u64, i64)) -> Option<Fingerprint> {
        self.inner
            .fingerprints
            .read()
            .expect("fingerprints lock poisoned")
            .get(path)
            .copied()
            .filter(|fp| (fp.size, fp.mtime_ns) == stamp)
    }

    async fn store_fingerprint(&self, path: PathBuf, fp: Fingerprint) {
        let key = path.to_string_lossy().into_owned();
        if self
            .db(move |db| db.set_fingerprint(&key, &fp))
            .await
            .is_ok()
        {
            self.inner
                .fingerprints
                .write()
                .expect("fingerprints lock poisoned")
                .insert(path, fp);
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn dedupe_pass_inner(&self, apply: bool) -> DedupeStatus {
        let mut status = DedupeStatus::default();
        let files = self.dedupe_files().await;
        // Identity and stamp of each, off the runtime.
        let looked: Vec<Option<(FileIdentity, (u64, i64))>> = {
            let paths: Vec<PathBuf> = files.iter().map(|f| f.path.clone()).collect();
            tokio::task::spawn_blocking(move || {
                paths
                    .iter()
                    .map(|p| Some((identity(p).ok()?, fingerprint::stamp(p).ok()?)))
                    .collect()
            })
            .await
            .unwrap_or_default()
        };
        let mut files: Vec<(FileRef, FileIdentity, (u64, i64))> = files
            .into_iter()
            .zip(looked)
            .filter_map(|(f, l)| {
                let (id, st) = l?;
                (st.0 == f.size).then_some((f, id, st))
            })
            .collect();
        // Only sizes held by two or more separate files can hold duplicates.
        let reps = dedupe::needing_hash(&files, |f| f.0.size, |f| f.1);
        let rep_ids: HashSet<(u64, u128)> = reps
            .iter()
            .map(|&i| (files[i].1.volume, files[i].1.index))
            .collect();
        files.retain(|f| rep_ids.contains(&(f.1.volume, f.1.index)));

        // A hash per separate file: known ones first, then read the others.
        let mut hashes: HashMap<(u64, u128), (Fingerprint, PathBuf)> = HashMap::new();
        for (f, id, st) in &files {
            if let Some(fp) = self.known_fingerprint(&f.path, *st) {
                hashes.insert((id.volume, id.index), (fp, f.path.clone()));
            }
        }
        let rate = self.settings().library.fingerprint_rate;
        let mut todo: Vec<&(FileRef, FileIdentity, (u64, i64))> = Vec::new();
        let mut queued: HashSet<(u64, u128)> = HashSet::new();
        for f in &files {
            let k = (f.1.volume, f.1.index);
            if !hashes.contains_key(&k) && queued.insert(k) {
                todo.push(f);
            }
        }
        for (f, id, _) in todo {
            let k = (id.volume, id.index);
            if rate == 0 || self.stopping() || self.any_checking() {
                status.pending_hash_bytes += f.size;
                continue;
            }
            let (p, stop) = (f.path.clone(), self.inner.stopping.clone());
            match tokio::task::spawn_blocking(move || fingerprint::compute(&p, rate, &stop)).await {
                Ok(Ok(fp)) => {
                    self.store_fingerprint(f.path.clone(), fp).await;
                    hashes.insert(k, (fp, f.path.clone()));
                }
                Ok(Err(e)) => {
                    tracing::debug!(path = %f.path.display(), error = %e, "dedupe: could not hash");
                    status.pending_hash_bytes += f.size;
                }
                Err(e) => tracing::warn!(error = %e, "dedupe hash task failed"),
            }
        }

        let copies: Vec<dedupe::Copy> = files
            .iter()
            .filter_map(|(f, id, st)| {
                let (fp, _) = hashes.get(&(id.volume, id.index))?;
                // A link of a hashed file shares its stamp; anything else changed since.
                ((fp.size, fp.mtime_ns) == *st).then(|| dedupe::Copy {
                    path: f.path.clone(),
                    owner: f.owner.clone(),
                    size: f.size,
                    mtime_ns: st.1,
                    sha256: fp.sha256,
                    id: *id,
                })
            })
            .collect();
        let plan = dedupe::plan(&copies);
        status.groups = plan.groups;
        status.reclaimable_bytes = plan.reclaimable_bytes;
        status.reclaimable_files = plan.reclaimable_files;
        status.unmergeable_files = plan.unmergeable_files;
        if !apply {
            return status;
        }

        for m in &plan.merges {
            let keeper = &copies[m.keeper[0]];
            let keeper_owners: Vec<(String, usize)> =
                m.keeper.iter().map(|&i| copies[i].owner.clone()).collect();
            let Some((keeper_fp, _)) = hashes.get(&(keeper.id.volume, keeper.id.index)).cloned()
            else {
                status.skipped_files += m.targets.iter().map(|t| t.len() as u64).sum::<u64>();
                continue;
            };
            for target in &m.targets {
                let mut all = true;
                let mut how_all = Vec::new();
                for &ti in target {
                    match self
                        .replace_copy(keeper, &keeper_owners, &copies[ti], rate)
                        .await
                    {
                        Ok(how) => {
                            how_all.push(how);
                            let new_stamp = fingerprint::stamp(&copies[ti].path)
                                .unwrap_or((keeper_fp.size, keeper_fp.mtime_ns));
                            let fp = Fingerprint {
                                size: new_stamp.0,
                                mtime_ns: new_stamp.1,
                                ..keeper_fp
                            };
                            self.store_fingerprint(copies[ti].path.clone(), fp).await;
                            tracing::info!(
                                path = %copies[ti].path.display(),
                                keeper = %keeper.path.display(),
                                ?how,
                                "dedupe: replaced an identical copy with a link"
                            );
                        }
                        Err(e) => {
                            all = false;
                            status.skipped_files += 1;
                            tracing::info!(path = %copies[ti].path.display(), error = %e, "dedupe: left a copy alone");
                        }
                    }
                }
                status.reclaimed_files += how_all.len() as u64;
                if all && !how_all.is_empty() {
                    status.reclaimed_bytes += m.size;
                    status.reclaimable_bytes = status.reclaimable_bytes.saturating_sub(m.size);
                }
            }
        }
        status.reclaimable_files = status
            .reclaimable_files
            .saturating_sub(status.reclaimed_files);
        status
    }

    /// Replace one copy with a link to the keeper: verify both in full, then swap with the
    /// copy's torrent holding the file. The keeper's files are marked linked just before
    /// the rename, so neither side can be written through the link.
    async fn replace_copy(
        &self,
        keeper: &dedupe::Copy,
        keeper_owners: &[(String, usize)],
        target: &dedupe::Copy,
        rate: u64,
    ) -> Result<crate::library::Linked, dedupe::SwapError> {
        let busy = || dedupe::SwapError::Io(std::io::Error::other("the torrent is busy"));
        let handle = self.idle_handle(&target.owner.0).ok_or_else(busy)?;
        let mut keeper_handles = Vec::new();
        for (id, file) in keeper_owners {
            keeper_handles.push((self.idle_handle(id).ok_or_else(busy)?, *file));
        }
        let (kp, ks) = (keeper.path.clone(), (keeper.size, keeper.mtime_ns));
        let (tp, ts) = (target.path.clone(), (target.size, target.mtime_ns));
        let (sha, stop, file) = (target.sha256, self.inner.stopping.clone(), target.owner.1);
        tokio::task::spawn_blocking(move || {
            dedupe::verify(&kp, ks, &tp, ts, &sha, rate, &stop)?;
            let mut how = None;
            let mut swap_err = None;
            let swapped = handle.replace_file(file, &mut |at| {
                let r = dedupe::link_over(&kp, at, ts, &mut || {
                    if handle.stats().state == EngineState::Checking {
                        return Err(std::io::Error::other("the torrent started a check"));
                    }
                    for (h, f) in &keeper_handles {
                        h.mark_linked(&[*f]);
                    }
                    Ok(())
                });
                match r {
                    Ok(h) => {
                        how = Some(h);
                        Ok(())
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        swap_err = Some(e);
                        Err(std::io::Error::other(msg))
                    }
                }
            });
            match (swapped, how) {
                (Ok(()), Some(h)) => Ok(h),
                (Err(e), _) => Err(swap_err.unwrap_or(dedupe::SwapError::Io(e))),
                (Ok(()), None) => Err(dedupe::SwapError::NotLinkable),
            }
        })
        .await
        .unwrap_or_else(|e| Err(dedupe::SwapError::Io(std::io::Error::other(e.to_string()))))
    }

    /// What dedupe last found and has done.
    pub fn dedupe_status(&self) -> DedupeStatus {
        let mut s = self
            .lib()
            .status
            .lock()
            .expect("library lock poisoned")
            .clone();
        s.mode = self.settings().library.dedupe;
        s.running = self.lib().running.load(Ordering::Acquire);
        s
    }

    // ---------------------------------------------------------------- folders

    /// Dedupe and library folders, for Settings → Library.
    pub async fn library_status(&self) -> Result<LibraryStatus, CoreError> {
        let rows = self.db(|db| db.library_folders()).await?;
        let scanning = self
            .lib()
            .scanning
            .lock()
            .expect("library lock poisoned")
            .clone();
        Ok(LibraryStatus {
            dedupe: self.dedupe_status(),
            folders: rows
                .into_iter()
                .map(|r| folder_info(r, &scanning))
                .collect(),
        })
    }

    /// Add a library folder: an absolute path to an existing folder. It is walked at once.
    pub async fn add_library_folder(&self, path: &str) -> Result<LibraryFolder, CoreError> {
        let p = Path::new(path.trim());
        if !p.is_absolute() {
            return Err(CoreError::InvalidRequest(
                "a library folder must be an absolute path".into(),
            ));
        }
        let p = std::path::absolute(p)?;
        if !p.is_dir() {
            return Err(CoreError::InvalidRequest(format!(
                "{} is not a folder on the server",
                p.display()
            )));
        }
        let s = p.to_string_lossy().into_owned();
        let at = now_ms();
        let id = self
            .db(move |db| db.add_library_folder(&s, at))
            .await?
            .ok_or_else(|| {
                CoreError::InvalidRequest("that folder is already a library folder".into())
            })?;
        self.rescan_library_folder(id);
        let rows = self.db(|db| db.library_folders()).await?;
        let scanning = self
            .lib()
            .scanning
            .lock()
            .expect("library lock poisoned")
            .clone();
        rows.into_iter()
            .find(|r| r.id == id)
            .map(|r| folder_info(r, &scanning))
            .ok_or(CoreError::NotFound)
    }

    /// Forget a library folder. Its files are not touched; torrents already linked to them
    /// keep their links.
    pub async fn remove_library_folder(&self, id: i64) -> Result<(), CoreError> {
        if self.db(move |db| db.remove_library_folder(id)).await? {
            Ok(())
        } else {
            Err(CoreError::NotFound)
        }
    }

    /// Ask for a folder to be walked again soon.
    pub fn rescan_library_folder(&self, id: i64) {
        self.lib()
            .scan_wanted
            .lock()
            .expect("library lock poisoned")
            .insert(id);
        self.lib().wake_scan.notify_one();
    }

    /// Walk the folders that are due (never walked, past the rescan interval, or asked
    /// for), one after another.
    async fn scan_due_folders(&self) {
        let Ok(rows) = self.db(|db| db.library_folders()).await else {
            return;
        };
        let hours = self.settings().library.folder_rescan_hours;
        let now = now_ms();
        let wanted: HashSet<i64> = std::mem::take(
            &mut *self
                .lib()
                .scan_wanted
                .lock()
                .expect("library lock poisoned"),
        );
        for r in rows {
            let due = wanted.contains(&r.id)
                || r.last_scan_at.is_none()
                || (hours > 0 && now - r.last_scan_at.unwrap_or(0) >= i64::from(hours) * 3_600_000);
            if due && !self.stopping() {
                let _ = self.scan_folder_now(r.id).await;
            }
        }
    }

    /// Walk one folder now and record what changed. Returns how many files it holds.
    pub async fn scan_folder_now(&self, id: i64) -> Result<usize, CoreError> {
        let rows = self.db(|db| db.library_folders()).await?;
        let row = rows
            .into_iter()
            .find(|r| r.id == id)
            .ok_or(CoreError::NotFound)?;
        if !self
            .lib()
            .scanning
            .lock()
            .expect("library lock poisoned")
            .insert(id)
        {
            return Err(CoreError::InvalidRequest(
                "the folder is being walked".into(),
            ));
        }
        let (root, stop) = (PathBuf::from(&row.path), self.inner.stopping.clone());
        let walked = tokio::task::spawn_blocking(move || folders::walk(&root, &stop)).await;
        let result = match walked {
            Ok(Ok(found)) => {
                let n = found.len();
                let old = self.db(move |db| db.folder_stamps(id)).await;
                match old {
                    Ok(old) => {
                        let (up, del) = folders::diff(&old, &found);
                        let changed = !up.is_empty();
                        let r = self
                            .db(move |db| {
                                db.apply_folder_scan(id, &up, &del)?;
                                db.set_folder_scanned(id, now_ms(), None)
                            })
                            .await;
                        let prefix = row.path.clone();
                        self.lib()
                            .hash_failed
                            .lock()
                            .expect("library lock poisoned")
                            .retain(|p| !Path::new(p).starts_with(&prefix));
                        if changed {
                            self.lib().wake_hash.notify_one();
                        }
                        r.map(|()| n)
                    }
                    Err(e) => Err(e),
                }
            }
            Ok(Err(folders::WalkError::Stopped)) => Ok(0),
            Ok(Err(folders::WalkError::Io(e))) => {
                let msg = e.to_string();
                tracing::warn!(folder = %row.path, error = %msg, "could not walk a library folder");
                let _ = self
                    .db(move |db| db.set_folder_scanned(id, now_ms(), Some(&msg)))
                    .await;
                Err(CoreError::Io(e))
            }
            Err(e) => Err(CoreError::Io(std::io::Error::other(e.to_string()))),
        };
        self.lib()
            .scanning
            .lock()
            .expect("library lock poisoned")
            .remove(&id);
        result
    }

    /// Fingerprint every folder file that needs it, at the configured rate, stopping early
    /// for a torrent check or shutdown. Returns how many were done. The background task
    /// calls this; tests call it directly.
    pub async fn hash_folders_now(&self) -> usize {
        let rate = self.settings().library.fingerprint_rate;
        let mut done = 0;
        loop {
            let skip = self
                .lib()
                .hash_failed
                .lock()
                .expect("library lock poisoned")
                .clone();
            let limit = skip.len() + 16;
            let Ok(batch) = self.db(move |db| db.unhashed_folder_files(limit)).await else {
                return done;
            };
            let batch: Vec<FolderFile> = batch
                .into_iter()
                .filter(|f| !skip.contains(&f.path))
                .collect();
            if batch.is_empty() {
                return done;
            }
            for f in batch {
                if self.stopping() || self.any_checking() {
                    return done;
                }
                let (p, stop) = (PathBuf::from(&f.path), self.inner.stopping.clone());
                match tokio::task::spawn_blocking(move || fingerprint::compute(&p, rate, &stop))
                    .await
                {
                    Ok(Ok(fp)) => {
                        let (id, path) = (f.folder_id, f.path.clone());
                        if self
                            .db(move |db| db.set_folder_fingerprint(id, &path, &fp))
                            .await
                            .is_ok()
                        {
                            done += 1;
                        }
                    }
                    Ok(Err(fingerprint::Error::Stopped)) => return done,
                    Ok(Err(e)) => {
                        tracing::debug!(path = %f.path, error = %e, "library folder: could not fingerprint");
                        self.lib()
                            .hash_failed
                            .lock()
                            .expect("library lock poisoned")
                            .insert(f.path);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "fingerprint task failed");
                        return done;
                    }
                }
            }
        }
    }

    /// The folder files of these sizes, for matching a new torrent. Empty when there are no
    /// folders (or on a database error, which just means no reuse).
    pub(crate) async fn folder_candidates(&self, sizes: Vec<u64>) -> Vec<FolderFile> {
        if sizes.is_empty() {
            return Vec::new();
        }
        self.db(move |db| db.folder_files_by_size(&sizes))
            .await
            .unwrap_or_default()
    }

    /// A torrent is being removed and its files kept: record its complete files that are
    /// under a library folder, so later torrents can reuse them. Their roots (from a v2
    /// torrent, or a current fingerprint) carry over, so a match needs no reading.
    pub(crate) async fn keep_as_folder_files(
        &self,
        torrent: &apitorrent_metainfo::Torrent,
        paths: &[PathBuf],
        complete: &[bool],
    ) {
        let Ok(rows) = self.db(|db| db.library_folders()).await else {
            return;
        };
        if rows.is_empty() {
            return;
        }
        let roots: Vec<(i64, PathBuf)> = rows
            .into_iter()
            .map(|r| (r.id, PathBuf::from(r.path)))
            .collect();
        let mut records = Vec::new();
        for (i, (f, p)) in torrent.info.files.iter().zip(paths).enumerate() {
            if f.is_pad() || f.length == 0 || !complete.get(i).copied().unwrap_or(false) {
                continue;
            }
            let Some((folder_id, _)) = folders::folder_of(p, &roots) else {
                continue;
            };
            let Ok(st) = fingerprint::stamp(p) else {
                continue;
            };
            if st.0 != f.length {
                continue;
            }
            let fp = self.known_fingerprint(p, st);
            records.push(FolderFile {
                folder_id: *folder_id,
                path: p.to_string_lossy().into_owned(),
                size: st.0,
                mtime_ns: st.1,
                pieces_root: f.pieces_root.map(|r| r.0).or(fp.map(|fp| fp.pieces_root.0)),
                sha256: fp.map(|fp| fp.sha256),
            });
        }
        if records.is_empty() {
            return;
        }
        let n = records.len();
        let r = self
            .db(move |db| {
                for rec in &records {
                    db.upsert_folder_file(rec)?;
                }
                Ok(())
            })
            .await;
        match r {
            Ok(()) => tracing::info!(
                files = n,
                "kept a removed torrent's files as library candidates"
            ),
            Err(e) => tracing::warn!(error = %e, "could not record a removed torrent's files"),
        }
    }

    /// Which of an entry's files are complete, for [`Self::keep_as_folder_files`].
    pub(crate) async fn complete_files(
        &self,
        key: &Key,
        handle: &TorrentHandle,
        torrent: &apitorrent_metainfo::Torrent,
        placed: &[bool],
    ) -> Vec<bool> {
        if !self.is_primary(key) {
            return placed.to_vec();
        }
        let Some(have) = handle.have().await else {
            return Vec::new();
        };
        let layout = Layout::from_info(&torrent.info);
        let placements = handle.placements();
        (0..torrent.info.files.len())
            .map(|i| {
                placements.get(i) == Some(&apitorrent_engine::storage::Placement::Save)
                    && layout.file_pieces(i).all(|p| have.get(p))
            })
            .collect()
    }
}

fn folder_info(r: FolderRow, scanning: &HashSet<i64>) -> LibraryFolder {
    LibraryFolder {
        id: r.id,
        scanning: scanning.contains(&r.id),
        path: r.path,
        added_at: r.added_at,
        last_scan_at: r.last_scan_at,
        files: r.files,
        bytes: r.bytes,
        hashed_files: r.hashed_files,
        hashed_bytes: r.hashed_bytes,
        error: r.error,
    }
}

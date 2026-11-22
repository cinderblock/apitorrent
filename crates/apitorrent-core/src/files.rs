//! File order and moving completed files: which setting applies to a torrent, the order
//! its files download in, and where each completed file goes.
//!
//! Both settings come from the torrent itself, else its group (found by label), else the
//! instance, like seed limits. The engine only gets the result: an order of file indexes
//! and, file by file, a path to move to.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::Duration;

use apitorrent_engine::FileEvent;
use apitorrent_engine::layout::Layout;
use apitorrent_engine::storage::Placement;
use apitorrent_types::Settings;
use apitorrent_types::files::{FileOrder, MoveCompleted, MoveWhen, SequentialFile};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;

use crate::automation::matcher::FileGlob;
use crate::automation::version::natural;
use crate::{Core, CoreError, Key};

/// The file order that applies to a torrent in `group`, given its own.
pub fn effective_order(
    settings: &Settings,
    group: Option<&str>,
    own: Option<&FileOrder>,
) -> FileOrder {
    own.cloned()
        .or_else(|| {
            settings
                .groups
                .iter()
                .find(|g| Some(g.name.as_str()) == group)?
                .file_order
                .clone()
        })
        .unwrap_or_else(|| settings.file_order.clone())
}

/// The move-when-complete setting that applies to a torrent in `group`, given its own.
pub fn effective_move(
    settings: &Settings,
    group: Option<&str>,
    own: Option<&MoveCompleted>,
) -> MoveCompleted {
    own.cloned()
        .or_else(|| {
            settings
                .groups
                .iter()
                .find(|g| Some(g.name.as_str()) == group)?
                .move_completed
                .clone()
        })
        .unwrap_or_else(|| settings.move_completed.clone())
}

/// One file as the order sees it.
#[derive(Debug)]
pub struct OrderFile<'a> {
    /// Its path inside the torrent, `/`-separated.
    pub path: &'a str,
    pub length: u64,
    /// Pad files and files not downloaded (priority 0) are left out of the order.
    pub wanted: bool,
}

/// The engine's file order: indexes of wanted files, first first. `None` for no
/// preference.
pub fn order_files(files: &[OrderFile<'_>], order: &FileOrder) -> Option<Vec<usize>> {
    let by_name = |a: &usize, b: &usize| {
        natural(files[*a].path)
            .cmp(&natural(files[*b].path))
            .then(a.cmp(b))
    };
    let mut wanted: Vec<usize> = (0..files.len()).filter(|&i| files[i].wanted).collect();
    match order {
        FileOrder::Off => return None,
        FileOrder::Torrent => {}
        FileOrder::Name => wanted.sort_by(by_name),
        FileOrder::SmallestFirst => {
            wanted.sort_by(|a, b| {
                files[*a]
                    .length
                    .cmp(&files[*b].length)
                    .then_with(|| by_name(a, b))
            });
        }
        FileOrder::LargestFirst => {
            wanted.sort_by(|a, b| {
                files[*b]
                    .length
                    .cmp(&files[*a].length)
                    .then_with(|| by_name(a, b))
            });
        }
        FileOrder::Manual { files: first } => {
            let place = |i: usize| {
                first
                    .iter()
                    .position(|&f| f as usize == i)
                    .unwrap_or(usize::MAX)
            };
            wanted.sort_by(|a, b| match place(*a).cmp(&place(*b)) {
                Ordering::Equal => by_name(a, b),
                other => other,
            });
        }
    }
    Some(wanted)
}

/// A [`MoveCompleted`] ready to use.
#[derive(Debug)]
pub struct Mover {
    pub when: MoveWhen,
    to: Option<PathBuf>,
    rules: Vec<(FileGlob, Option<PathBuf>)>,
}

impl Mover {
    /// `None` when the setting moves nothing. A rule that doesn't compile is skipped
    /// (settings are validated when saved, so that only happens to old stored ones).
    pub fn new(m: &MoveCompleted) -> Option<Self> {
        if !m.active() {
            return None;
        }
        let dir = |s: &str| (!s.trim().is_empty()).then(|| PathBuf::from(s.trim()));
        Some(Self {
            when: m.when,
            to: dir(&m.to),
            rules: m
                .rules
                .iter()
                .filter_map(|r| Some((FileGlob::compile(&r.pattern).ok()?, dir(&r.to))))
                .collect(),
        })
    }

    /// Where a file goes, or `None` to leave it in place. Rules match `path`, its path
    /// inside the torrent (`/`-separated); it keeps `relative`, its path under the save
    /// directory (also `/`-separated), under its destination.
    pub fn destination(&self, path: &str, relative: &str) -> Option<PathBuf> {
        let dir = self
            .rules
            .iter()
            .find(|(glob, _)| glob.is_match(path))
            .map_or(self.to.as_ref(), |(_, to)| to.as_ref())?;
        Some(relative.split('/').fold(dir.clone(), |p, c| p.join(c)))
    }
}

/// Check a setting before it is saved: patterns compile, and destinations are absolute.
pub fn validate_move(m: &MoveCompleted) -> Result<(), String> {
    let absolute = |s: &str, what: &str| {
        let s = s.trim();
        if s.is_empty() || Path::new(s).is_absolute() {
            Ok(())
        } else {
            Err(format!("{what} must be a full path: {s}"))
        }
    };
    absolute(&m.to, "the destination")?;
    for r in &m.rules {
        if r.pattern.trim().is_empty() {
            return Err("a move rule needs a pattern".to_owned());
        }
        FileGlob::compile(&r.pattern).map_err(|e| format!("move rule {}: {e}", r.pattern))?;
        absolute(&r.to, "a move rule's destination")?;
    }
    Ok(())
}

/// One entry's own file settings, and the files it moved. Stored as JSON per entry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FileOptions {
    /// Its own file order; `None` follows its group's or the instance's.
    pub order: Option<FileOrder>,
    /// Files downloaded front to back, each with the byte its reader needs next.
    pub sequential: Vec<(u32, u64)>,
    /// Its own move-when-complete setting; `None` follows its group's or the instance's.
    pub move_completed: Option<MoveCompleted>,
    /// Files moved out of the save path when they completed, and where to.
    pub moved: Vec<(u32, String)>,
    /// Each wanted file's first and last piece first.
    pub first_last: bool,
    /// Super-seeding (BEP 16).
    pub super_seed: bool,
}

impl FileOptions {
    pub fn is_sequential(&self, file: usize) -> bool {
        self.sequential.iter().any(|&(f, _)| f as usize == file)
    }

    pub fn moved_to(&self, file: usize) -> Option<&str> {
        self.moved
            .iter()
            .find(|(f, _)| *f as usize == file)
            .map(|(_, p)| p.as_str())
    }

    /// The engine's resume data for moved files: one entry per file, `None` when none moved.
    pub fn locations(&self, files: usize) -> Option<Vec<Option<PathBuf>>> {
        if self.moved.is_empty() {
            return None;
        }
        let mut out = vec![None; files];
        for (f, p) in &self.moved {
            if let Some(slot) = out.get_mut(*f as usize) {
                *slot = Some(PathBuf::from(p));
            }
        }
        Some(out)
    }

    fn set_moved(&mut self, file: usize, to: String) {
        let file = u32::try_from(file).expect("fits");
        self.moved.retain(|(f, _)| *f != file);
        self.moved.push((file, to));
        self.moved.sort_unstable();
    }
}

/// How often completed files are looked at again, in case an event was missed (a torrent
/// still checking when its files completed, say). Failed moves wait for a restart or a
/// change of setting instead.
const SWEEP_EVERY: Duration = Duration::from_secs(300);

impl Core {
    /// Set a torrent's own file order (`None` follows its group's or the instance's).
    pub async fn set_file_order(
        &self,
        key: &Key,
        order: Option<FileOrder>,
    ) -> Result<(), CoreError> {
        {
            let mut torrents = self.inner.torrents.write().expect("torrents lock poisoned");
            let e = torrents.get_mut(key).ok_or(CoreError::NotFound)?;
            if let Some(FileOrder::Manual { files }) = &order
                && let Some(bad) = files
                    .iter()
                    .find(|&&f| f as usize >= e.torrent.info.files.len())
            {
                return Err(CoreError::InvalidRequest(format!("no file {bad}")));
            }
            e.files.order = order;
        }
        self.save_file_options(key).await?;
        self.sync_file_handling(&key.id).await;
        Ok(())
    }

    /// Fetch each wanted file's first and last piece first, or stop.
    pub async fn set_first_last(&self, key: &Key, on: bool) -> Result<(), CoreError> {
        {
            let mut torrents = self.inner.torrents.write().expect("torrents lock poisoned");
            torrents
                .get_mut(key)
                .ok_or(CoreError::NotFound)?
                .files
                .first_last = on;
        }
        self.save_file_options(key).await?;
        self.sync_file_handling(&key.id).await;
        Ok(())
    }

    /// Super-seed the torrent (BEP 16), or stop.
    pub async fn set_super_seed(&self, key: &Key, on: bool) -> Result<(), CoreError> {
        {
            let mut torrents = self.inner.torrents.write().expect("torrents lock poisoned");
            torrents
                .get_mut(key)
                .ok_or(CoreError::NotFound)?
                .files
                .super_seed = on;
        }
        self.save_file_options(key).await?;
        self.sync_file_handling(&key.id).await;
        Ok(())
    }

    /// Download files front to back (or stop), from the byte each reader needs next.
    pub async fn set_sequential(
        &self,
        key: &Key,
        updates: &[SequentialFile],
    ) -> Result<(), CoreError> {
        let (handle, changes) = {
            let mut torrents = self.inner.torrents.write().expect("torrents lock poisoned");
            let others: Vec<(u32, u64)> = torrents
                .iter()
                .filter(|(k, _)| k.id == key.id && *k != key)
                .flat_map(|(_, e)| e.files.sequential.iter().copied())
                .collect();
            let e = torrents.get_mut(key).ok_or(CoreError::NotFound)?;
            let files = &e.torrent.info.files;
            let mut changes = Vec::new();
            for u in updates {
                let i = u.index as usize;
                if files
                    .get(i)
                    .is_none_or(apitorrent_metainfo::FileEntry::is_pad)
                {
                    return Err(CoreError::InvalidRequest(format!("no file {}", u.index)));
                }
                e.files.sequential.retain(|&(f, _)| f != u.index);
                if u.sequential {
                    let from = u.from.min(files[i].length.saturating_sub(1));
                    e.files.sequential.push((u.index, from));
                    changes.push((i, Some(from)));
                } else if let Some(&(_, from)) = others.iter().find(|(f, _)| *f == u.index) {
                    // Another space still reads it.
                    changes.push((i, Some(from)));
                } else {
                    changes.push((i, None));
                }
            }
            e.files.sequential.sort_unstable();
            (e.handle.clone(), changes)
        };
        self.save_file_options(key).await?;
        for (file, from) in changes {
            handle.set_stream(file, from).await;
        }
        Ok(())
    }

    /// Set a torrent's own move-when-complete setting (`None` follows its group's or the
    /// instance's). Files already complete move straight away; files moved before stay
    /// where they are unless the new setting sends them elsewhere.
    pub async fn set_move_completed(
        &self,
        key: &Key,
        setting: Option<MoveCompleted>,
    ) -> Result<(), CoreError> {
        if let Some(m) = &setting {
            validate_move(m).map_err(CoreError::InvalidRequest)?;
            // Like a save path, a destination must be inside the space's storage roots.
            for to in std::iter::once(&m.to).chain(m.rules.iter().map(|r| &r.to)) {
                if !to.trim().is_empty() {
                    self.check_save_path(key.space, Path::new(to.trim()))?;
                }
            }
        }
        {
            let mut torrents = self.inner.torrents.write().expect("torrents lock poisoned");
            let e = torrents.get_mut(key).ok_or(CoreError::NotFound)?;
            e.files.move_completed = setting;
            e.move_errors.clear();
        }
        self.save_file_options(key).await?;
        let core = self.clone();
        let key = key.clone();
        tokio::spawn(async move { core.move_files(&key, None, true).await });
        Ok(())
    }

    async fn save_file_options(&self, key: &Key) -> Result<(), CoreError> {
        let json = {
            let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
            let e = torrents.get(key).ok_or(CoreError::NotFound)?;
            (e.files != FileOptions::default())
                .then(|| serde_json::to_string(&e.files).expect("file options serialize"))
        };
        let k = key.clone();
        self.db(move |db| db.set_file_options(&k, json.as_deref()))
            .await?;
        Ok(())
    }

    /// Give a swarm its file order (the primary's setting, over the files any entry wants)
    /// and every entry's sequential files.
    pub(crate) async fn sync_file_handling(&self, id: &str) {
        let settings = self.settings();
        let (handle, order, streams, first_last, super_seed) = {
            let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
            let swarms = self.inner.swarms.lock().expect("swarms lock poisoned");
            let Some(swarm) = swarms.get(id) else { return };
            let Some(p) = torrents.get(&Key::new(swarm.primary, id.to_owned())) else {
                return;
            };
            let entries: Vec<&crate::Entry> = torrents
                .iter()
                .filter(|(k, _)| k.id == id)
                .map(|(_, e)| e)
                .collect();
            let display: Vec<String> = p
                .torrent
                .info
                .files
                .iter()
                .map(apitorrent_metainfo::FileEntry::display_path)
                .collect();
            let files: Vec<OrderFile<'_>> = p
                .torrent
                .info
                .files
                .iter()
                .enumerate()
                .map(|(i, f)| OrderFile {
                    path: &display[i],
                    length: f.length,
                    wanted: !f.is_pad()
                        && entries
                            .iter()
                            .any(|e| e.file_priorities.get(i).is_some_and(|&x| x > 0)),
                })
                .collect();
            let effective = effective_order(&settings, p.group.as_deref(), p.files.order.as_ref());
            let order = order_files(&files, &effective);
            let mut streams: Vec<(u32, u64)> = p.files.sequential.clone();
            for e in &entries {
                for &(f, from) in &e.files.sequential {
                    if streams.iter().all(|(g, _)| *g != f) {
                        streams.push((f, from));
                    }
                }
            }
            // Swarm-wide switches: on when any entry has them on.
            let first_last = entries.iter().any(|e| e.files.first_last);
            let super_seed = entries.iter().any(|e| e.files.super_seed);
            (swarm.handle.clone(), order, streams, first_last, super_seed)
        };
        handle.set_file_order(order).await;
        handle.set_first_last(first_last).await;
        handle.set_super_seed(super_seed).await;
        for (file, from) in streams {
            handle.set_stream(file as usize, Some(from)).await;
        }
    }

    /// The instance's or a group's settings changed (`None`), or one torrent's label did:
    /// send the file order again, and move files the new settings want elsewhere.
    pub(crate) fn files_settings_changed(&self, key: Option<Key>) {
        let core = self.clone();
        tokio::spawn(async move {
            let ids: Vec<String> = match &key {
                Some(k) => vec![k.id.clone()],
                None => core
                    .inner
                    .swarms
                    .lock()
                    .expect("swarms lock poisoned")
                    .keys()
                    .cloned()
                    .collect(),
            };
            for id in &ids {
                core.sync_file_handling(id).await;
            }
            core.sweep_moves(key.as_ref(), true).await;
        });
    }

    /// Moves completed files as they complete, until shutdown.
    pub(crate) async fn mover(self) {
        let mut events = self.inner.session.file_events();
        self.sweep_moves(None, true).await;
        let mut tick = tokio::time::interval(SWEEP_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tokio::select! {
                ev = events.recv() => match ev {
                    Ok(e) => {
                        if let Some(key) = self.primary_of(&e) {
                            self.move_files(&key, Some(e.file), false).await;
                        }
                    }
                    Err(RecvError::Lagged(_)) => self.sweep_moves(None, false).await,
                    Err(RecvError::Closed) => return,
                },
                _ = tick.tick() => self.sweep_moves(None, false).await,
                () = self.inner.stop.notified() => return,
            }
        }
    }

    /// The primary entry of the swarm an event is about.
    fn primary_of(&self, e: &FileEvent) -> Option<Key> {
        self.inner
            .swarms
            .lock()
            .expect("swarms lock poisoned")
            .iter()
            .find(|(_, s)| s.handle.info_hashes() == e.torrent)
            .map(|(id, s)| Key::new(s.primary, id.clone()))
    }

    /// Move whatever is complete and due, for one torrent (`only`) or every one whose
    /// setting moves anything.
    async fn sweep_moves(&self, only: Option<&Key>, retry_errors: bool) {
        let settings = self.settings();
        let keys: Vec<Key> = {
            let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
            let swarms = self.inner.swarms.lock().expect("swarms lock poisoned");
            swarms
                .iter()
                .map(|(id, s)| Key::new(s.primary, id.clone()))
                .filter(|k| only.is_none_or(|o| o == k))
                .filter(|k| {
                    torrents.get(k).is_some_and(|e| {
                        effective_move(
                            &settings,
                            e.group.as_deref(),
                            e.files.move_completed.as_ref(),
                        )
                        .active()
                    })
                })
                .collect()
        };
        for key in keys {
            self.move_files(&key, None, retry_errors).await;
        }
    }

    /// Move one torrent's completed files where its setting sends them: file `only` (it
    /// just completed), or every file. "Whole torrent" moves nothing until every wanted
    /// file is complete. Files that failed to move before are left alone unless
    /// `retry_errors`.
    async fn move_files(&self, key: &Key, only: Option<usize>, retry_errors: bool) {
        // Never alongside a move to another save path: that stops the engine torrent and
        // changes where every file is. One waiting its turn is swept again when it's done.
        let _turn = self.inner.move_lock.lock().await;
        let settings = self.settings();
        let plan = {
            let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
            let Some(e) = torrents.get(key) else { return };
            if !self.is_primary(key) || e.moving {
                return;
            }
            let effective = effective_move(
                &settings,
                e.group.as_deref(),
                e.files.move_completed.as_ref(),
            );
            let Some(mover) = Mover::new(&effective) else {
                return;
            };
            let wanted: Vec<bool> = e
                .torrent
                .info
                .files
                .iter()
                .enumerate()
                .map(|(i, f)| {
                    !f.is_pad()
                        && torrents
                            .iter()
                            .filter(|(k, _)| k.id == key.id)
                            .any(|(_, x)| x.file_priorities.get(i).is_some_and(|&p| p > 0))
                })
                .collect();
            let display: Vec<String> = e
                .torrent
                .info
                .files
                .iter()
                .map(apitorrent_metainfo::FileEntry::display_path)
                .collect();
            let relative: Vec<String> = Self::laid_out_paths(e)
                .iter()
                .zip(&display)
                .map(|(p, shown)| {
                    p.strip_prefix(&e.save_path).map_or_else(
                        |_| shown.clone(),
                        |r| {
                            r.components()
                                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                                .collect::<Vec<_>>()
                                .join("/")
                        },
                    )
                })
                .collect();
            let errors: Vec<usize> = if retry_errors {
                Vec::new()
            } else {
                e.move_errors.keys().copied().collect()
            };
            (
                e.handle.clone(),
                e.torrent.clone(),
                mover,
                wanted,
                display,
                relative,
                Self::entry_paths(e),
                errors,
            )
        };
        let (handle, torrent, mover, wanted, display, relative, current, errors) = plan;
        let Some(have) = handle.have().await else {
            return;
        };
        let layout = Layout::from_info(&torrent.info);
        let complete = |i: usize| layout.file_pieces(i).all(|p| have.get(p));
        let wanted_files: Vec<usize> = (0..wanted.len()).filter(|&i| wanted[i]).collect();
        let files = match mover.when {
            MoveWhen::EachFile => only.map_or_else(|| wanted_files.clone(), |f| vec![f]),
            MoveWhen::WholeTorrent => {
                if !wanted_files.iter().all(|&i| complete(i)) {
                    return;
                }
                wanted_files
            }
        };
        let placements = {
            let h = handle.clone();
            tokio::task::spawn_blocking(move || h.placements())
                .await
                .unwrap_or_default()
        };
        let mut changed = false;
        for i in files {
            if !wanted.get(i).copied().unwrap_or(false)
                || !complete(i)
                || placements.get(i) != Some(&Placement::Save)
                || errors.contains(&i)
            {
                continue;
            }
            let Some(to) = mover.destination(&display[i], &relative[i]) else {
                continue;
            };
            if current.get(i) == Some(&to) {
                continue;
            }
            let job = (key.id.clone(), i);
            if !self
                .inner
                .moving_files
                .lock()
                .expect("moving files lock poisoned")
                .insert(job.clone())
            {
                continue;
            }
            let result = handle.relocate_file(i, to.clone()).await;
            self.inner
                .moving_files
                .lock()
                .expect("moving files lock poisoned")
                .remove(&job);
            let name = display[i].as_str();
            match &result {
                Ok(()) => {
                    tracing::info!(torrent = %key, file = %to.display(), "moved a completed file");
                }
                Err(e) => {
                    tracing::warn!(torrent = %key, file = name, to = %to.display(), error = %e, "could not move a completed file");
                }
            }
            if let Some(e) = self
                .inner
                .torrents
                .write()
                .expect("torrents lock poisoned")
                .get_mut(key)
            {
                match result {
                    Ok(()) => {
                        e.files.set_moved(i, to.to_string_lossy().into_owned());
                        e.move_errors.remove(&i);
                    }
                    Err(err) => {
                        e.move_errors.insert(i, err.to_string());
                    }
                }
            }
            changed = true;
        }
        if changed {
            if let Err(e) = self.save_file_options(key).await {
                tracing::warn!(torrent = %key, error = %e, "could not save where files moved");
            }
            self.refresh();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apitorrent_types::files::MoveRule;

    fn files<'a>(list: &'a [(&'a str, u64, bool)]) -> Vec<OrderFile<'a>> {
        list.iter()
            .map(|&(path, length, wanted)| OrderFile {
                path,
                length,
                wanted,
            })
            .collect()
    }

    #[test]
    fn orders_by_name_size_torrent_and_hand() {
        let list = [
            ("show/ep10.mkv", 300, true),
            ("show/ep2.mkv", 100, true),
            ("show/skipped.nfo", 1, false),
            ("show/ep1.mkv", 200, true),
        ];
        let f = files(&list);
        assert_eq!(order_files(&f, &FileOrder::Off), None);
        assert_eq!(order_files(&f, &FileOrder::Name), Some(vec![3, 1, 0]));
        assert_eq!(
            order_files(&f, &FileOrder::SmallestFirst),
            Some(vec![1, 3, 0])
        );
        assert_eq!(
            order_files(&f, &FileOrder::LargestFirst),
            Some(vec![0, 3, 1])
        );
        assert_eq!(order_files(&f, &FileOrder::Torrent), Some(vec![0, 1, 3]));
        assert_eq!(
            order_files(&f, &FileOrder::Manual { files: vec![0, 2] }),
            Some(vec![0, 3, 1]),
            "listed first (unwanted ones skipped), the rest by name"
        );
    }

    #[test]
    fn the_most_specific_setting_wins() {
        let mut s = crate::settings::defaults(&apitorrent_engine::SessionConfig::default());
        s.file_order = FileOrder::Name;
        s.groups.push(apitorrent_types::GroupSettings {
            name: "tv".into(),
            labels: vec!["tv".into()],
            download_rate: 0,
            upload_rate: 0,
            max_connections: 0,
            seed_limits: None,
            file_order: Some(FileOrder::SmallestFirst),
            move_completed: Some(MoveCompleted {
                enabled: true,
                to: "/public".into(),
                ..MoveCompleted::default()
            }),
        });
        assert_eq!(effective_order(&s, None, None), FileOrder::Name);
        assert_eq!(
            effective_order(&s, Some("tv"), None),
            FileOrder::SmallestFirst
        );
        assert_eq!(
            effective_order(&s, Some("tv"), Some(&FileOrder::Off)),
            FileOrder::Off
        );
        assert!(!effective_move(&s, None, None).active());
        assert!(effective_move(&s, Some("tv"), None).active());
        let off = MoveCompleted::default();
        assert!(!effective_move(&s, Some("tv"), Some(&off)).active());
    }

    #[cfg(windows)]
    const ROOT: &str = r"C:\public";
    #[cfg(not(windows))]
    const ROOT: &str = "/public";

    #[test]
    fn rules_pick_destinations_and_keep_relative_paths() {
        let video = Path::new(ROOT).join("video");
        let m = MoveCompleted {
            enabled: true,
            to: ROOT.into(),
            when: MoveWhen::EachFile,
            rules: vec![
                MoveRule {
                    pattern: "*.nfo".into(),
                    to: String::new(),
                },
                MoveRule {
                    pattern: "*.MKV".into(),
                    to: video.to_string_lossy().into_owned(),
                },
            ],
        };
        validate_move(&m).unwrap();
        let mover = Mover::new(&m).unwrap();
        assert_eq!(
            mover.destination("Show/S01/ep1.mkv", "Show/S01/ep1.mkv"),
            Some(video.join("Show").join("S01").join("ep1.mkv"))
        );
        assert_eq!(
            mover.destination("Show/info.nfo", "Show/info.nfo"),
            None,
            "an empty rule keeps it"
        );
        assert_eq!(
            mover.destination("Show/cover.jpg", "Show/cover.jpg"),
            Some(Path::new(ROOT).join("Show").join("cover.jpg"))
        );
        let only_rules = MoveCompleted {
            to: String::new(),
            ..m.clone()
        };
        assert_eq!(
            Mover::new(&only_rules)
                .unwrap()
                .destination("a/b.jpg", "a/b.jpg"),
            None
        );
        assert!(
            Mover::new(&MoveCompleted {
                enabled: false,
                ..m.clone()
            })
            .is_none()
        );
    }

    #[test]
    fn bad_settings_are_refused() {
        let relative = MoveCompleted {
            enabled: true,
            to: "public".into(),
            ..MoveCompleted::default()
        };
        assert!(validate_move(&relative).is_err());
        let no_pattern = MoveCompleted {
            enabled: true,
            rules: vec![MoveRule::default()],
            ..MoveCompleted::default()
        };
        assert!(validate_move(&no_pattern).is_err());
    }
}

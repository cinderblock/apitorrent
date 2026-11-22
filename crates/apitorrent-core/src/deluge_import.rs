//! Planning a Deluge import into a space, and the plain-text report of it.
//!
//! [`plan`] turns what [`apitorrent_deluge::load`] read into the rows apitorrent stores,
//! without touching the filesystem beyond reading it. Two callers carry a plan out: the
//! running daemon (`Core::import_deluge`, which starts each torrent at once), and
//! `apitorrentd import deluge`, which writes the rows for the daemon to start on its next
//! start (see [`write`]). [`report`] describes a plan in plain text meant to be pasted into
//! a chat: counts, save paths and the storage roots they fall in, labels, limits, and what
//! can't be carried over.
//!
//! Importing twice is safe: torrents the space already has are left alone and listed.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use apitorrent_deluge::{DelugeSettings, DelugeState};
use apitorrent_engine::bitfield::Bitfield;
use apitorrent_metainfo::{Magnet, Torrent};
use apitorrent_types::spaces::{SpaceId, SpaceSettings};
use apitorrent_types::{
    DelugeImportFilter, DelugeImportOptions, DelugeSettingsPick, ImportSkip, SeedLimitAction,
    SeedLimits, Settings, TorrentLimits,
};

use crate::db::{Database, TorrentRow};
use crate::seeding::SeedState;
use crate::{Key, import};

/// Where a plan imports to.
#[derive(Debug, Clone)]
pub struct Target<'a> {
    pub space: SpaceId,
    pub slug: &'a str,
    /// The space's storage roots; empty means any path.
    pub roots: &'a [String],
    /// Where torrents without a save path of their own go.
    pub download_dir: &'a Path,
    /// Torrent ids the space already has.
    pub present: &'a HashSet<String>,
    /// Save path prefixes to rewrite (`from`, `to`).
    pub map: &'a [(String, String)],
    /// Add everything paused.
    pub paused: bool,
    /// Give every torrent Deluge's stop-at-ratio as limits of its own (a space other than
    /// `default`, whose seed limits have no space-wide level).
    pub ratio_per_torrent: bool,
    /// What of each torrent comes over.
    pub options: &'a DelugeImportOptions,
    /// The queue position of the first imported torrent.
    pub first_position: u32,
}

/// One torrent to add.
#[derive(Debug, Clone)]
pub struct PlannedTorrent {
    pub name: String,
    /// Deluge's id for it (its v1 info-hash, lowercase hex), which a hybrid or v2
    /// torrent's apitorrent id isn't.
    pub deluge_id: String,
    pub torrent: Arc<Torrent>,
    pub row: TorrentRow,
    pub have: Option<Bitfield>,
    pub file_paths: Option<Vec<Vec<String>>>,
    pub limits: TorrentLimits,
    pub seed: SeedState,
    pub label: Option<String>,
    /// Deluge options that don't carry over.
    pub not_carried: Vec<String>,
    /// Only a `.torrent` file left in Deluge's state directory (see
    /// [`apitorrent_deluge::DelugeTorrent::leftover`]): imported paused, with nothing to
    /// take over.
    pub leftover: bool,
    /// File order, sequential files and moving completed files.
    pub files: crate::files::FileOptions,
}

/// A magnet link Deluge never got the metadata for.
#[derive(Debug, Clone)]
pub struct PlannedMagnet {
    pub name: String,
    pub deluge_id: String,
    pub uri: String,
    pub row: TorrentRow,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub torrents: Vec<PlannedTorrent>,
    pub magnets: Vec<PlannedMagnet>,
    /// Names of torrents the space already has.
    pub already: Vec<String>,
    pub skipped: Vec<ImportSkip>,
}

/// Deluge's "stop seeding at ratio" (with "remove") as seed limits.
pub fn seed_limits(stop: Option<(f64, bool)>) -> SeedLimits {
    stop.map_or_else(SeedLimits::default, |(ratio, remove)| SeedLimits {
        ratio,
        seed_minutes: 0,
        action: if remove {
            SeedLimitAction::Remove
        } else {
            SeedLimitAction::Pause
        },
    })
}

/// The picked ones of Deluge's settings applied to the instance's (for the `default`
/// space).
pub fn apply_instance_settings(s: &mut Settings, d: &DelugeSettings, pick: DelugeSettingsPick) {
    if pick.download_rate {
        s.download_rate = d.max_download_rate.unwrap_or(0);
    }
    if pick.upload_rate {
        s.upload_rate = d.max_upload_rate.unwrap_or(0);
    }
    if pick.max_connections
        && let Some(n) = d.max_connections
    {
        s.max_connections = n;
    }
    if pick.queue_slots {
        s.queue.max_active_downloads = d.max_active_downloading.unwrap_or(0);
        s.queue.max_active_seeds = d.max_active_seeding.unwrap_or(0);
        s.queue.max_active = d.max_active_limit.unwrap_or(0);
    }
    if pick.ignore_slow {
        s.queue.ignore_slow = d.dont_count_slow;
    }
    if pick.stop_at_ratio {
        s.seed_limits = seed_limits(d.stop_at_ratio);
    }
}

/// The picked ones of Deluge's settings applied to another space's: its rates,
/// connections and slots. (Its stop-at-ratio goes on each torrent: see
/// [`Target::ratio_per_torrent`].)
pub fn apply_space_settings(s: &mut SpaceSettings, d: &DelugeSettings, pick: DelugeSettingsPick) {
    if pick.download_rate {
        s.download_rate = d.max_download_rate.unwrap_or(0);
    }
    if pick.upload_rate {
        s.upload_rate = d.max_upload_rate.unwrap_or(0);
    }
    if pick.max_connections {
        s.max_connections = d.max_connections.unwrap_or(0);
    }
    if pick.queue_slots {
        s.max_active_downloads = d.max_active_downloading.unwrap_or(0);
        s.max_active_seeds = d.max_active_seeding.unwrap_or(0);
        s.max_active = d.max_active_limit.unwrap_or(0);
    }
}

/// Deluge's settings, for showing before an import.
#[must_use]
pub fn settings_view(d: &DelugeSettings) -> apitorrent_types::DelugeSettingsView {
    apitorrent_types::DelugeSettingsView {
        download_rate: d.max_download_rate,
        upload_rate: d.max_upload_rate,
        max_connections: d.max_connections,
        max_active_downloading: d.max_active_downloading,
        max_active_seeding: d.max_active_seeding,
        max_active: d.max_active_limit,
        dont_count_slow: d.dont_count_slow,
        stop_at_ratio: stop_at_ratio(d.stop_at_ratio),
    }
}

fn stop_at_ratio(s: Option<(f64, bool)>) -> Option<apitorrent_types::StopAtRatio> {
    s.map(|(ratio, remove)| apitorrent_types::StopAtRatio { ratio, remove })
}

/// The storage root a path is in: `Some(root)`, `Some("")` when the space has none (any
/// path), `None` when it is outside all of them.
fn root_of<'r>(roots: &'r [String], path: &Path) -> Option<&'r str> {
    if roots.is_empty() {
        return Some("");
    }
    roots
        .iter()
        .find(|r| path.starts_with(r.as_str()))
        .map(String::as_str)
}

fn now_ms() -> i64 {
    crate::now_ms()
}

/// What one Deluge torrent is, for previewing and filtering an import.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    /// The info-hash id apitorrent will give it (Deluge's own id when it can't be read).
    pub id: String,
    /// Deluge's id: the v1 info-hash in hex.
    pub deluge_id: String,
    pub name: String,
    pub label: Option<String>,
    /// Its trackers' hosts, lowercase, without duplicates.
    pub hosts: Vec<String>,
    /// Where its files are, after the path mapping.
    pub save_path: PathBuf,
    pub size: u64,
    /// Done, 0 to 1, from the resume data.
    pub progress: Option<f64>,
    /// Not running in Deluge (paused there, or a leftover).
    pub paused: bool,
    /// A `.torrent` file left in Deluge's state directory that Deluge doesn't list.
    pub leftover: bool,
    /// Why it can't be read.
    pub error: Option<String>,
    /// What else it has that an import may carry over.
    pub details: Details,
}

/// A Deluge torrent's options, as the import's choices see them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Details {
    pub private: bool,
    pub limits: TorrentLimits,
    pub stop_at_ratio: Option<(f64, bool)>,
    /// Files Deluge doesn't download.
    pub skipped_files: u32,
    pub renamed_files: u32,
    pub downloaded: u64,
    pub uploaded: u64,
    pub seeding_secs: u64,
    pub queue: Option<u32>,
    /// Milliseconds since the epoch.
    pub added_at: Option<i64>,
    /// Deluge's tracker list differs from the `.torrent` file's.
    pub edited_trackers: bool,
    pub not_carried: Vec<String>,
}

/// Tracker tiers without empty tiers or blank URLs, for comparing.
fn tiers(t: &[Vec<String>]) -> Vec<Vec<&str>> {
    t.iter()
        .map(|tier| {
            tier.iter()
                .map(|u| u.trim())
                .filter(|u| !u.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|tier| !tier.is_empty())
        .collect()
}

impl Summary {
    pub fn complete(&self) -> bool {
        self.progress.is_some_and(|p| p >= 1.0)
    }
}

/// The host of a tracker URL, lowercase.
pub fn tracker_host(url: &str) -> Option<String> {
    url::Url::parse(url.trim())
        .ok()?
        .host_str()
        .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
}

fn hosts<'a>(urls: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    let mut out: Vec<String> = urls.into_iter().filter_map(|u| tracker_host(u)).collect();
    out.sort();
    out.dedup();
    out
}

/// Each of Deluge's torrents, described (in Deluge's order).
pub fn summarize(
    state: &DelugeState,
    map: &[(String, String)],
    download_dir: &Path,
) -> Vec<Summary> {
    state
        .torrents
        .iter()
        .map(|d| {
            let save_path = import::save_path(
                d.save_path.as_deref(),
                state.settings.download_location.as_deref(),
                download_dir,
                map,
            );
            let mut out = Summary {
                id: d.id.to_ascii_lowercase(),
                deluge_id: d.id.to_ascii_lowercase(),
                name: d.id.clone(),
                label: d.label.clone(),
                hosts: d
                    .trackers
                    .as_ref()
                    .map(|t| hosts(t.iter().flatten()))
                    .unwrap_or_default(),
                save_path,
                size: 0,
                progress: None,
                paused: d.paused || d.leftover,
                leftover: d.leftover,
                error: None,
                details: Details {
                    private: false,
                    limits: TorrentLimits {
                        download_rate: d.max_download_rate.unwrap_or(0),
                        upload_rate: d.max_upload_rate.unwrap_or(0),
                        max_connections: d.max_connections,
                    },
                    stop_at_ratio: d.stop_at_ratio,
                    skipped_files: 0,
                    renamed_files: u32::try_from(d.renamed.len()).unwrap_or(u32::MAX),
                    downloaded: d.total_downloaded,
                    uploaded: d.total_uploaded,
                    seeding_secs: d.seeding_secs,
                    queue: d.queue,
                    added_at: d.added_at.map(|s| s.saturating_mul(1000)),
                    edited_trackers: false,
                    not_carried: d.not_carried.clone(),
                },
            };
            match Torrent::from_bytes(&d.torrent) {
                Ok(t) => {
                    out.details.private = t.info.private;
                    out.details.edited_trackers = d
                        .trackers
                        .as_ref()
                        .is_some_and(|mine| tiers(mine) != tiers(&t.trackers));
                    let files = t.info.files.len();
                    out.details.skipped_files = d
                        .file_priorities
                        .as_ref()
                        .filter(|p| p.len() == files)
                        .map_or(0, |p| p.iter().map(|&x| u32::from(x == 0)).sum());
                    out.id = t.info.info_hashes().id_string();
                    out.name.clone_from(&t.info.name);
                    out.size = t.info.content_length();
                    if d.trackers.is_none() {
                        out.hosts = hosts(t.trackers.iter().flatten());
                    }
                    let count = t.info.piece_count();
                    out.progress = d
                        .pieces
                        .as_deref()
                        .filter(|p| p.len() as u64 == count && count > 0)
                        .map(|p| {
                            #[allow(clippy::cast_precision_loss, reason = "a fraction for display")]
                            let f = p.iter().filter(|&&b| b != 0).count() as f64 / count as f64;
                            f
                        });
                }
                Err(e) => match d.magnet.as_deref().map(Magnet::parse) {
                    Some(Ok(m)) => {
                        out.id = m.info_hashes.id_string();
                        if let Some(n) = m.display_name {
                            out.name = n;
                        }
                        if d.trackers.is_none() {
                            out.hosts = hosts(&m.trackers);
                        }
                        out.progress = Some(0.0);
                    }
                    _ => out.error = Some(format!("unreadable .torrent: {e}")),
                },
            }
            out
        })
        .collect()
}

/// Whether a host matches a filter entry: the same host, or a subdomain of it.
fn host_matches(host: &str, entry: &str) -> bool {
    let entry = entry.trim().trim_end_matches('.').to_ascii_lowercase();
    !entry.is_empty() && (host == entry || host.ends_with(&format!(".{entry}")))
}

/// Whether a torrent passes a filter.
pub fn matches(s: &Summary, f: &DelugeImportFilter) -> bool {
    let label = s.label.as_deref().unwrap_or("");
    let same = |a: &str, b: &str| a.trim().eq_ignore_ascii_case(b.trim());
    if !f.labels.is_empty() && !f.labels.iter().any(|l| same(l, label)) {
        return false;
    }
    if f.exclude_labels.iter().any(|l| same(l, label)) {
        return false;
    }
    if !f.trackers.is_empty()
        && !s
            .hosts
            .iter()
            .any(|h| f.trackers.iter().any(|e| host_matches(h, e)))
    {
        return false;
    }
    if !f.save_paths.is_empty()
        && !f
            .save_paths
            .iter()
            .any(|p| !p.trim().is_empty() && s.save_path.starts_with(p.trim()))
    {
        return false;
    }
    if f.complete.is_some_and(|c| c != s.complete()) {
        return false;
    }
    if f.paused.is_some_and(|p| p != s.paused) {
        return false;
    }
    if let Some(words) = &f.name {
        let name = s.name.to_lowercase();
        if !words
            .split_whitespace()
            .all(|w| name.contains(&w.to_lowercase()))
        {
            return false;
        }
    }
    if !f.ids.is_empty() {
        let wanted = |id: &str| f.ids.iter().any(|w| w.trim().eq_ignore_ascii_case(id));
        if !wanted(&s.id) && !wanted(&s.deluge_id) {
            return false;
        }
    }
    true
}

/// Keep only the torrents the filter takes; returns how many were left out.
pub fn select(
    state: &mut DelugeState,
    filter: &DelugeImportFilter,
    map: &[(String, String)],
    download_dir: &Path,
) -> u32 {
    if filter.is_empty() {
        return 0;
    }
    let keep: Vec<bool> = summarize(state, map, download_dir)
        .iter()
        .map(|s| matches(s, filter))
        .collect();
    let before = state.torrents.len();
    let mut i = 0;
    state.torrents.retain(|_| {
        i += 1;
        keep[i - 1]
    });
    u32::try_from(before - state.torrents.len()).unwrap_or(u32::MAX)
}

/// A space slug from a Deluge config directory's name (`deluge-tv` → `deluge-tv`,
/// `.config/deluge` → `deluge`).
pub fn suggested_slug(config_dir: &Path) -> String {
    let name = config_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug: String = slug.trim_matches('-').chars().take(40).collect();
    let slug = slug.trim_end_matches('-').to_owned();
    if slug.is_empty() {
        "deluge".to_owned()
    } else {
        slug
    }
}

/// Plan importing what Deluge has into `t`'s space.
pub fn plan(state: &DelugeState, t: &Target<'_>) -> Plan {
    let o = t.options;
    let mut out = Plan::default();
    let mut position = t.first_position;
    let mut seen: HashSet<String> = HashSet::new();
    // Deluge's queue order (the order `load` gives), or the order they were added in.
    let mut order: Vec<&apitorrent_deluge::DelugeTorrent> = state.torrents.iter().collect();
    if !o.queue_order {
        order.sort_by_key(|d| (d.added_at.is_none(), d.added_at.unwrap_or(0), d.id.clone()));
    }
    let added_at = |d: &apitorrent_deluge::DelugeTorrent| match d.added_at {
        Some(s) if o.added_dates => s.saturating_mul(1000),
        _ => now_ms(),
    };
    for d in order {
        let label = o.label_for(d.label.as_deref());
        let bytes = match &d.trackers {
            Some(tiers) if o.trackers => {
                import::with_trackers(&d.torrent, tiers).unwrap_or_else(|| d.torrent.clone())
            }
            _ => d.torrent.clone(),
        };
        let torrent = match Torrent::from_bytes(&bytes) {
            Ok(parsed) => Arc::new(parsed),
            Err(e) => {
                // A magnet that never got its metadata in Deluge: add it as a magnet.
                let Some(uri) = &d.magnet else {
                    out.skipped.push(ImportSkip {
                        name: d.id.clone(),
                        reason: format!("unreadable .torrent: {e}"),
                    });
                    continue;
                };
                let magnet = match Magnet::parse(uri) {
                    Ok(m) => m,
                    Err(e) => {
                        out.skipped.push(ImportSkip {
                            name: d.id.clone(),
                            reason: format!("no metadata, and its magnet link doesn't parse: {e}"),
                        });
                        continue;
                    }
                };
                let id = magnet.info_hashes.id_string();
                let name = magnet.display_name.clone().unwrap_or_else(|| id.clone());
                if t.present.contains(&id) || !seen.insert(id.clone()) {
                    out.already.push(name);
                    continue;
                }
                let save = import::save_path(
                    d.save_path.as_deref(),
                    state.settings.download_location.as_deref(),
                    t.download_dir,
                    t.map,
                );
                if root_of(t.roots, &save).is_none() {
                    out.skipped.push(ImportSkip {
                        name,
                        reason: format!(
                            "{} is outside space {}'s storage roots",
                            save.display(),
                            t.slug
                        ),
                    });
                    continue;
                }
                let row = TorrentRow {
                    space: t.space,
                    id: id.clone(),
                    info_hash_v1: magnet.info_hashes.get_v1().map(|h| h.to_hex()),
                    info_hash_v2: magnet.info_hashes.get_v2().map(|h| h.to_hex()),
                    name: name.clone(),
                    torrent: Vec::new(),
                    save_path: save.to_string_lossy().into_owned(),
                    label: label.clone(),
                    added_at: added_at(d),
                    paused: t.paused || d.paused,
                    have: None,
                    file_priorities: None,
                    downloaded_total: 0,
                    uploaded_total: 0,
                    placements: None,
                    held_path: None,
                    unwanted_policy: 0,
                    queue_position: position,
                    download_rate: 0,
                    upload_rate: 0,
                    max_connections: None,
                    magnet: Some(uri.clone()),
                    naming: 0,
                    linked: None,
                    file_paths: None,
                    trackers: None,
                };
                position += 1;
                out.magnets.push(PlannedMagnet {
                    name,
                    deluge_id: d.id.to_ascii_lowercase(),
                    uri: uri.clone(),
                    row,
                });
                continue;
            }
        };
        let name = torrent.info.name.clone();
        let id = torrent.info.info_hashes().id_string();
        if t.present.contains(&id) || !seen.insert(id.clone()) {
            out.already.push(name);
            continue;
        }
        let save_path = import::save_path(
            d.save_path.as_deref(),
            state.settings.download_location.as_deref(),
            t.download_dir,
            t.map,
        );
        if root_of(t.roots, &save_path).is_none() {
            out.skipped.push(ImportSkip {
                name,
                reason: format!(
                    "{} is outside space {}'s storage roots",
                    save_path.display(),
                    t.slug
                ),
            });
            continue;
        }
        let files = torrent.info.files.len();
        let mut prios = d
            .file_priorities
            .clone()
            .filter(|p| p.len() == files)
            .unwrap_or_else(|| vec![4; files]);
        if !o.skipped_files {
            for p in prios.iter_mut().filter(|p| **p == 0) {
                *p = 4;
            }
        }
        let pieces = u32::try_from(torrent.info.piece_count()).unwrap_or(0);
        let have = d
            .pieces
            .as_deref()
            .and_then(|p| import::pieces_bitfield(p, pieces));
        let file_paths = import::file_layout(&torrent, &save_path, &d.renamed);
        let limits = if o.torrent_limits {
            TorrentLimits {
                download_rate: d.max_download_rate.unwrap_or(0),
                upload_rate: d.max_upload_rate.unwrap_or(0),
                max_connections: d.max_connections,
            }
        } else {
            TorrentLimits::default()
        };
        let (downloaded, uploaded, seeding_secs) = if o.totals {
            (d.total_downloaded, d.total_uploaded, d.seeding_secs)
        } else {
            (0, 0, 0)
        };
        let hashes = torrent.info.info_hashes();
        let row = TorrentRow {
            space: t.space,
            id: id.clone(),
            info_hash_v1: hashes.get_v1().map(|h| h.to_hex()),
            info_hash_v2: hashes.get_v2().map(|h| h.to_hex()),
            name: name.clone(),
            torrent: bytes,
            save_path: save_path.to_string_lossy().into_owned(),
            label: label.clone(),
            added_at: added_at(d),
            // A leftover never ran in Deluge; it waits here to be looked at.
            paused: t.paused || d.paused || d.leftover,
            have: have.as_ref().map(|h| h.as_bytes().to_vec()),
            file_priorities: Some(prios),
            downloaded_total: downloaded,
            uploaded_total: uploaded,
            placements: None,
            held_path: None,
            unwanted_policy: 0,
            queue_position: position,
            download_rate: limits.download_rate,
            upload_rate: limits.upload_rate,
            max_connections: limits.max_connections,
            magnet: None,
            naming: 0,
            linked: None,
            file_paths: file_paths
                .as_ref()
                .map(|p| serde_json::to_string(p).expect("paths serialize")),
            trackers: None,
        };
        position += 1;
        // Deluge copies its global "stop at ratio" into every torrent, so only a torrent
        // that differs from the global setting gets limits of its own (or every one, the
        // global setting's, where the global setting has nowhere to go).
        let global = state.settings.stop_at_ratio;
        let own = if o.seed_limits && d.stop_at_ratio != global {
            Some(seed_limits(d.stop_at_ratio))
        } else {
            t.ratio_per_torrent.then(|| seed_limits(global))
        };
        out.torrents.push(PlannedTorrent {
            name,
            deluge_id: d.id.to_ascii_lowercase(),
            torrent,
            row,
            have,
            file_paths,
            limits,
            seed: SeedState {
                seed_ms: seeding_secs.saturating_mul(1000),
                limits: own,
                reached: false,
            },
            label,
            not_carried: d.not_carried.clone(),
            leftover: d.leftover,
            files: crate::files::FileOptions::default(),
        });
    }
    out
}

/// Store a plan's torrents for the daemon to start on its next start, each recorded as
/// imported from `source` (Deluge's config directory). Those that start running were taken
/// over at `taken_over_at` (see [`take_over_running`]); paused ones wait to be. Returns how
/// many were written; one that fails is skipped with its reason.
pub fn write(
    db: &mut Database,
    plan: &Plan,
    source: &str,
    taken_over_at: Option<i64>,
) -> (usize, Vec<ImportSkip>) {
    let mut written = 0;
    let mut failed = Vec::new();
    let imported = |db: &Database, row: &TorrentRow, deluge_id: &str, leftover: bool| {
        let r = imported_row(row, deluge_id, source, taken_over_at, leftover);
        if let Err(e) = db.set_imported(&r) {
            tracing::warn!(torrent = %row.name, error = %e, "could not record it as imported");
        }
    };
    for t in &plan.torrents {
        match db.insert_torrent(&t.row) {
            Ok(()) => {
                let key = Key::new(t.row.space, t.row.id.clone());
                if let Err(e) = db.save_seed_states(&[(key, t.seed)]) {
                    tracing::warn!(torrent = %t.name, error = %e, "could not store seeding time");
                }
                imported(db, &t.row, &t.deluge_id, t.leftover);
                written += 1;
            }
            Err(e) => failed.push(ImportSkip {
                name: t.name.clone(),
                reason: e.to_string(),
            }),
        }
    }
    for m in &plan.magnets {
        match db.insert_torrent(&m.row) {
            Ok(()) => {
                imported(db, &m.row, &m.deluge_id, false);
                written += 1;
            }
            Err(e) => failed.push(ImportSkip {
                name: m.name.clone(),
                reason: e.to_string(),
            }),
        }
    }
    (written, failed)
}

/// The record of one imported torrent: taken over at `taken_over_at` if it starts running,
/// waiting to be if it is added paused. A `leftover` (a `.torrent` file Deluge doesn't
/// list) has nothing to take over, so it counts as taken over at once.
pub fn imported_row(
    row: &TorrentRow,
    deluge_id: &str,
    source: &str,
    taken_over_at: Option<i64>,
    leftover: bool,
) -> crate::db::ImportedRow {
    let now = now_ms();
    crate::db::ImportedRow {
        key: Key::new(row.space, row.id.clone()),
        source: source.to_owned(),
        imported_at: now,
        taken_over_at: if leftover {
            Some(now)
        } else if row.paused {
            None
        } else {
            taken_over_at
        },
        source_id: (deluge_id != row.id).then(|| deluge_id.to_owned()),
        leftover,
    }
}

/// Deluge's ids of the plan's torrents that would start running here.
#[must_use]
pub fn running_ids(plan: &Plan) -> Vec<String> {
    plan.torrents
        .iter()
        .filter(|t| !t.row.paused)
        .map(|t| t.deluge_id.clone())
        .chain(
            plan.magnets
                .iter()
                .filter(|m| !m.row.paused)
                .map(|m| m.deluge_id.clone()),
        )
        .collect()
}

/// The tracker hosts of the plan's private torrents, sorted: the trackers that may only
/// accept clients on a list, and knew these torrents as Deluge's.
#[must_use]
pub fn private_tracker_hosts(plan: &Plan) -> Vec<String> {
    hosts(
        plan.torrents
            .iter()
            .filter(|t| t.torrent.info.private)
            .flat_map(|t| t.torrent.trackers.iter().flatten()),
    )
}

/// The private tracker hosts to give an identity rule, as `options` chooses; empty when
/// it chooses none.
#[must_use]
pub fn identity_hosts(plan: &Plan, options: &DelugeImportOptions) -> Vec<String> {
    if options.identity == apitorrent_types::ImportIdentity::None {
        return Vec::new();
    }
    let mut out = private_tracker_hosts(plan);
    out.retain(|h| {
        !options
            .identity_skip_hosts
            .iter()
            .any(|s| s.trim().eq_ignore_ascii_case(h))
    });
    out
}

/// Refuse options that can't be carried out: an identity the peer id can't hold.
pub fn check_options(options: &DelugeImportOptions) -> Result<(), String> {
    if let apitorrent_types::ImportIdentity::Other { identity } = &options.identity {
        crate::settings::client_profile(identity)
            .map_err(|e| format!("the identity for private trackers: {e}"))?;
    }
    Ok(())
}

/// The identity those hosts get: Deluge's (asking its daemon for the versions) or the
/// chosen one.
pub async fn import_identity(
    options: &DelugeImportOptions,
    config_dir: &Path,
) -> apitorrent_types::ClientIdentity {
    match &options.identity {
        apitorrent_types::ImportIdentity::Other { identity } => identity.clone(),
        _ => crate::takeover::deluge_identity(config_dir).await,
    }
}

/// Before an import starts any torrent here, pause Deluge's copies (see `takeover`).
/// Returns when they were taken over (`None` when nothing starts). If Deluge runs but can't
/// be made to stop them, everything is imported paused instead, with a warning.
pub async fn take_over_running(
    plan: &mut Plan,
    config_dir: &Path,
    warnings: &mut Vec<String>,
) -> Option<i64> {
    let running = running_ids(plan);
    if running.is_empty() {
        return None;
    }
    match crate::takeover::pause_in_deluge(config_dir, &running).await {
        Ok(_) => Some(now_ms()),
        Err(e) => {
            for t in &mut plan.torrents {
                t.row.paused = true;
            }
            for m in &mut plan.magnets {
                m.row.paused = true;
            }
            warnings.push(format!(
                "{e}. Everything was imported paused; resuming a torrent here tries again to \
                 pause it in Deluge first."
            ));
            None
        }
    }
}

/// Run a future to completion from synchronous code, inside a multi-threaded runtime or
/// outside any.
fn block_on<F: std::future::Future>(f: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(h) => tokio::task::block_in_place(|| h.block_on(f)),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime for the Deluge daemon call")
            .block_on(f),
    }
}

fn rate(bytes_per_sec: Option<u64>) -> String {
    match bytes_per_sec {
        None | Some(0) => "no limit".to_owned(),
        Some(b) if b >= 1024 * 1024 => format!("{:.1} MiB/s", to_f64(b) / 1_048_576.0),
        Some(b) => format!("{:.0} KiB/s", to_f64(b) / 1024.0),
    }
}

#[allow(clippy::cast_precision_loss)]
fn to_f64(v: u64) -> f64 {
    v as f64
}

fn count(n: Option<u32>) -> String {
    n.map_or_else(|| "no limit".to_owned(), |n| n.to_string())
}

fn identity_name(i: &apitorrent_types::ClientIdentity) -> String {
    use apitorrent_types::ClientIdentity as C;
    match i {
        C::Apitorrent => "apitorrent".to_owned(),
        C::Deluge {
            version,
            libtorrent,
        } => format!("Deluge {version} (libtorrent {libtorrent})"),
        C::QBittorrent { version } => format!("qBittorrent {version}"),
        C::Transmission { version } => format!("Transmission {version}"),
        C::Custom { user_agent, .. } => format!("{user_agent} (custom)"),
    }
}

fn ratio(stop: Option<(f64, bool)>) -> String {
    match stop {
        None => "off".to_owned(),
        Some((r, true)) => format!("{r} (then remove)"),
        Some((r, false)) => format!("{r} (then pause)"),
    }
}

/// What the report needs to know besides the plan.
#[derive(Debug, Clone)]
pub struct ReportContext<'a> {
    pub deluge_dir: &'a Path,
    pub slug: &'a str,
    /// The space doesn't exist yet: the import creates it.
    pub creates_space: bool,
    pub roots: &'a [String],
    pub download_dir: &'a Path,
    /// What of each torrent, and of Deluge's settings, the import takes.
    pub options: &'a DelugeImportOptions,
    pub dry_run: bool,
    /// After a real import: how many rows were written.
    pub written: Option<usize>,
    pub failed: &'a [ImportSkip],
    /// How many torrents the filter left out.
    pub not_selected: u32,
    /// Tracker hosts that got (or, in a dry run, would get) a Deluge identity rule.
    pub identity_hosts: &'a [String],
}

/// A plain-text report of a plan, to paste into a chat.
#[allow(clippy::too_many_lines)]
pub fn report(state: &DelugeState, plan: &Plan, c: &ReportContext<'_>) -> String {
    let mut out = String::new();
    let line = "=".repeat(60);
    let _ = writeln!(out, "{line}");
    if c.dry_run {
        let _ = writeln!(
            out,
            "apitorrent Deluge import: DRY RUN (nothing was changed)"
        );
    } else {
        let _ = writeln!(out, "apitorrent Deluge import");
    }
    let _ = writeln!(out, "{line}");
    let _ = writeln!(out, "Deluge config:  {}", c.deluge_dir.display());
    let _ = writeln!(
        out,
        "Into space:     {}{}",
        c.slug,
        if c.creates_space {
            if c.dry_run {
                " (doesn't exist yet: would be created)"
            } else {
                " (created)"
            }
        } else {
            ""
        }
    );
    let _ = writeln!(out, "Default folder: {}", c.download_dir.display());
    if c.not_selected > 0 {
        let _ = writeln!(
            out,
            "Filter:         left out {} torrent{}",
            c.not_selected,
            if c.not_selected == 1 { "" } else { "s" }
        );
    }
    if !c.identity_hosts.is_empty() {
        let _ = writeln!(
            out,
            "Identity:       {} as Deluge to {}",
            if c.dry_run {
                "would present"
            } else {
                "presents"
            },
            c.identity_hosts.join(", ")
        );
    }
    let _ = writeln!(
        out,
        "Storage roots:  {}",
        if c.roots.is_empty() {
            "none (any path)".to_owned()
        } else {
            c.roots.join(", ")
        }
    );

    let with_resume = plan.torrents.iter().filter(|t| t.have.is_some()).count();
    let verb = if c.dry_run {
        "would import:"
    } else {
        "to import:"
    };
    let _ = writeln!(
        out,
        "
COUNTS"
    );
    let _ = writeln!(out, "  torrents in Deluge:       {}", state.torrents.len());
    let _ = writeln!(
        out,
        "  {verb:<26}{} ({with_resume} with progress from Deluge, {} to check)",
        plan.torrents.len(),
        plan.torrents.len() - with_resume,
    );
    let _ = writeln!(out, "  magnets without metadata: {}", plan.magnets.len());
    let _ = writeln!(out, "  already in this space:    {}", plan.already.len());
    let _ = writeln!(out, "  can't import:             {}", plan.skipped.len());
    if let Some(n) = c.written {
        let _ = writeln!(out, "  written:                  {n}");
    }

    // Save paths, each with its storage root.
    let mut paths: BTreeMap<String, usize> = BTreeMap::new();
    for t in &plan.torrents {
        *paths.entry(t.row.save_path.clone()).or_default() += 1;
    }
    for m in &plan.magnets {
        *paths.entry(m.row.save_path.clone()).or_default() += 1;
    }
    let _ = writeln!(out, "\nSAVE PATHS ({})", paths.len());
    for (path, n) in &paths {
        let root = match root_of(c.roots, Path::new(path)) {
            Some("") => "any path allowed".to_owned(),
            Some(r) => format!("in storage root {r}"),
            None => "outside the storage roots".to_owned(),
        };
        let what = if *n == 1 { "torrent" } else { "torrents" };
        let _ = writeln!(out, "  {path}  ({n} {what}, {root})");
    }
    let outside: Vec<&ImportSkip> = plan
        .skipped
        .iter()
        .filter(|s| s.reason.contains("outside"))
        .collect();
    if !outside.is_empty() {
        let _ = writeln!(
            out,
            "  {} more outside the space's storage roots (see CAN'T IMPORT)",
            outside.len()
        );
    }

    let mut labels: BTreeMap<String, usize> = BTreeMap::new();
    for t in &plan.torrents {
        *labels
            .entry(t.label.clone().unwrap_or_else(|| "(none)".to_owned()))
            .or_default() += 1;
    }
    for m in &plan.magnets {
        *labels
            .entry(m.row.label.clone().unwrap_or_else(|| "(none)".to_owned()))
            .or_default() += 1;
    }
    let _ = writeln!(out, "\nLABELS ({})", labels.len());
    for (label, n) in &labels {
        let _ = writeln!(out, "  {label}: {n}");
    }

    let d = &state.settings;
    let _ = writeln!(out, "\nGLOBAL LIMITS (Deluge)");
    let _ = writeln!(
        out,
        "  download {}, upload {}, connections {}",
        rate(d.max_download_rate),
        rate(d.max_upload_rate),
        count(d.max_connections)
    );
    let _ = writeln!(
        out,
        "  active torrents: downloading {}, seeding {}, in all {}; slow ones {}",
        count(d.max_active_downloading),
        count(d.max_active_seeding),
        count(d.max_active_limit),
        if d.dont_count_slow {
            "don't count"
        } else {
            "count"
        }
    );
    let _ = writeln!(out, "  stop seeding at ratio: {}", ratio(d.stop_at_ratio));
    let pick = c.options.settings;
    let default_space = c.slug == apitorrent_types::spaces::DEFAULT_SLUG;
    let taken: Vec<&str> = [
        (pick.download_rate, "download rate"),
        (pick.upload_rate, "upload rate"),
        (pick.max_connections, "connections"),
        (pick.queue_slots, "active slots"),
        (pick.ignore_slow && default_space, "slow torrents"),
        (pick.stop_at_ratio, "stop at ratio"),
    ]
    .into_iter()
    .filter_map(|(on, name)| on.then_some(name))
    .collect();
    let where_to = if taken.is_empty() {
        "not taken (pass --settings, or --setting NAME, to take them)".to_owned()
    } else if default_space {
        format!("{} become the instance's settings", taken.join(", "))
    } else if pick.stop_at_ratio {
        format!(
            "{} become space {}'s; the stop-at-ratio becomes each torrent's own seed limits",
            taken.join(", "),
            c.slug
        )
    } else {
        format!("{} become space {}'s", taken.join(", "), c.slug)
    };
    let _ = writeln!(out, "  -> {where_to}");

    let o = c.options;
    let keep = |on: bool| if on { "kept" } else { "left out" };
    let _ = writeln!(out, "\nPER-TORRENT OPTIONS");
    let renames: Vec<String> = o
        .label_renames
        .iter()
        .map(
            |r| match r.to.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                Some(to) => format!("{} -> {to}", r.from),
                None => format!("{} dropped", r.from),
            },
        )
        .collect();
    let renamed = if o.labels && !renames.is_empty() {
        format!(" ({})", renames.join(", "))
    } else {
        String::new()
    };
    let _ = writeln!(out, "  labels:            {}{renamed}", keep(o.labels));
    let _ = writeln!(
        out,
        "  queue order:       {}",
        if o.queue_order {
            "Deluge's"
        } else {
            "by date added"
        }
    );
    let _ = writeln!(out, "  added dates:       {}", keep(o.added_dates));
    let _ = writeln!(out, "  totals, seed time: {}", keep(o.totals));
    let _ = writeln!(
        out,
        "  skipped files:     {}",
        if o.skipped_files {
            "stay skipped"
        } else {
            "downloaded"
        }
    );
    let _ = writeln!(
        out,
        "  trackers:          {}",
        if o.trackers {
            "Deluge's lists"
        } else {
            "each .torrent's own"
        }
    );
    let _ = writeln!(out, "  speed limits:      {}", keep(o.torrent_limits));
    let _ = writeln!(out, "  stop at ratio:     {}", keep(o.seed_limits));
    let identity = match &o.identity {
        apitorrent_types::ImportIdentity::Deluge => "present as Deluge".to_owned(),
        apitorrent_types::ImportIdentity::None => "no identity rules".to_owned(),
        apitorrent_types::ImportIdentity::Other { identity } => {
            format!("present as {}", identity_name(identity))
        }
    };
    let _ = writeln!(out, "  private trackers:  {identity}");

    let limited: Vec<&PlannedTorrent> = plan
        .torrents
        .iter()
        .filter(|t| {
            t.limits != TorrentLimits::default()
                || (t.seed.limits.is_some() && t.seed.limits != Some(seed_limits(d.stop_at_ratio)))
        })
        .collect();
    let _ = writeln!(out, "\nPER-TORRENT LIMITS ({})", limited.len());
    for t in &limited {
        let mut parts = Vec::new();
        if t.limits.download_rate > 0 {
            parts.push(format!("down {}", rate(Some(t.limits.download_rate))));
        }
        if t.limits.upload_rate > 0 {
            parts.push(format!("up {}", rate(Some(t.limits.upload_rate))));
        }
        if let Some(n) = t.limits.max_connections {
            parts.push(format!("{n} connections"));
        }
        if let Some(l) = t.seed.limits
            && l.active()
        {
            parts.push(format!("stop at ratio {}", l.ratio));
        }
        let _ = writeln!(out, "  {}: {}", t.name, parts.join(", "));
    }

    let notes: Vec<(String, &String)> = plan
        .torrents
        .iter()
        .flat_map(|t| t.not_carried.iter().map(move |n| (t.name.clone(), n)))
        .collect();
    let problems = plan.skipped.len() + c.failed.len() + notes.len() + state.label_options.len();
    let _ = writeln!(out, "\nCAN'T IMPORT OR CARRY OVER ({problems})");
    for s in plan.skipped.iter().chain(c.failed) {
        let _ = writeln!(out, "  skipped {}: {}", s.name, s.reason);
    }
    for (name, n) in &notes {
        let _ = writeln!(out, "  {name}: {n}");
    }
    for n in &state.label_options {
        let _ = writeln!(out, "  {n}");
    }

    if !plan.already.is_empty() {
        let _ = writeln!(
            out,
            "\nALREADY IN THIS SPACE ({}, left as they are)",
            plan.already.len()
        );
        for name in &plan.already {
            let _ = writeln!(out, "  {name}");
        }
    }
    if !state.warnings.is_empty() {
        let _ = writeln!(out, "\nWARNINGS ({})", state.warnings.len());
        for w in &state.warnings {
            let _ = writeln!(out, "  {w}");
        }
    }
    let _ = writeln!(out, "\n{line}");
    if c.dry_run {
        let _ = writeln!(out, "Run again without --dry-run to import.");
    } else if c.written.is_some() {
        let _ = writeln!(
            out,
            "Start (or restart) apitorrentd to start them; progress from Deluge is checked \
             against the files, not rehashed."
        );
    }
    let _ = writeln!(out, "{line}");
    out
}

/// The folder a space's torrents go to by default, as the running core decides it (see
/// `Core::download_dir`).
pub fn default_folder(
    space: SpaceId,
    slug: &str,
    settings: &SpaceSettings,
    instance: &Path,
) -> PathBuf {
    if let Some(d) = &settings.download_dir {
        return PathBuf::from(d);
    }
    if space == apitorrent_types::spaces::DEFAULT_SPACE {
        return instance.to_path_buf();
    }
    settings
        .storage_roots
        .first()
        .map_or_else(|| instance.join(slug), PathBuf::from)
}

/// What `apitorrentd import deluge` asks for.
#[derive(Debug, Clone)]
pub struct OfflineImport {
    /// Deluge's config directory.
    pub config_dir: PathBuf,
    /// The space to import into, created if missing.
    pub space: String,
    /// Report only; change nothing.
    pub dry_run: bool,
    /// Save path prefixes to rewrite.
    pub map: Vec<(String, String)>,
    pub paused: bool,
    /// What of each torrent, and of Deluge's settings, comes over.
    pub options: DelugeImportOptions,
    /// Which torrents to take.
    pub filter: DelugeImportFilter,
}

/// Import into the instance in `data_dir` without a running daemon: rows the daemon starts
/// on its next start. A dry run opens nothing for writing. Returns the plain-text report.
pub fn run_offline(
    data_dir: &Path,
    instance_download_dir: &Path,
    req: &OfflineImport,
) -> Result<String, String> {
    use apitorrent_types::spaces::{DEFAULT_SLUG, DEFAULT_SPACE, valid_slug};
    if !valid_slug(&req.space) {
        return Err(format!(
            "{:?} isn't a space slug: 1 to 40 lowercase letters, digits and dashes",
            req.space
        ));
    }
    check_options(&req.options)?;
    let state = apitorrent_deluge::load(&req.config_dir).map_err(|e| e.to_string())?;
    let db_path = data_dir.join("apitorrent.db");
    let mut db = if req.dry_run {
        Database::open_read_only(&db_path).map_err(|e| format!("{}: {e}", db_path.display()))?
    } else {
        std::fs::create_dir_all(data_dir).map_err(|e| format!("{}: {e}", data_dir.display()))?;
        Some(Database::open(&db_path).map_err(|e| format!("{}: {e}", db_path.display()))?)
    };
    let mut warnings = Vec::new();
    if req.dry_run && db.is_none() {
        warnings.push(format!(
            "{} doesn't exist yet or is from another version of apitorrent: the report \
             assumes the space is empty",
            db_path.display()
        ));
    }
    let spaces = match &db {
        Some(db) => db.spaces().map_err(|e| e.to_string())?,
        None => Vec::new(),
    };
    let existing = spaces.iter().find(|s| s.slug == req.space).cloned();
    let creates_space = existing.is_none() && req.space != DEFAULT_SLUG;
    // A new space keeps Deluge's default folder as its own.
    let mut settings = existing.as_ref().map_or_else(
        || SpaceSettings {
            download_dir: state
                .settings
                .download_location
                .as_deref()
                .map(|p| import::remap(p, &req.map))
                .filter(|p| Path::new(p).is_absolute()),
            ..SpaceSettings::default()
        },
        |s| s.settings.clone(),
    );
    let space_id = match (&existing, req.space == DEFAULT_SLUG) {
        (Some(s), _) => s.id,
        (None, true) => DEFAULT_SPACE,
        (None, false) => match (&db, req.dry_run) {
            (Some(db), false) => db
                .insert_space(&req.space, &req.space, "", &settings, now_ms())
                .map_err(|e| format!("could not create space {}: {e}", req.space))?,
            // Not created in a dry run; any id will do for the plan.
            _ => SpaceId::MAX,
        },
    };
    if req.options.settings.any()
        && !req.dry_run
        && let Some(db) = &db
    {
        if space_id == DEFAULT_SPACE {
            let mut s: Settings = db
                .meta(crate::SETTINGS_KEY)
                .ok()
                .flatten()
                .and_then(|j| serde_json::from_str(&j).ok())
                .unwrap_or_else(|| {
                    crate::settings::defaults(&apitorrent_engine::SessionConfig::default())
                });
            apply_instance_settings(&mut s, &state.settings, req.options.settings);
            let json = serde_json::to_string(&s).expect("settings serialize");
            db.set_meta(crate::SETTINGS_KEY, &json)
                .map_err(|e| e.to_string())?;
        } else {
            apply_space_settings(&mut settings, &state.settings, req.options.settings);
            let name = existing
                .as_ref()
                .map_or(req.space.as_str(), |s| s.name.as_str());
            let description = existing.as_ref().map_or("", |s| s.description.as_str());
            db.update_space(space_id, name, description, &settings)
                .map_err(|e| e.to_string())?;
        }
    }
    let present = match &db {
        Some(db) => db.torrent_ids(space_id).map_err(|e| e.to_string())?,
        None => HashSet::new(),
    };
    let first_position = match &db {
        Some(db) => db
            .next_queue_position(space_id)
            .map_err(|e| e.to_string())?,
        None => 0,
    };
    let download_dir = default_folder(space_id, &req.space, &settings, instance_download_dir);
    let target = Target {
        space: space_id,
        slug: &req.space,
        roots: &settings.storage_roots,
        download_dir: &download_dir,
        present: &present,
        map: &req.map,
        paused: req.paused,
        ratio_per_torrent: space_id != DEFAULT_SPACE && req.options.settings.stop_at_ratio,
        options: &req.options,
        first_position,
    };
    let mut state = state;
    let not_selected = select(&mut state, &req.filter, &req.map, &download_dir);
    let mut plan = plan(&state, &target);
    let private_hosts = identity_hosts(&plan, &req.options);
    let mut identity_hosts = Vec::new();
    let (written, failed) = if let (Some(db), false) = (&mut db, req.dry_run) {
        let taken_over_at = block_on(take_over_running(&mut plan, &req.config_dir, &mut warnings));
        let (n, failed) = write(db, &plan, &req.config_dir.to_string_lossy(), taken_over_at);
        if !private_hosts.is_empty() {
            let identity = block_on(import_identity(&req.options, &req.config_dir));
            match add_identity_rules(db, &private_hosts, &identity) {
                Ok(added) => identity_hosts = added,
                Err(e) => warnings.push(format!("couldn't add client identity rules: {e}")),
            }
        }
        (Some(n), failed)
    } else {
        identity_hosts.clone_from(&private_hosts);
        let n = running_ids(&plan).len();
        if n > 0 {
            warnings.push(format!(
                "{n} torrent(s) would start running: the import first pauses them in \
                 Deluge, if its daemon is running"
            ));
        }
        (None, Vec::new())
    };
    state.warnings.extend(warnings);
    Ok(report(
        &state,
        &plan,
        &ReportContext {
            deluge_dir: &req.config_dir,
            slug: &req.space,
            creates_space,
            roots: &settings.storage_roots,
            download_dir: &download_dir,
            options: &req.options,
            dry_run: req.dry_run,
            written,
            failed: &failed,
            not_selected,
            identity_hosts: &identity_hosts,
        },
    ))
}

/// Add Deluge identity rules for `hosts` to the instance settings stored in `db` (for the
/// offline import; the daemon does this through `Core::ensure_tracker_identities`).
fn add_identity_rules(
    db: &Database,
    hosts: &[String],
    identity: &apitorrent_types::ClientIdentity,
) -> Result<Vec<String>, String> {
    let mut s: Settings = db
        .meta(crate::SETTINGS_KEY)
        .map_err(|e| e.to_string())?
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_else(|| crate::settings::defaults(&apitorrent_engine::SessionConfig::default()));
    let added = crate::settings::ensure_tracker_identities(&mut s.identity, hosts, identity);
    if !added.is_empty() {
        let json = serde_json::to_string(&s).expect("settings serialize");
        db.set_meta(crate::SETTINGS_KEY, &json)
            .map_err(|e| e.to_string())?;
    }
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary() -> Summary {
        Summary {
            id: "aa".into(),
            deluge_id: "aa".into(),
            name: "Some.Show.S01E02.1080p".into(),
            label: Some("TV".into()),
            hosts: vec!["tracker.example.org".into(), "udp.other.net".into()],
            save_path: PathBuf::from("/srv/torrents/tv"),
            size: 10,
            progress: Some(1.0),
            paused: false,
            leftover: false,
            error: None,
            details: Details::default(),
        }
    }

    fn f() -> DelugeImportFilter {
        DelugeImportFilter::default()
    }

    #[test]
    fn filters() {
        let s = summary();
        assert!(matches(&s, &f()));
        assert!(matches(
            &s,
            &DelugeImportFilter {
                labels: vec!["tv".into()],
                ..f()
            }
        ));
        assert!(!matches(
            &s,
            &DelugeImportFilter {
                labels: vec![String::new()],
                ..f()
            }
        ));
        assert!(!matches(
            &s,
            &DelugeImportFilter {
                exclude_labels: vec!["Tv".into()],
                ..f()
            }
        ));
        assert!(matches(
            &s,
            &DelugeImportFilter {
                trackers: vec!["example.org".into()],
                ..f()
            }
        ));
        assert!(!matches(
            &s,
            &DelugeImportFilter {
                trackers: vec!["ample.org".into()],
                ..f()
            }
        ));
        assert!(matches(
            &s,
            &DelugeImportFilter {
                save_paths: vec!["/srv/torrents".into()],
                ..f()
            }
        ));
        assert!(!matches(
            &s,
            &DelugeImportFilter {
                save_paths: vec!["/srv/tor".into()],
                ..f()
            }
        ));
        assert!(matches(
            &s,
            &DelugeImportFilter {
                complete: Some(true),
                paused: Some(false),
                ..f()
            }
        ));
        assert!(!matches(
            &s,
            &DelugeImportFilter {
                complete: Some(false),
                ..f()
            }
        ));
        assert!(!matches(
            &s,
            &DelugeImportFilter {
                paused: Some(true),
                ..f()
            }
        ));
        assert!(matches(
            &s,
            &DelugeImportFilter {
                name: Some("show s01e02".into()),
                ..f()
            }
        ));
        assert!(!matches(
            &s,
            &DelugeImportFilter {
                name: Some("show s02".into()),
                ..f()
            }
        ));
        assert!(matches(
            &s,
            &DelugeImportFilter {
                ids: vec!["AA".into()],
                ..f()
            }
        ));
        assert!(!matches(
            &s,
            &DelugeImportFilter {
                ids: vec!["bb".into()],
                ..f()
            }
        ));
        // Kinds combine with "and".
        assert!(!matches(
            &s,
            &DelugeImportFilter {
                labels: vec!["tv".into()],
                complete: Some(false),
                ..f()
            }
        ));
        let unknown = Summary {
            progress: None,
            ..summary()
        };
        assert!(!matches(
            &unknown,
            &DelugeImportFilter {
                complete: Some(true),
                ..f()
            }
        ));
    }

    #[test]
    fn hosts_and_slugs() {
        assert_eq!(
            tracker_host("https://Tracker.Example.org:443/abc/announce").as_deref(),
            Some("tracker.example.org")
        );
        assert_eq!(tracker_host("udp://t.net:1337").as_deref(), Some("t.net"));
        assert_eq!(tracker_host("not a url"), None);
        assert_eq!(
            suggested_slug(Path::new("/home/u/.config/deluge")),
            "deluge"
        );
        assert_eq!(suggested_slug(Path::new("/srv/Deluge TV_2")), "deluge-tv-2");
        assert_eq!(suggested_slug(Path::new("/srv/...")), "deluge");
    }

    /// A Deluge state with two private torrents: `b` added first but queued second, `a`
    /// labelled "tv" with limits, totals, an edited tracker and a skipped file.
    fn two_torrents() -> DelugeState {
        use apitorrent_metainfo::{FileSource, TorrentBuilder, Version};
        let make = |name: &str, host: &str| {
            let bytes = TorrentBuilder::new(name, Version::V1)
                .piece_length(16 * 1024)
                .file(FileSource::from_bytes(&["one"], vec![1u8; 100]))
                .file(FileSource::from_bytes(&["two"], vec![2u8; 100]))
                .tracker_tier(vec![format!("https://{host}/announce")])
                .private(true)
                .build()
                .unwrap();
            let id = Torrent::from_bytes(&bytes)
                .unwrap()
                .info
                .info_hashes()
                .get_v1()
                .unwrap()
                .to_hex();
            apitorrent_deluge::DelugeTorrent {
                id,
                torrent: bytes,
                save_path: Some("/srv/t".into()),
                paused: false,
                label: None,
                queue: None,
                file_priorities: None,
                pieces: None,
                renamed: std::collections::HashMap::new(),
                added_at: None,
                leftover: false,
                total_downloaded: 0,
                total_uploaded: 0,
                max_download_rate: None,
                max_upload_rate: None,
                max_connections: None,
                trackers: None,
                magnet: None,
                stop_at_ratio: None,
                seeding_secs: 0,
                not_carried: Vec::new(),
            }
        };
        let mut a = make("a", "a.example.org");
        a.label = Some("TV".into());
        a.queue = Some(0);
        a.added_at = Some(2_000);
        a.file_priorities = Some(vec![4, 0]);
        a.total_downloaded = 10;
        a.total_uploaded = 20;
        a.seeding_secs = 30;
        a.max_upload_rate = Some(1024);
        a.stop_at_ratio = Some((3.0, false));
        a.trackers = Some(vec![vec!["https://new.example.org/announce".into()]]);
        let mut b = make("b", "b.example.org");
        b.queue = Some(1);
        b.added_at = Some(1_000);
        DelugeState {
            torrents: vec![a, b],
            ..DelugeState::default()
        }
    }

    fn plan_with(state: &DelugeState, options: &DelugeImportOptions) -> Plan {
        let present = HashSet::new();
        plan(
            state,
            &Target {
                space: 1,
                slug: "tv",
                roots: &[],
                download_dir: Path::new("/dl"),
                present: &present,
                map: &[],
                paused: false,
                ratio_per_torrent: false,
                options,
                first_position: 0,
            },
        )
    }

    /// A `.torrent` file Deluge left but doesn't list comes over paused, keeps its file's
    /// date, and is recorded as a leftover with nothing to take over.
    #[test]
    fn leftovers_come_over_paused() {
        let mut state = two_torrents();
        state.torrents[1].leftover = true;
        let summaries = summarize(&state, &[], Path::new("/dl"));
        assert!(summaries[1].leftover && summaries[1].paused);
        assert!(!summaries[0].leftover);

        let p = plan_with(&state, &DelugeImportOptions::default());
        let b = p.torrents.iter().find(|t| t.name == "b").unwrap();
        assert!(b.leftover && b.row.paused);
        assert_eq!(b.row.added_at, 1_000_000);
        let r = imported_row(&b.row, &b.deluge_id, "/deluge", None, true);
        assert!(r.leftover);
        assert!(
            r.taken_over_at.is_some(),
            "Deluge never had it: nothing to take over"
        );

        let a = p.torrents.iter().find(|t| t.name == "a").unwrap();
        assert!(!a.leftover && !a.row.paused);
        let r = imported_row(&a.row, &a.deluge_id, "/deluge", None, false);
        assert!(!r.leftover && r.taken_over_at.is_none());
    }

    #[test]
    fn everything_comes_over_by_default() {
        let state = two_torrents();
        let p = plan_with(&state, &DelugeImportOptions::default());
        let a = &p.torrents[0];
        assert_eq!(a.name, "a", "Deluge's queue order");
        assert_eq!(a.row.queue_position, 0);
        assert_eq!(a.label.as_deref(), Some("TV"));
        assert_eq!(a.row.added_at, 2_000_000);
        assert_eq!((a.row.downloaded_total, a.row.uploaded_total), (10, 20));
        assert_eq!(a.seed.seed_ms, 30_000);
        assert_eq!(a.row.file_priorities.as_deref(), Some(&[4, 0][..]));
        assert_eq!(a.limits.upload_rate, 1024);
        assert_eq!(a.seed.limits.map(|l| l.ratio), Some(3.0));
        assert_eq!(a.torrent.trackers, [["https://new.example.org/announce"]]);
        assert_eq!(
            identity_hosts(&p, &DelugeImportOptions::default()),
            ["b.example.org", "new.example.org"]
        );

        // The preview sees the same options.
        let s = summarize(&state, &[], Path::new("/dl"));
        assert!(s[0].details.private);
        assert!(s[0].details.edited_trackers);
        assert!(!s[1].details.edited_trackers);
        assert_eq!(s[0].details.skipped_files, 1);
        assert_eq!(s[0].details.stop_at_ratio, Some((3.0, false)));
    }

    #[test]
    fn options_leave_things_out() {
        let state = two_torrents();
        let o = DelugeImportOptions {
            labels: true,
            label_renames: vec![apitorrent_types::LabelRename {
                from: "tv".into(),
                to: Some("Shows".into()),
            }],
            queue_order: false,
            added_dates: false,
            totals: false,
            skipped_files: false,
            trackers: false,
            torrent_limits: false,
            seed_limits: false,
            identity: apitorrent_types::ImportIdentity::Deluge,
            identity_skip_hosts: vec!["B.example.org".into()],
            settings: DelugeSettingsPick::default(),
        };
        let p = plan_with(&state, &o);
        assert_eq!(p.torrents[0].name, "b", "in the order they were added");
        let a = &p.torrents[1];
        assert_eq!(a.row.queue_position, 1);
        assert_eq!(a.label.as_deref(), Some("Shows"));
        assert_eq!(a.row.label.as_deref(), Some("Shows"));
        assert!(a.row.added_at > 2_000_000);
        assert_eq!((a.row.downloaded_total, a.row.uploaded_total), (0, 0));
        assert_eq!(a.seed.seed_ms, 0);
        assert_eq!(a.row.file_priorities.as_deref(), Some(&[4, 4][..]));
        assert_eq!(a.limits, TorrentLimits::default());
        assert_eq!(a.seed.limits, None);
        assert_eq!(a.torrent.trackers, [["https://a.example.org/announce"]]);
        assert_eq!(identity_hosts(&p, &o), ["a.example.org"]);

        let none = DelugeImportOptions {
            labels: false,
            identity: apitorrent_types::ImportIdentity::None,
            ..o
        };
        let p = plan_with(&state, &none);
        assert_eq!(p.torrents[1].label, None);
        assert!(identity_hosts(&p, &none).is_empty());
    }

    #[test]
    fn labels_dropped_or_renamed() {
        let o = DelugeImportOptions {
            label_renames: vec![
                apitorrent_types::LabelRename {
                    from: "Junk".into(),
                    to: None,
                },
                apitorrent_types::LabelRename {
                    from: "old".into(),
                    to: Some("  ".into()),
                },
            ],
            ..DelugeImportOptions::default()
        };
        assert_eq!(o.label_for(Some("junk")), None);
        assert_eq!(o.label_for(Some("old")), None);
        assert_eq!(o.label_for(Some("tv")).as_deref(), Some("tv"));
        assert_eq!(o.label_for(None), None);
    }

    #[test]
    fn settings_taken_one_by_one() {
        let d = DelugeSettings {
            max_download_rate: Some(100),
            max_upload_rate: Some(200),
            max_connections: Some(50),
            max_active_downloading: Some(3),
            ..DelugeSettings::default()
        };
        let mut s = SpaceSettings::default();
        apply_space_settings(
            &mut s,
            &d,
            DelugeSettingsPick {
                upload_rate: true,
                ..DelugeSettingsPick::default()
            },
        );
        assert_eq!(
            (s.download_rate, s.upload_rate, s.max_connections),
            (0, 200, 0)
        );
        apply_space_settings(&mut s, &d, DelugeSettingsPick::all());
        assert_eq!(
            (s.download_rate, s.max_connections, s.max_active_downloads),
            (100, 50, 3)
        );
    }
}

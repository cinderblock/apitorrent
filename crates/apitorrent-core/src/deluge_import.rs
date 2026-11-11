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
use apitorrent_types::{ImportSkip, SeedLimitAction, SeedLimits, Settings, TorrentLimits};

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
    /// The queue position of the first imported torrent.
    pub first_position: u32,
}

/// One torrent to add.
#[derive(Debug, Clone)]
pub struct PlannedTorrent {
    pub name: String,
    pub torrent: Arc<Torrent>,
    pub row: TorrentRow,
    pub have: Option<Bitfield>,
    pub file_paths: Option<Vec<Vec<String>>>,
    pub limits: TorrentLimits,
    pub seed: SeedState,
    pub label: Option<String>,
    /// Deluge options that don't carry over.
    pub not_carried: Vec<String>,
}

/// A magnet link Deluge never got the metadata for.
#[derive(Debug, Clone)]
pub struct PlannedMagnet {
    pub name: String,
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

/// Deluge's settings applied to the instance's (for the `default` space).
pub fn apply_instance_settings(s: &mut Settings, d: &DelugeSettings) {
    s.download_rate = d.max_download_rate.unwrap_or(0);
    s.upload_rate = d.max_upload_rate.unwrap_or(0);
    if let Some(n) = d.max_connections {
        s.max_connections = n;
    }
    s.queue.max_active_downloads = d.max_active_downloading.unwrap_or(0);
    s.queue.max_active_seeds = d.max_active_seeding.unwrap_or(0);
    s.queue.max_active = d.max_active_limit.unwrap_or(0);
    s.queue.ignore_slow = d.dont_count_slow;
    s.seed_limits = seed_limits(d.stop_at_ratio);
}

/// Deluge's settings applied to another space's: its rates, connections and slots.
pub fn apply_space_settings(s: &mut SpaceSettings, d: &DelugeSettings) {
    s.download_rate = d.max_download_rate.unwrap_or(0);
    s.upload_rate = d.max_upload_rate.unwrap_or(0);
    s.max_connections = d.max_connections.unwrap_or(0);
    s.max_active_downloads = d.max_active_downloading.unwrap_or(0);
    s.max_active_seeds = d.max_active_seeding.unwrap_or(0);
    s.max_active = d.max_active_limit.unwrap_or(0);
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

/// Plan importing what Deluge has into `t`'s space.
pub fn plan(state: &DelugeState, t: &Target<'_>) -> Plan {
    let mut out = Plan::default();
    let mut position = t.first_position;
    let mut seen: HashSet<String> = HashSet::new();
    for d in &state.torrents {
        let bytes = match &d.trackers {
            Some(tiers) => {
                import::with_trackers(&d.torrent, tiers).unwrap_or_else(|| d.torrent.clone())
            }
            None => d.torrent.clone(),
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
                    label: d.label.clone(),
                    added_at: d.added_at.map_or_else(now_ms, |s| s.saturating_mul(1000)),
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
        let prios = d
            .file_priorities
            .clone()
            .filter(|p| p.len() == files)
            .unwrap_or_else(|| vec![4; files]);
        let pieces = u32::try_from(torrent.info.piece_count()).unwrap_or(0);
        let have = d
            .pieces
            .as_deref()
            .and_then(|p| import::pieces_bitfield(p, pieces));
        let file_paths = import::file_layout(&torrent, &save_path, &d.renamed);
        let limits = TorrentLimits {
            download_rate: d.max_download_rate.unwrap_or(0),
            upload_rate: d.max_upload_rate.unwrap_or(0),
            max_connections: d.max_connections,
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
            label: d.label.clone(),
            added_at: d.added_at.map_or_else(now_ms, |s| s.saturating_mul(1000)),
            paused: t.paused || d.paused,
            have: have.as_ref().map(|h| h.as_bytes().to_vec()),
            file_priorities: Some(prios),
            downloaded_total: d.total_downloaded,
            uploaded_total: d.total_uploaded,
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
        // that differs from the global setting gets limits of its own (or every one, where
        // the global setting has nowhere to go).
        let own = (t.ratio_per_torrent || d.stop_at_ratio != state.settings.stop_at_ratio)
            .then(|| seed_limits(d.stop_at_ratio));
        out.torrents.push(PlannedTorrent {
            name,
            torrent,
            row,
            have,
            file_paths,
            limits,
            seed: SeedState {
                seed_ms: d.seeding_secs.saturating_mul(1000),
                limits: own,
                reached: false,
            },
            label: d.label.clone(),
            not_carried: d.not_carried.clone(),
        });
    }
    out
}

/// Store a plan's torrents for the daemon to start on its next start. Returns how many
/// were written; one that fails is skipped with its reason.
pub fn write(db: &mut Database, plan: &Plan) -> (usize, Vec<ImportSkip>) {
    let mut written = 0;
    let mut failed = Vec::new();
    for t in &plan.torrents {
        match db.insert_torrent(&t.row) {
            Ok(()) => {
                let key = Key::new(t.row.space, t.row.id.clone());
                if let Err(e) = db.save_seed_states(&[(key, t.seed)]) {
                    tracing::warn!(torrent = %t.name, error = %e, "could not store seeding time");
                }
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
            Ok(()) => written += 1,
            Err(e) => failed.push(ImportSkip {
                name: m.name.clone(),
                reason: e.to_string(),
            }),
        }
    }
    (written, failed)
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
    pub import_settings: bool,
    pub dry_run: bool,
    /// After a real import: how many rows were written.
    pub written: Option<usize>,
    pub failed: &'a [ImportSkip],
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
    let where_to = if !c.import_settings {
        "not taken (pass --settings to take them)".to_owned()
    } else if c.slug == apitorrent_types::spaces::DEFAULT_SLUG {
        "become the instance's settings".to_owned()
    } else {
        format!(
            "rates, connections and slots become space {}'s; the stop-at-ratio becomes \
             each torrent's own seed limits",
            c.slug
        )
    };
    let _ = writeln!(out, "  -> {where_to}");

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
    /// Take Deluge's limits too.
    pub import_settings: bool,
    pub paused: bool,
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
    if req.import_settings
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
            apply_instance_settings(&mut s, &state.settings);
            let json = serde_json::to_string(&s).expect("settings serialize");
            db.set_meta(crate::SETTINGS_KEY, &json)
                .map_err(|e| e.to_string())?;
        } else {
            apply_space_settings(&mut settings, &state.settings);
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
        ratio_per_torrent: space_id != DEFAULT_SPACE && req.import_settings,
        first_position,
    };
    let plan = plan(&state, &target);
    let (written, failed) = match (&mut db, req.dry_run) {
        (Some(db), false) => {
            let (n, failed) = write(db, &plan);
            (Some(n), failed)
        }
        _ => (None, Vec::new()),
    };
    let mut state = state;
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
            import_settings: req.import_settings,
            dry_run: req.dry_run,
            written,
            failed: &failed,
        },
    ))
}

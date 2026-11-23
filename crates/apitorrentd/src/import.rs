//! `apitorrentd import deluge`: bring a Deluge installation into a space, without the
//! daemon running. The torrents are written to the database and start on the daemon's next
//! start; `--dry-run` only prints what would happen. The report is plain text, made to be
//! copied out of a terminal.

use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};

#[derive(Debug, Subcommand)]
pub enum ImportCommand {
    /// Import a Deluge 2 installation's torrents (see `docs/architecture.md`).
    Deluge(DelugeArgs),
}

#[derive(Debug, Args)]
pub struct DelugeArgs {
    /// Deluge's config directory, the one holding `state/` (`~/.config/deluge`).
    #[arg(long)]
    config: PathBuf,
    /// The space to import into; it is created if it doesn't exist.
    #[arg(long, default_value = "default")]
    space: String,
    /// Print what would be imported, and change nothing.
    #[arg(long)]
    dry_run: bool,
    /// Rewrite save paths starting with FROM to start with TO instead (Deluge ran with its
    /// downloads mounted elsewhere). Repeatable: `--map /downloads=/srv/torrents`.
    #[arg(long = "map", value_name = "FROM=TO")]
    map: Vec<String>,
    /// Also take all of Deluge's speed, connection and queue limits and its stop-at-ratio
    /// (into the space's settings, or the instance's for `default`), and its proxy (for
    /// `default`).
    #[arg(long)]
    settings: bool,
    /// Take one of Deluge's settings. Repeatable.
    #[arg(long = "setting", value_enum, value_name = "NAME")]
    setting: Vec<SettingName>,
    /// Add everything paused, whatever its state in Deluge.
    #[arg(long)]
    paused: bool,
    #[command(flatten)]
    filter: FilterArgs,
    #[command(flatten)]
    options: OptionArgs,
}

/// One of Deluge's settings, for `--setting`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum SettingName {
    DownloadRate,
    UploadRate,
    Connections,
    /// Active downloads, seeds and torrents in all.
    Slots,
    /// Don't count slow torrents against the slots (the default space only).
    SlowTorrents,
    StopAtRatio,
    /// Encryption for peer connections (the default space only).
    Encryption,
    /// DHT, local peer discovery, UPnP and NAT-PMP (the default space only).
    Discovery,
    /// The proxy (the default space only).
    Proxy,
}

/// What of each torrent comes over. Everything does unless a flag leaves it out.
#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools)]
struct OptionArgs {
    /// Leave Deluge's labels behind.
    #[arg(long)]
    no_labels: bool,
    /// Give torrents labelled FROM in Deluge the label TO instead (no label when TO is
    /// empty). Repeatable: `--rename-label tv=TV --rename-label junk=`.
    #[arg(long = "rename-label", value_name = "FROM=TO")]
    rename_label: Vec<String>,
    /// Queue them in the order they were added, not Deluge's queue order.
    #[arg(long)]
    no_queue_order: bool,
    /// Date them now, not when they were added to Deluge.
    #[arg(long)]
    no_added_dates: bool,
    /// Start downloaded and uploaded totals and seeding time at zero.
    #[arg(long)]
    no_totals: bool,
    /// Download the files Deluge skips.
    #[arg(long)]
    download_skipped: bool,
    /// Use each .torrent file's own trackers, not Deluge's (possibly edited) lists.
    #[arg(long)]
    original_trackers: bool,
    /// Leave each torrent's own speed and connection limits behind.
    #[arg(long)]
    no_limits: bool,
    /// Leave each torrent's own stop-at-ratio behind.
    #[arg(long)]
    no_seed_limits: bool,
    /// Leave Deluge's "move completed" behind: files stay where they download.
    #[arg(long)]
    no_move_completed: bool,
    /// Leave Deluge's sequential download behind: rarest pieces first, as usual.
    #[arg(long)]
    no_sequential: bool,
    /// Leave Deluge's "prioritize first and last pieces" behind.
    #[arg(long)]
    no_first_last: bool,
    /// Leave Deluge's super-seeding behind.
    #[arg(long)]
    no_super_seed: bool,
    /// The client private trackers see for these torrents: `none` (apitorrent's own;
    /// adds no rules), or another client to present as: `deluge` (as before),
    /// `qbittorrent` or `transmission`.
    #[arg(long, value_enum, default_value = "none")]
    identity: IdentityName,
    /// Add no identity rule for this tracker host. Repeatable.
    #[arg(long = "no-identity-for", value_name = "HOST")]
    no_identity_for: Vec<String>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum IdentityName {
    Deluge,
    None,
    Qbittorrent,
    Transmission,
}

impl DelugeArgs {
    fn options(&self) -> Result<apitorrent_types::DelugeImportOptions, String> {
        use apitorrent_types::{ClientIdentity, DelugeSettingsPick, ImportIdentity, LabelRename};
        let o = &self.options;
        let mut settings = if self.settings {
            DelugeSettingsPick::all()
        } else {
            DelugeSettingsPick::default()
        };
        for s in &self.setting {
            match s {
                SettingName::DownloadRate => settings.download_rate = true,
                SettingName::UploadRate => settings.upload_rate = true,
                SettingName::Connections => settings.max_connections = true,
                SettingName::Slots => settings.queue_slots = true,
                SettingName::SlowTorrents => settings.ignore_slow = true,
                SettingName::StopAtRatio => settings.stop_at_ratio = true,
                SettingName::Encryption => settings.encryption = true,
                SettingName::Discovery => settings.discovery = true,
                SettingName::Proxy => settings.proxy = true,
            }
        }
        let label_renames = o
            .rename_label
            .iter()
            .map(|r| {
                r.split_once('=')
                    .map(|(from, to)| LabelRename {
                        from: from.to_owned(),
                        to: Some(to.to_owned()).filter(|t| !t.trim().is_empty()),
                    })
                    .ok_or_else(|| format!("--rename-label takes FROM=TO, not {r:?}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let identity = match o.identity {
            IdentityName::Deluge => ImportIdentity::Deluge,
            IdentityName::None => ImportIdentity::None,
            IdentityName::Qbittorrent => ImportIdentity::Other {
                identity: ClientIdentity::QBittorrent {
                    version: "4.6.5".into(),
                },
            },
            IdentityName::Transmission => ImportIdentity::Other {
                identity: ClientIdentity::Transmission {
                    version: "4.0.6".into(),
                },
            },
        };
        Ok(apitorrent_types::DelugeImportOptions {
            labels: !o.no_labels,
            label_renames,
            queue_order: !o.no_queue_order,
            added_dates: !o.no_added_dates,
            totals: !o.no_totals,
            skipped_files: !o.download_skipped,
            trackers: !o.original_trackers,
            torrent_limits: !o.no_limits,
            seed_limits: !o.no_seed_limits,
            move_completed: !o.no_move_completed,
            sequential: !o.no_sequential,
            first_last: !o.no_first_last,
            super_seed: !o.no_super_seed,
            identity,
            identity_skip_hosts: o.no_identity_for.clone(),
            settings,
        })
    }
}

/// Which torrents to take. Each kind of filter given must match; a repeated flag matches
/// any of its values. With none, everything is imported.
#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools)]
struct FilterArgs {
    /// Only torrents with this label ("" for none). Repeatable.
    #[arg(long = "label", value_name = "LABEL")]
    labels: Vec<String>,
    /// Leave out torrents with this label. Repeatable.
    #[arg(long = "exclude-label", value_name = "LABEL")]
    exclude_labels: Vec<String>,
    /// Only torrents announcing to this tracker host or its subdomains. Repeatable.
    #[arg(long = "tracker", value_name = "HOST")]
    trackers: Vec<String>,
    /// Only torrents saved in this folder or below it (after --map). Repeatable.
    #[arg(long = "save-path", value_name = "PATH")]
    save_paths: Vec<String>,
    /// Only complete torrents.
    #[arg(long, conflicts_with = "only_incomplete")]
    only_complete: bool,
    /// Only incomplete torrents.
    #[arg(long)]
    only_incomplete: bool,
    /// Only torrents paused in Deluge.
    #[arg(long, conflicts_with = "only_active")]
    only_paused: bool,
    /// Only torrents running in Deluge.
    #[arg(long)]
    only_active: bool,
    /// Only torrents whose name contains all of these words (ignoring case).
    #[arg(long, value_name = "WORDS")]
    name: Option<String>,
    /// Exactly this torrent, by info-hash. Repeatable.
    #[arg(long = "hash", value_name = "HASH")]
    ids: Vec<String>,
}

impl FilterArgs {
    fn filter(&self) -> apitorrent_types::DelugeImportFilter {
        let pick = |yes: bool, no: bool| match (yes, no) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            _ => None,
        };
        apitorrent_types::DelugeImportFilter {
            labels: self.labels.clone(),
            exclude_labels: self.exclude_labels.clone(),
            trackers: self.trackers.clone(),
            save_paths: self.save_paths.clone(),
            complete: pick(self.only_complete, self.only_incomplete),
            paused: pick(self.only_paused, self.only_active),
            name: self.name.clone(),
            ids: self.ids.clone(),
        }
    }
}

/// Run an import command; returns the report to print.
pub fn run(cmd: &ImportCommand, data_dir: &Path, download_dir: &Path) -> Result<String, String> {
    let ImportCommand::Deluge(a) = cmd;
    let map = a
        .map
        .iter()
        .map(|m| {
            m.split_once('=')
                .map(|(f, t)| (f.to_owned(), t.to_owned()))
                .ok_or_else(|| format!("--map takes FROM=TO, not {m:?}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    apitorrent_core::deluge_import::run_offline(
        data_dir,
        download_dir,
        &apitorrent_core::deluge_import::OfflineImport {
            config_dir: a.config.clone(),
            space: a.space.clone(),
            dry_run: a.dry_run,
            map,
            paused: a.paused,
            filter: a.filter.filter(),
            options: a.options()?,
        },
    )
}

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
    /// Also take Deluge's speed, connection and queue limits (into the space's settings, or
    /// the instance's for `default`).
    #[arg(long)]
    settings: bool,
    /// Add everything paused, whatever its state in Deluge.
    #[arg(long)]
    paused: bool,
    #[command(flatten)]
    filter: FilterArgs,
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
            import_settings: a.settings,
            paused: a.paused,
            filter: a.filter.filter(),
        },
    )
}

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
        },
    )
}

//! Self-updates: the `update` subcommand, the updater behind `/api/v1/system/update`, and
//! the restart into a newly installed binary. The checks themselves live in
//! `apitorrent-update`.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use apitorrent_server::{UpdateFailure, UpdateFailureKind, UpdateFuture, UpdateService};
use apitorrent_types::update::{UpdateSettings, UpdateStatus};
use apitorrent_update::restart::{RESTART_EXIT_CODE, Restart};
use apitorrent_update::{Installed, UpdateError, Updater, UpdaterConfig};

/// `apitorrentd update`.
#[derive(Debug, clap::Args)]
pub(crate) struct UpdateArgs {
    /// Only say whether a newer release exists; don't install it.
    #[arg(long)]
    check: bool,
}

/// The updater for this binary, or `None` (logged) if it can't run here.
pub(crate) fn updater(data_dir: PathBuf) -> Option<Updater> {
    let r = UpdaterConfig::for_this_binary(data_dir).and_then(Updater::new);
    match r {
        Ok(u) => {
            if let Some(reason) = u.installs_disabled_reason() {
                tracing::info!("update installs are off: {reason}");
            }
            Some(u)
        }
        Err(e) => {
            tracing::warn!(error = %e, "self-update is unavailable");
            None
        }
    }
}

fn failure(e: &UpdateError) -> UpdateFailure {
    UpdateFailure {
        code: e.code().to_owned(),
        message: e.to_string(),
        kind: match e {
            UpdateError::Busy => UpdateFailureKind::Busy,
            UpdateError::Disabled(_) => UpdateFailureKind::Disabled,
            _ => UpdateFailureKind::Failed,
        },
    }
}

/// The updater as the API server sees it.
#[derive(Debug)]
pub(crate) struct Service(pub(crate) Updater);

impl UpdateService for Service {
    fn status(&self) -> UpdateStatus {
        self.0.status()
    }

    fn check(&self) -> UpdateFuture<Result<UpdateStatus, UpdateFailure>> {
        let u = self.0.clone();
        Box::pin(async move { u.check().await.map_err(|e| failure(&e)) })
    }

    fn install(&self) -> UpdateFuture<Result<UpdateStatus, UpdateFailure>> {
        let u = self.0.clone();
        Box::pin(async move {
            u.install().await.map_err(|e| failure(&e))?;
            Ok(u.status())
        })
    }

    fn set_settings(&self, settings: UpdateSettings) -> Result<UpdateStatus, UpdateFailure> {
        self.0.set_settings(settings).map_err(|e| failure(&e))
    }
}

pub(crate) fn service(u: &Updater) -> Arc<dyn UpdateService> {
    Arc::new(Service(u.clone()))
}

/// Resolves once an install has put a new binary in place, after a moment for the API's
/// answer to reach the admin who asked. Never resolves without an updater.
pub(crate) async fn installed(u: Option<Updater>) {
    let Some(u) = u else {
        return std::future::pending().await;
    };
    let mut rx = u.installed();
    if rx.wait_for(Option::is_some).await.is_err() {
        return std::future::pending().await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    tracing::info!("restarting into the new version");
}

/// After a clean shutdown: start the new binary, if one was installed.
pub(crate) fn restart_if_installed(u: Option<&Updater>) -> Option<ExitCode> {
    let u = u?;
    let done: Installed = u.installed().borrow().clone()?;
    let exe = u.exe().to_path_buf();
    match apitorrent_update::restart::how_now() {
        Restart::Exit => {
            tracing::info!(
                version = %done.to,
                code = RESTART_EXIT_CODE,
                "exiting so systemd starts the new version"
            );
            Some(ExitCode::from(RESTART_EXIT_CODE))
        }
        Restart::Reexec => {
            let args: Vec<OsString> = std::env::args_os().skip(1).collect();
            tracing::info!(version = %done.to, exe = %exe.display(), "starting the new version");
            match apitorrent_update::restart::reexec(&exe, &args) {
                // Windows: the new process is running; this one ends.
                Ok(()) => Some(ExitCode::SUCCESS),
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "couldn't start the new version; start apitorrentd again by hand"
                    );
                    Some(ExitCode::from(RESTART_EXIT_CODE))
                }
            }
        }
    }
}

/// `apitorrentd update [--check]`.
pub(crate) async fn run_cli(args: &UpdateArgs, data_dir: PathBuf) -> ExitCode {
    let u = match UpdaterConfig::for_this_binary(data_dir).and_then(Updater::new) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let status = match u.check().await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let Some(latest) = &status.latest else {
        println!(
            "apitorrentd {} (there is no published release yet)",
            status.current_version
        );
        return ExitCode::SUCCESS;
    };
    if !status.available {
        println!(
            "apitorrentd {} is up to date (the latest release is {})",
            status.current_version, latest.version
        );
        return ExitCode::SUCCESS;
    }
    println!(
        "apitorrentd {} is available (this is {}): {}",
        latest.version, status.current_version, latest.url
    );
    if args.check {
        return ExitCode::SUCCESS;
    }
    if let Some(reason) = &status.installs_disabled_reason {
        eprintln!("Can't install it: {reason}.");
        return ExitCode::FAILURE;
    }
    match u.install().await {
        Ok(done) => {
            println!(
                "Installed apitorrentd {}. Restart the daemon to run it (under systemd: \
                 `systemctl restart apitorrentd`).",
                done.to
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("The update failed: {e}");
            ExitCode::FAILURE
        }
    }
}

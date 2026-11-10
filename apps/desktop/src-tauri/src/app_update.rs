//! The app's own updates, through `tauri-plugin-updater`.
//!
//! The plugin reads `latest.json` from the latest *published* GitHub release and installs
//! only an installer signed with apitorrent's update key: the same key the daemon uses,
//! compiled in from `release/update-signing.pub` (by `apitorrent-update`). A build without
//! that file still says when a new version exists (from GitHub's release list), but won't
//! install it.
//!
//! Local mode runs the core inside this process, so updating the app updates the "daemon"
//! it embeds too; there is nothing separate to update.
//!
//! The app checks once a day while it runs, and tells the window with an
//! `apitorrent://app-update` event. Installing only happens when the user asks.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter as _, Manager as _, Runtime, State};
use tauri_plugin_updater::UpdaterExt as _;

use apitorrent_update::UpdateKey;
use apitorrent_update::semver;

/// How often the app looks for a new version while it runs.
const CHECK_EVERY: Duration = Duration::from_secs(24 * 3600);
/// The first check waits a little, so it doesn't compete with reconnecting at launch.
const FIRST_CHECK: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppUpdateView {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub available: bool,
    pub notes: Option<String>,
    pub installs_enabled: bool,
    pub installs_disabled_reason: Option<String>,
    pub error: Option<String>,
}

/// The last check, so a reloaded window doesn't ask GitHub again.
#[derive(Default)]
pub struct AppUpdates {
    last: Mutex<Option<(Instant, AppUpdateView)>>,
    /// One check or install at a time.
    busy: tokio::sync::Mutex<()>,
}

fn key() -> Option<UpdateKey> {
    UpdateKey::embedded()
}

const NO_KEY: &str = "this build has no update signing key, so it can only report new versions";

/// The updater plugin, with the compiled-in key when there is one. Without it the key
/// stays empty, so nothing can verify and nothing installs.
pub fn plugin<R: Runtime>() -> tauri::plugin::TauriPlugin<R, tauri_plugin_updater::Config> {
    let mut b = tauri_plugin_updater::Builder::new();
    if let Some(k) = key() {
        b = b.pubkey(k.as_text());
    }
    b.build()
}

async fn check_now<R: Runtime>(app: &AppHandle<R>) -> AppUpdateView {
    let current = app.package_info().version.to_string();
    let mut view = AppUpdateView {
        current_version: current.clone(),
        latest_version: None,
        available: false,
        notes: None,
        installs_enabled: key().is_some(),
        installs_disabled_reason: key().is_none().then(|| NO_KEY.to_owned()),
        error: None,
    };
    if key().is_some() {
        match app.updater().map_err(|e| e.to_string()) {
            Ok(u) => match u.check().await {
                Ok(Some(update)) => {
                    view.latest_version = Some(update.version.clone());
                    view.available = true;
                    view.notes.clone_from(&update.body);
                }
                Ok(None) => view.latest_version = Some(current),
                Err(e) => view.error = Some(e.to_string()),
            },
            Err(e) => view.error = Some(e),
        }
    } else {
        match apitorrent_update::github::latest_published(&apitorrent_update::Source::github())
            .await
        {
            Ok(Some(r)) => {
                let newer = semver::Version::parse(&current).is_ok_and(|c| r.version > c);
                view.latest_version = Some(r.version.to_string());
                view.available = newer;
                view.notes = r.notes;
            }
            Ok(None) => {}
            Err(e) => view.error = Some(e.to_string()),
        }
    }
    view
}

/// The last check's result, checking again when it is a day old or `force` is set.
#[tauri::command]
pub async fn app_update_check<R: Runtime>(
    app: AppHandle<R>,
    updates: State<'_, AppUpdates>,
    force: bool,
) -> Result<AppUpdateView, String> {
    if !force
        && let Some((at, v)) = updates.last.lock().expect("not poisoned").as_ref()
        && at.elapsed() < CHECK_EVERY
    {
        return Ok(v.clone());
    }
    let _busy = updates.busy.lock().await;
    let view = check_now(&app).await;
    *updates.last.lock().expect("not poisoned") = Some((Instant::now(), view.clone()));
    Ok(view)
}

/// Download, verify and install the new version, then restart into it. A Local profile's
/// core is shut down cleanly first.
#[tauri::command]
pub async fn app_update_install<R: Runtime>(
    app: AppHandle<R>,
    updates: State<'_, AppUpdates>,
    state: State<'_, crate::state::AppState>,
) -> Result<(), String> {
    if key().is_none() {
        return Err(NO_KEY.to_owned());
    }
    let _busy = updates
        .busy
        .try_lock()
        .map_err(|_| "an update is already being checked or installed".to_owned())?;
    let update = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "there is no newer version".to_owned())?;
    tracing::info!(version = %update.version, "downloading the app update");
    // The plugin checks the signature against the compiled-in key.
    let bytes = update
        .download(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string())?;
    // Installing may end this process (Windows runs the installer and exits), so save a
    // Local profile's state first.
    crate::state::disconnect(&state).await;
    update.install(bytes).map_err(|e| e.to_string())?;
    tracing::info!(version = %update.version, "app update installed; restarting");
    app.restart();
}

/// The daily check, telling the window when something new turns up.
pub fn spawn_daily<R: Runtime>(app: AppHandle<R>) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_CHECK).await;
        loop {
            let updates = app.state::<AppUpdates>();
            let view = {
                let _busy = updates.busy.lock().await;
                check_now(&app).await
            };
            *updates.last.lock().expect("not poisoned") = Some((Instant::now(), view.clone()));
            if view.available {
                tracing::info!(latest = ?view.latest_version, "a new version of the app is available");
                let _ = app.emit("apitorrent://app-update", &view);
            }
            if let Some(e) = &view.error {
                tracing::warn!(error = %e, "the app update check failed");
            }
            tokio::time::sleep(CHECK_EVERY).await;
        }
    });
}

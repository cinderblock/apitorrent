//! The commands the web UI calls (`invoke`). They are the whole of what the webview can do
//! beyond the web app itself: it never sees an API token, never makes a network request of
//! its own, and can only open files through a verified path mapping.

use std::path::PathBuf;

use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tauri::{AppHandle, Runtime, State};
use tokio_tungstenite::tungstenite::Message;

use apitorrent_client::Trust;
use apitorrent_types::StorageRoot;

use crate::backend::Backend;
use crate::links::IncomingLink;
use crate::mapping::{self, MappingStatus, PathMapping, RemotePath};
use crate::profiles::{self, Profile, ProfileKind, token_key};
use crate::state::{self, AppState, ConnectionView};

type CmdResult<T> = Result<T, String>;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileView {
    #[serde(flatten)]
    pub profile: Profile,
    /// Remote profiles: whether an API token is saved in the keychain.
    pub has_token: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopState {
    pub version: &'static str,
    pub platform: &'static str,
    pub profiles: Vec<ProfileView>,
    pub active_id: Option<String>,
    pub connection: Option<ConnectionView>,
    pub connect_error: Option<String>,
    /// A problem with the saved profiles, shown once.
    pub notice: Option<String>,
}

fn view(state: &AppState, p: &Profile) -> ProfileView {
    let has_token =
        matches!(p.kind, ProfileKind::Remote(_)) && state.token(&p.id).ok().flatten().is_some();
    ProfileView {
        profile: p.clone(),
        has_token,
    }
}

#[tauri::command]
pub async fn desktop_state(state: State<'_, AppState>) -> CmdResult<DesktopState> {
    let (profiles, active_id) = {
        let store = state.store.lock().expect("not poisoned");
        let profiles = store.profiles().iter().map(|p| view(&state, p)).collect();
        (profiles, store.active().map(|p| p.id.clone()))
    };
    Ok(DesktopState {
        version: env!("CARGO_PKG_VERSION"),
        platform: std::env::consts::OS,
        profiles,
        active_id,
        connection: state::connection_view(&state).await,
        connect_error: state.connect_error.lock().expect("not poisoned").clone(),
        notice: state.notice.lock().expect("not poisoned").take(),
    })
}

/// The device a saved profile is paired as, and its credential, if it is paired.
fn paired_device(
    state: &AppState,
    id: &str,
) -> Option<(Profile, crate::pairing::PairedDevice, String)> {
    let profile = state.store.lock().expect("not poisoned").get(id).cloned()?;
    let ProfileKind::Remote(r) = &profile.kind else {
        return None;
    };
    let device = r.device.clone()?;
    let credential = state.token(id).ok().flatten()?;
    Some((profile, device, credential))
}

/// Remove a profile's device on its server, if it has one (best effort: the server may be
/// unreachable, and the user can remove it on the Account page).
async fn sign_out_device(state: &AppState, id: &str) {
    if let Some((profile, device, credential)) = paired_device(state, id)
        && let ProfileKind::Remote(r) = &profile.kind
    {
        crate::pairing::sign_out(&r.base_url, &r.trust, &credential, &device.id).await;
    }
}

/// Save a profile (a new one when `id` is empty). `token`: `Some("")` removes the saved
/// token, `Some(t)` replaces it, `None` leaves it. `pairing`: an approved device pairing
/// whose credential replaces the saved one. Saving the active profile reconnects.
#[tauri::command]
pub async fn profile_save<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    mut profile: Profile,
    token: Option<String>,
    pairing: Option<String>,
) -> CmdResult<ProfileView> {
    if profile.id.is_empty() {
        profile.id = profiles::new_id();
    }
    // Which device the profile is paired as is the app's to say, not the page's.
    let previous = state
        .store
        .lock()
        .expect("not poisoned")
        .get(&profile.id)
        .and_then(|p| match &p.kind {
            ProfileKind::Remote(r) => r.device.clone(),
            ProfileKind::Local(_) => None,
        });
    let mut token = token;
    let paired = match (&pairing, &mut profile.kind) {
        (Some(id), ProfileKind::Remote(r)) => {
            let (credential, device) = state.pairings.take(id, &r.base_url)?;
            r.device = Some(device);
            token = Some(credential);
            true
        }
        (Some(_), ProfileKind::Local(_)) => {
            return Err("only a server profile can be paired".into());
        }
        (None, ProfileKind::Remote(r)) => {
            // A pasted token (or removing it) replaces the device credential.
            r.device = if token.is_some() {
                None
            } else {
                previous.clone()
            };
            false
        }
        (None, ProfileKind::Local(_)) => false,
    };
    if previous.is_some() && (paired || token.is_some()) {
        // The old device credential is being replaced: remove it on the server too.
        sign_out_device(&state, &profile.id).await;
    }
    if let Some(t) = &token {
        let t = t.trim();
        if !t.is_empty()
            && (t.len() > 512 || t.chars().any(|c| c.is_whitespace() || c.is_control()))
        {
            return Err("that doesn't look like an API token (they start with apt_)".into());
        }
    }
    let is_active = {
        let mut store = state.store.lock().expect("not poisoned");
        store.upsert(profile.clone()).map_err(|e| e.to_string())?;
        store.active().is_some_and(|a| a.id == profile.id)
    };
    if let Some(t) = token {
        let key = token_key(&profile.id);
        let t = t.trim();
        let r = if t.is_empty() {
            state.secrets.delete(&key)
        } else {
            state.secrets.set(&key, t)
        };
        r.map_err(|e| {
            format!("the profile was saved, but the system keychain refused the token: {e}")
        })?;
    }
    if is_active {
        state::connect(&app, &state, profile.clone()).await?;
    }
    Ok(view(&state, &profile))
}

#[tauri::command]
pub async fn profile_delete(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let was_active = state
        .store
        .lock()
        .expect("not poisoned")
        .active()
        .is_some_and(|a| a.id == id);
    if was_active {
        state::disconnect(&state).await;
    }
    // A paired device is removed on the server too (read before the profile goes).
    let device = paired_device(&state, &id);
    state
        .store
        .lock()
        .expect("not poisoned")
        .remove(&id)
        .map_err(|e| e.to_string())?;
    if let Some((profile, device, credential)) = device
        && let ProfileKind::Remote(r) = &profile.kind
    {
        crate::pairing::sign_out(&r.base_url, &r.trust, &credential, &device.id).await;
    }
    state.secrets.delete(&token_key(&id)).map_err(|e| {
        format!("the profile was removed, but its token is still in the keychain: {e}")
    })
}

/// Make a profile the active one and connect with it. The UI reloads afterwards.
#[tauri::command]
pub async fn profile_activate<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    id: String,
) -> CmdResult<()> {
    let profile = {
        let mut store = state.store.lock().expect("not poisoned");
        let p = store
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("no profile with id {id}"))?;
        store.set_active(Some(&id)).map_err(|e| e.to_string())?;
        p
    };
    state::connect(&app, &state, profile).await
}

/// Disconnect and forget the active profile (back to the profile chooser).
#[tauri::command]
pub async fn profile_disconnect(state: State<'_, AppState>) -> CmdResult<()> {
    state::disconnect(&state).await;
    state
        .store
        .lock()
        .expect("not poisoned")
        .set_active(None)
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeView {
    /// The key the server presented: its full hash in hex, and the short pairing code the
    /// daemon prints, for comparing by eye.
    pub presented: Option<String>,
    pub presented_code: Option<String>,
    /// Whether `trust` accepts the server.
    pub accepted: bool,
    pub error: Option<String>,
}

/// Show what a server presents and whether a trust setting accepts it. Sends nothing but a
/// TLS handshake.
#[tauri::command]
pub async fn trust_probe(base_url: String, trust: Option<Trust>) -> CmdResult<ProbeView> {
    let trust = trust.unwrap_or(Trust::Os);
    let p = apitorrent_client::probe(&base_url, &trust)
        .await
        .map_err(|e| e.to_string())?;
    Ok(ProbeView {
        presented: p.presented.map(|h| h.to_hex()),
        presented_code: p.presented.map(|h| h.pairing_code()),
        accepted: p.verdict.is_ok(),
        error: p.verdict.err(),
    })
}

/// Pair with a server: the code the user typed (the daemon's short pairing code, or the
/// full hash) must name exactly the key the server presents. That key is then pinned, and
/// only it: the returned trust setting is saved in the profile.
#[tauri::command]
pub async fn trust_pair(base_url: String, code: String) -> CmdResult<Trust> {
    // First learn the key the server presents (whether or not anything trusts it yet).
    let seen = apitorrent_client::probe(&base_url, &Trust::Os)
        .await
        .map_err(|e| e.to_string())?;
    let Some(presented) = seen.presented else {
        return Err("the server didn't present a certificate (is the URL https://?)".into());
    };
    if !presented.matches_code(&code) {
        return Err(format!(
            "the server presents the key with code {presented}, which isn't the code you \
             typed. Don't pair: check that the code came from this server, and that nothing \
             sits between you and it."
        ));
    }
    // Then connect again trusting only that key, which also proves the server holds it.
    let trust = Trust::Pinned {
        spki_sha256: presented,
    };
    let p = apitorrent_client::probe(&base_url, &trust)
        .await
        .map_err(|e| e.to_string())?;
    match (p.presented, p.verdict) {
        (Some(again), Ok(())) if again == presented => Ok(trust),
        (_, Err(e)) => Err(e),
        _ => Err("the server presented a different key the second time; don't pair".into()),
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: String,
}

/// One API request through the active connection. The token, if any, is added here.
#[tauri::command]
pub async fn api_request(
    state: State<'_, AppState>,
    method: String,
    path: String,
    body: Option<String>,
    content_type: Option<String>,
) -> CmdResult<ApiResponse> {
    let backend = state.backend().await?;
    let r = backend
        .request(
            &method,
            &path,
            body.map(String::into_bytes),
            content_type.as_deref(),
        )
        .await?;
    Ok(ApiResponse {
        status: r.status,
        content_type: r.content_type,
        body: String::from_utf8_lossy(&r.body).into_owned(),
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "camelCase")]
pub enum SyncEvent {
    Open,
    Message { data: String },
    Close { code: u16, reason: String },
}

/// Open the sync socket through the active connection; events arrive on `channel`.
#[tauri::command]
pub async fn sync_open(state: State<'_, AppState>, channel: Channel<SyncEvent>) -> CmdResult<u32> {
    let backend = state.backend().await?;
    let id = state.next_sync_id();
    let task = tokio::spawn(async move {
        let mut socket = match backend.sync().await {
            Ok(s) => s,
            Err(e) => {
                let _ = channel.send(SyncEvent::Close {
                    code: close_code_for(&e),
                    reason: e,
                });
                return;
            }
        };
        if channel.send(SyncEvent::Open).is_err() {
            return;
        }
        let (code, reason) = loop {
            match socket.next().await {
                Some(Ok(Message::Text(t))) => {
                    if channel
                        .send(SyncEvent::Message {
                            data: t.as_str().to_owned(),
                        })
                        .is_err()
                    {
                        return; // The page went away.
                    }
                }
                Some(Ok(Message::Close(frame))) => {
                    break frame.map_or((1005, String::new()), |f| {
                        (f.code.into(), f.reason.as_str().to_owned())
                    });
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => break (1006, e.to_string()),
                None => break (1006, "the connection closed".to_owned()),
            }
        };
        let _ = channel.send(SyncEvent::Close { code, reason });
    });
    state
        .syncs
        .lock()
        .expect("not poisoned")
        .insert(id, task.abort_handle());
    Ok(id)
}

/// A failed handshake with 401 is the same as the server closing for a dead credential.
fn close_code_for(error: &str) -> u16 {
    if error.contains("answered 401") {
        4401
    } else {
        1006
    }
}

// Tauri hands commands their `State` by value.
#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
pub fn sync_close(state: State<'_, AppState>, id: u32) {
    if let Some(h) = state.syncs.lock().expect("not poisoned").remove(&id) {
        h.abort();
    }
}

/// The links (magnets, `.torrent` files) the OS handed over since the last call.
#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
pub fn take_pending_links(state: State<'_, AppState>) -> Vec<IncomingLink> {
    std::mem::take(&mut *state.pending_links.lock().expect("not poisoned"))
}

async fn fresh_roots(
    state: &AppState,
    backend: &Backend,
    profile: &str,
) -> CmdResult<Vec<StorageRoot>> {
    let roots = backend.roots().await?;
    *state.roots.lock().expect("not poisoned") = Some((profile.to_owned(), roots.clone()));
    Ok(roots)
}

/// Roots of the active connection, fetched once and then cached until it changes.
async fn cached_roots(
    state: &AppState,
    backend: &Backend,
    profile: &str,
) -> CmdResult<Vec<StorageRoot>> {
    let cached = state
        .roots
        .lock()
        .expect("not poisoned")
        .as_ref()
        .filter(|(id, _)| id == profile)
        .map(|(_, r)| r.clone());
    match cached {
        Some(r) => Ok(r),
        None => fresh_roots(state, backend, profile).await,
    }
}

async fn active_remote(state: &AppState) -> CmdResult<(Profile, Backend)> {
    let active = state.active.read().await;
    let a = active.as_ref().ok_or("not connected")?;
    Ok((a.profile.clone(), a.backend.clone()))
}

fn read_file(p: &std::path::Path) -> std::io::Result<Vec<u8>> {
    // Markers are tiny; don't read something huge if a path is wrong.
    use std::io::Read as _;
    let mut buf = Vec::new();
    std::fs::File::open(p)?
        .take(64 * 1024)
        .read_to_end(&mut buf)?;
    Ok(buf)
}

/// Check every mapping of `mappings` (or, by default, of the active profile) against the
/// server's storage roots.
#[tauri::command]
pub async fn mappings_check(
    state: State<'_, AppState>,
    mappings: Option<Vec<PathMapping>>,
) -> CmdResult<Vec<MappingStatus>> {
    let (profile, backend) = active_remote(&state).await?;
    let mappings = match (mappings, &profile.kind) {
        (Some(m), _) => m,
        (None, ProfileKind::Remote(r)) => r.mappings.clone(),
        (None, ProfileKind::Local(_)) => Vec::new(),
    };
    let roots = fresh_roots(&state, &backend, &profile.id).await?;
    Ok(mappings
        .iter()
        .map(|m| mapping::verify(m, &roots, &read_file))
        .collect())
}

/// Mappings to offer for the server's storage roots, from their share locators.
#[tauri::command]
pub async fn mapping_suggestions(
    state: State<'_, AppState>,
) -> CmdResult<Vec<mapping::Suggestion>> {
    let (profile, backend) = active_remote(&state).await?;
    let roots = fresh_roots(&state, &backend, &profile.id).await?;
    // Reading the mount table (and markers on network shares) can block.
    tokio::task::spawn_blocking(move || {
        let mounts = crate::shares::current_mounts();
        mapping::suggestions(&roots, &read_file, cfg!(windows), &mounts)
    })
    .await
    .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PathAction {
    /// Open the directory in the file manager.
    OpenFolder,
    /// Open the containing directory with the item selected.
    Reveal,
    /// Open the file with its default app.
    Open,
}

/// The local path for `save_path` (+ `file`, a torrent path joined with `/`) through the
/// active connection: as is for Local mode, through a verified mapping for Remote mode.
async fn locate(state: &AppState, save_path: &str, file: Option<&str>) -> CmdResult<PathBuf> {
    let (profile, backend) = active_remote(state).await?;
    let remote = RemotePath::parse(save_path)
        .ok_or_else(|| format!("{save_path} isn't an absolute path"))?;
    let remote = match file {
        Some(f) => remote
            .join_relative(f)
            .ok_or_else(|| format!("{f} isn't a plain relative path"))?,
        None => remote,
    };
    let mappings = match &profile.kind {
        ProfileKind::Local(_) => {
            // The server is this app: its paths are local paths. An identity mapping still
            // goes through the same checks against `..` and odd names.
            vec![PathMapping {
                remote: save_path.to_owned(),
                local: PathBuf::from(save_path),
            }]
        }
        ProfileKind::Remote(r) => r.mappings.clone(),
    };
    let (index, local) = mapping::to_local(&mappings, &remote).ok_or_else(|| {
        format!("no path mapping covers {save_path}; add one in this profile's settings")
    })?;
    if let ProfileKind::Remote(_) = profile.kind {
        // Check the mapping now, not just when it was saved: a share can be unmounted or
        // remounted elsewhere since.
        let roots = cached_roots(state, &backend, &profile.id).await?;
        let status = mapping::verify(&mappings[index], &roots, &read_file);
        if !status.verified {
            return Err(format!(
                "the mapping for {} isn't verified: {}",
                mappings[index].remote,
                status.problem.unwrap_or_default()
            ));
        }
    }
    Ok(local)
}

/// Open a folder, reveal a file, or open a file, through the OS shell.
#[tauri::command]
pub async fn path_action(
    state: State<'_, AppState>,
    action: PathAction,
    save_path: String,
    file: Option<String>,
) -> CmdResult<()> {
    let path = locate(&state, &save_path, file.as_deref()).await?;
    if !path.exists() {
        return Err(format!(
            "{} doesn't exist (yet): nothing of it may be downloaded, or the share may be out of date",
            path.display()
        ));
    }
    match action {
        PathAction::OpenFolder => {
            let dir = if path.is_dir() {
                path
            } else {
                path.parent()
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or(path)
            };
            tauri_plugin_opener::open_path(&dir, None::<&str>).map_err(|e| e.to_string())
        }
        PathAction::Reveal => {
            tauri_plugin_opener::reveal_item_in_dir(&path).map_err(|e| e.to_string())
        }
        PathAction::Open => {
            if path.is_dir() {
                return tauri_plugin_opener::open_path(&path, None::<&str>)
                    .map_err(|e| e.to_string());
            }
            if mapping::runs_code_when_opened(&path) {
                return Err(format!(
                    "{} is a program or script, so apitorrent won't open it; use \"Show in \
                     folder\" and open it yourself if you trust it",
                    path.display()
                ));
            }
            tauri_plugin_opener::open_path(&path, None::<&str>).map_err(|e| e.to_string())
        }
    }
}

/// Whether `save_path` can be opened here, and if not, why (shown next to the buttons).
#[tauri::command]
pub async fn path_available(
    state: State<'_, AppState>,
    save_path: String,
) -> CmdResult<Option<String>> {
    Ok(locate(&state, &save_path, None).await.err())
}

/// Start pairing this app as a device of the server at `base_url`, trusted as `trust`
/// (the profile being edited, saved or not). The page gets the code to show and the URL of
/// the server's approval page; the poll secret and, later, the credential stay here.
#[tauri::command]
pub async fn device_pair_start(
    state: State<'_, AppState>,
    base_url: String,
    trust: Trust,
    name: Option<String>,
) -> CmdResult<crate::pairing::PairingStarted> {
    let name = name
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(crate::pairing::default_device_name);
    state.pairings.start(&base_url, &trust, &name).await
}

#[tauri::command]
pub async fn device_pair_poll(
    state: State<'_, AppState>,
    pairing_id: String,
) -> CmdResult<crate::pairing::PairingStatus> {
    state.pairings.poll(&pairing_id).await
}

/// Open the server's approval page in the default browser (where the user is signed in to
/// the server, with passkeys that work there).
#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
pub fn device_pair_open(state: State<'_, AppState>, pairing_id: String) -> CmdResult<()> {
    let url = state
        .pairings
        .approve_url(&pairing_id)
        .ok_or("this pairing is over; start again")?;
    tauri_plugin_opener::open_url(url, None::<&str>).map_err(|e| e.to_string())
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
pub fn device_pair_cancel(state: State<'_, AppState>, pairing_id: String) {
    state.pairings.cancel(&pairing_id);
}

/// Where a folder picker opens: `start` if it is an existing absolute directory, else its
/// nearest existing ancestor, else the OS's choice.
pub fn picker_start(
    start: Option<&str>,
    is_dir: &dyn Fn(&std::path::Path) -> bool,
) -> Option<PathBuf> {
    let start = std::path::Path::new(start?.trim());
    if !start.is_absolute() {
        return None;
    }
    start
        .ancestors()
        .find(|p| is_dir(p))
        .map(std::path::Path::to_path_buf)
}

/// Ask for a folder with the OS's own picker. `None` when the user cancels. The dialog is
/// modal to the window that asked.
#[tauri::command]
pub async fn pick_folder<R: Runtime>(
    window: tauri::WebviewWindow<R>,
    title: Option<String>,
    start: Option<String>,
) -> CmdResult<Option<String>> {
    use tauri_plugin_dialog::DialogExt as _;
    let mut dialog = window.dialog().file().set_parent(&window);
    if let Some(t) = title.filter(|t| !t.trim().is_empty()) {
        dialog = dialog.set_title(t);
    }
    if let Some(dir) = picker_start(start.as_deref(), &|p| p.is_dir()) {
        dialog = dialog.set_directory(dir);
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    dialog.pick_folder(move |picked| {
        let _ = tx.send(picked);
    });
    let Some(picked) = rx
        .await
        .map_err(|_| "the folder picker closed unexpectedly")?
    else {
        return Ok(None);
    };
    let path = picked.into_path().map_err(|e| e.to_string())?;
    Ok(Some(path.display().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_starts_at_the_nearest_existing_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let inside = tmp.path().join("a");
        std::fs::create_dir(&inside).unwrap();
        let missing = inside.join("not").join("yet");
        let s = |p: &std::path::Path| picker_start(Some(&p.display().to_string()), &|p| p.is_dir());
        assert_eq!(s(&inside), Some(inside.clone()));
        assert_eq!(s(&missing), Some(inside));
        assert_eq!(picker_start(Some("relative/path"), &|_| true), None);
        assert_eq!(picker_start(Some("  "), &|_| true), None);
        assert_eq!(picker_start(None, &|_| true), None);
    }
}

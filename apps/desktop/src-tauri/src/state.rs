//! The app's state: profiles, the active connection, open sync sockets, links waiting for
//! the UI.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter as _, Manager as _, Runtime};
use tokio::sync::RwLock;

use apitorrent_client::{Remote, Token};
use apitorrent_types::StorageRoot;

use crate::backend::Backend;
use crate::links::IncomingLink;
use crate::local::{LanStatus, LocalNode};
use crate::profiles::{Profile, ProfileKind, ProfileStore, SecretStore, token_key};

/// Where the app keeps things.
#[derive(Debug, Clone)]
pub struct Dirs {
    /// `profiles.json`.
    pub config: PathBuf,
    /// Local profiles' data (`<data>/profiles/<id>`), and the web UI served on the LAN.
    pub data: PathBuf,
    /// The default download directory for Local profiles.
    pub downloads: PathBuf,
}

pub struct Active {
    pub profile: Profile,
    pub backend: Backend,
}

pub struct AppState {
    pub dirs: Dirs,
    pub store: Mutex<ProfileStore>,
    /// A problem with `profiles.json` found at start, shown once in the UI.
    pub notice: Mutex<Option<String>>,
    pub secrets: Arc<dyn SecretStore>,
    pub active: RwLock<Option<Active>>,
    /// Why the last connection attempt failed, if it did.
    pub connect_error: Mutex<Option<String>>,
    /// The active connection's storage roots, refreshed when mappings are checked.
    pub roots: Mutex<Option<(String, Vec<StorageRoot>)>>,
    pub syncs: Mutex<HashMap<u32, tokio::task::AbortHandle>>,
    next_sync: AtomicU32,
    pub pending_links: Mutex<Vec<IncomingLink>>,
    /// The task that watches for finished torrents (for notifications).
    pub watcher: Mutex<Option<tokio::task::AbortHandle>>,
}

impl AppState {
    pub fn new(dirs: Dirs, secrets: Arc<dyn SecretStore>) -> Self {
        let (store, problem) = ProfileStore::open(dirs.config.join("profiles.json"));
        Self {
            dirs,
            store: Mutex::new(store),
            notice: Mutex::new(problem.map(|e| e.to_string())),
            secrets,
            active: RwLock::new(None),
            connect_error: Mutex::new(None),
            roots: Mutex::new(None),
            syncs: Mutex::new(HashMap::new()),
            next_sync: AtomicU32::new(1),
            pending_links: Mutex::new(Vec::new()),
            watcher: Mutex::new(None),
        }
    }

    pub fn next_sync_id(&self) -> u32 {
        self.next_sync.fetch_add(1, Ordering::Relaxed)
    }

    pub async fn backend(&self) -> Result<Backend, String> {
        self.active
            .read()
            .await
            .as_ref()
            .map(|a| a.backend.clone())
            .ok_or_else(|| "not connected: choose a profile first".to_owned())
    }

    pub fn token(&self, profile: &str) -> Result<Option<String>, String> {
        self.secrets
            .get(&token_key(profile))
            .map_err(|e| format!("the system keychain refused: {e}"))
    }
}

/// What the UI shows about the connection.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionView {
    pub profile_id: String,
    pub local: bool,
    pub lan: Option<LanStatus>,
}

pub async fn connection_view(state: &AppState) -> Option<ConnectionView> {
    let active = state.active.read().await;
    let a = active.as_ref()?;
    Some(ConnectionView {
        profile_id: a.profile.id.clone(),
        local: a.backend.is_local(),
        lan: match &a.backend {
            Backend::Local(node) => node.lan_status(),
            Backend::Remote(_) => None,
        },
    })
}

/// Stop whatever is connected: close sync sockets and the watcher, and shut a local core
/// down (saving its state).
pub async fn disconnect(state: &AppState) {
    for (_, h) in state.syncs.lock().expect("not poisoned").drain() {
        h.abort();
    }
    if let Some(w) = state.watcher.lock().expect("not poisoned").take() {
        w.abort();
    }
    *state.roots.lock().expect("not poisoned") = None;
    let old = state.active.write().await.take();
    if let Some(Active {
        backend: Backend::Local(node),
        ..
    }) = old
    {
        node.shutdown().await;
    }
}

/// Connect with a profile, replacing any current connection. A Remote profile connects
/// lazily (its first request reaches the server); a Local one starts the core.
pub async fn connect<R: Runtime>(
    app: &AppHandle<R>,
    state: &AppState,
    profile: Profile,
) -> Result<(), String> {
    disconnect(state).await;
    let result = build(app, state, &profile).await;
    match result {
        Ok(backend) => {
            *state.connect_error.lock().expect("not poisoned") = None;
            *state.active.write().await = Some(Active {
                profile,
                backend: backend.clone(),
            });
            crate::notify::watch(app, state, backend);
            Ok(())
        }
        Err(e) => {
            *state.connect_error.lock().expect("not poisoned") = Some(e.clone());
            Err(e)
        }
    }
}

async fn build<R: Runtime>(
    app: &AppHandle<R>,
    state: &AppState,
    profile: &Profile,
) -> Result<Backend, String> {
    match &profile.kind {
        ProfileKind::Remote(r) => {
            let token = state.token(&profile.id)?.map(Token::new);
            let remote = Remote::new(&r.base_url, &r.trust, token).map_err(|e| e.to_string())?;
            Ok(Backend::Remote(remote))
        }
        ProfileKind::Local(l) => {
            let default_data = state.dirs.data.join("profiles").join(&profile.id);
            let web_ui = if l.lan.is_some() {
                extract_web_ui(app, &state.dirs.data.join("web-ui"))
            } else {
                None
            };
            let node =
                LocalNode::start(l, default_data, state.dirs.downloads.clone(), web_ui).await?;
            Ok(Backend::Local(node))
        }
    }
}

/// Write the web UI bundled in the app to `dir`, for the LAN listener to serve. `None` when
/// the app has no bundled UI (a development build that loads it from Vite).
fn extract_web_ui<R: Runtime>(app: &AppHandle<R>, dir: &std::path::Path) -> Option<PathBuf> {
    let resolver = app.asset_resolver();
    let keys: Vec<String> = resolver.iter().map(|(k, _)| k.into_owned()).collect();
    if !keys
        .iter()
        .any(|k| k.trim_start_matches('/') == "index.html")
    {
        tracing::info!("no bundled web UI; the network listener serves the API only");
        return None;
    }
    let _ = std::fs::remove_dir_all(dir);
    for key in keys {
        let rel = key.trim_start_matches('/');
        // Asset keys come from our own build, but stay inside `dir` regardless.
        if rel.split(['/', '\\']).any(|c| c == ".." || c.is_empty()) {
            continue;
        }
        let Some(asset) = resolver.get(key.clone()) else {
            continue;
        };
        let path = dir.join(rel);
        let written = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, asset.bytes()));
        if let Err(e) = written {
            tracing::warn!(path = %path.display(), error = %e, "couldn't write the web UI");
            return None;
        }
    }
    Some(dir.to_path_buf())
}

/// Hand links to the UI: queue them (the window may still be loading) and announce them.
pub fn deliver_links<R: Runtime>(app: &AppHandle<R>, links: Vec<IncomingLink>) {
    if links.is_empty() {
        return;
    }
    if let Some(state) = app.try_state::<AppState>() {
        state
            .pending_links
            .lock()
            .expect("not poisoned")
            .extend(links);
    }
    let _ = app.emit("apitorrent://links", ());
    crate::show_main_window(app);
}

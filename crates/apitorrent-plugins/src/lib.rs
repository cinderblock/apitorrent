//! apitorrent's plugin host: TypeScript plugins (bundled to one ES module) on embedded V8.
//!
//! - **One isolate per plugin**, each on its own thread, with a JavaScript heap limit and a
//!   separate budget for `ArrayBuffer` memory (which the heap limit doesn't cover).
//! - **A watchdog** terminates code that runs too long without yielding, and the heap-limit
//!   callback terminates a plugin that runs out of memory. Both record why, in the plugin's
//!   log and its `error`.
//! - **Every privileged call is an op checked against the plugin's grants.** The grants are
//!   what an admin approved from the manifest; an upgrade that asks for more waits for
//!   approval while the old version keeps running.
//! - **Globals are a curated web subset**: `fetch` (host-checked, strict TLS), `URL`,
//!   `TextEncoder`/`TextDecoder`, timers, `structuredClone`, `AbortController`,
//!   `Headers`/`Request`/`Response`, `crypto`, `console` (to the plugin's log). No
//!   filesystem, process, environment, sockets, WebAssembly or `Deno`.
//! - **The `apitorrent` API** reaches the app through [`AppApi`], implemented by the core.
//!
//! [`PluginHost`] implements [`PluginService`], which the HTTP server uses for the
//! management endpoints and for plugin routes.

mod alloc;
mod fetch;
mod logs;
mod ops;
mod runtime;
mod store;
mod watchdog;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use deno_core::v8;
use tokio::sync::{mpsc, oneshot, watch};

use apitorrent_plugin_api::network::NetworkGrants;
use apitorrent_plugin_api::{
    AppApi, BoxFuture, InstallPluginResponse, PendingPlugin, PluginBundle, PluginCapabilities,
    PluginError, PluginHttpRequest, PluginHttpResponse, PluginInfo, PluginLogEntry, PluginLogLevel,
    PluginManifest, PluginService, PluginSettings, PluginStatus, capabilities, schema, validate,
};

pub use fetch::FetchConfig;

use crate::logs::LogRing;
use crate::ops::PluginCtx;
use crate::runtime::{HostMsg, Limits, RunShared, STOP_GRACE};
use crate::store::{PluginRow, Store, StoreError};
use crate::watchdog::Watchdog;

/// How the host runs plugins.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// Where plugins live: `<data dir>/plugins`.
    pub dir: PathBuf,
    /// Each plugin's JavaScript heap limit.
    pub heap_limit: usize,
    /// Each plugin's `ArrayBuffer` memory limit (outside the heap).
    pub buffer_limit: usize,
    /// The longest a plugin may run without yielding before the watchdog stops it.
    pub max_turn: Duration,
    /// The shortest interval a scheduled job may ask for.
    pub min_schedule_interval: Duration,
    /// Each plugin's key-value storage limit, keys plus values.
    pub kv_quota: usize,
    /// Log lines kept per plugin.
    pub log_capacity: usize,
    /// How long a plugin route may take to answer.
    pub http_timeout: Duration,
    pub fetch: FetchConfig,
}

impl HostConfig {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            heap_limit: 64 * 1024 * 1024,
            buffer_limit: 64 * 1024 * 1024,
            max_turn: Duration::from_secs(2),
            min_schedule_interval: Duration::from_secs(60),
            kv_quota: 16 * 1024 * 1024,
            log_capacity: 1000,
            http_timeout: Duration::from_secs(30),
            fetch: FetchConfig::default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("the plugin store failed: {0}")]
    Store(String),
    #[error("{0}")]
    Startup(String),
}

impl From<StoreError> for PluginError {
    fn from(e: StoreError) -> Self {
        tracing::error!(error = %e, "plugin store error");
        Self::Internal("the plugin store failed".to_owned())
    }
}

/// The plugin host. Cheap to clone.
#[derive(Clone)]
pub struct PluginHost {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for PluginHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginHost")
            .field("dir", &self.inner.config.dir)
            .finish_non_exhaustive()
    }
}

struct Inner {
    config: Arc<HostConfig>,
    store: Arc<Store>,
    app: Arc<dyn AppApi>,
    main: tokio::runtime::Handle,
    fetcher: Arc<fetch::Fetcher>,
    watchdog: Watchdog,
    slots: Mutex<HashMap<String, Slot>>,
    /// Serialises installs, approvals, starts and stops.
    lifecycle: tokio::sync::Mutex<()>,
    generation: AtomicU64,
}

struct Slot {
    logs: Arc<LogRing>,
    run: Option<Run>,
}

#[derive(Clone)]
struct Run {
    shared: Arc<RunShared>,
    tx: mpsc::UnboundedSender<HostMsg>,
    isolate: v8::IsolateHandle,
    exited: watch::Receiver<bool>,
    manifest: Arc<PluginManifest>,
    stopping: Arc<std::sync::atomic::AtomicBool>,
    net: Arc<std::sync::RwLock<NetworkGrants>>,
}

impl PluginHost {
    /// Open the plugin directory and start every enabled plugin. Call it inside the tokio
    /// runtime that should run app calls and HTTP requests (the daemon's).
    pub fn start(config: HostConfig, app: Arc<dyn AppApi>) -> Result<Self, HostError> {
        let main = tokio::runtime::Handle::try_current()
            .map_err(|_| HostError::Startup("the plugin host needs a tokio runtime".into()))?;
        let store = Store::open(&config.dir).map_err(|e| HostError::Store(e.to_string()))?;
        let fetcher = fetch::Fetcher::new(config.fetch.clone()).map_err(HostError::Startup)?;
        let watchdog = Watchdog::start(config.max_turn)
            .map_err(|e| HostError::Startup(format!("could not start the watchdog: {e}")))?;
        let host = Self {
            inner: Arc::new(Inner {
                config: Arc::new(config),
                store: Arc::new(store),
                app,
                main,
                fetcher: Arc::new(fetcher),
                watchdog,
                slots: Mutex::new(HashMap::new()),
                lifecycle: tokio::sync::Mutex::new(()),
                generation: AtomicU64::new(0),
            }),
        };
        let rows = host
            .inner
            .store
            .plugins()
            .map_err(|e| HostError::Store(e.to_string()))?;
        if let Err(e) = host.inner.store.collect_garbage() {
            tracing::warn!(error = %e, "could not clean up old plugin bundles");
        }
        for row in rows {
            if row.enabled
                && row.active_version.is_some()
                && let Err(e) = host.start_run(&row.id)
            {
                tracing::warn!(plugin = %row.id, error = %e, "could not start a plugin");
            }
        }
        Ok(host)
    }

    /// Call a method of a role the plugin provides (`api.roles.provide`), such as a
    /// `source`'s `candidates`. Returns its result as JSON.
    pub async fn call_role(
        &self,
        id: &str,
        role: apitorrent_plugin_api::PluginRole,
        method: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, PluginError> {
        let run = self.running(id)?;
        if !run.manifest.capabilities.roles.contains(&role) {
            return Err(PluginError::InvalidRequest(format!(
                "plugin {id} doesn't have that role"
            )));
        }
        let role = serde_json::to_value(role)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let (reply, rx) = oneshot::channel();
        run.tx
            .send(HostMsg::Role {
                role,
                method: method.to_owned(),
                args,
                reply,
            })
            .map_err(|_| not_running())?;
        match tokio::time::timeout(self.inner.config.http_timeout, rx).await {
            Err(_) => Err(PluginError::Timeout),
            Ok(Err(_)) => Err(not_running()),
            Ok(Ok(r)) => r.map_err(PluginError::Internal),
        }
    }

    fn slot_logs(&self, id: &str) -> Arc<LogRing> {
        let mut slots = self.inner.slots.lock().expect("slots lock poisoned");
        slots
            .entry(id.to_owned())
            .or_insert_with(|| Slot {
                logs: Arc::new(LogRing::new(id, self.inner.config.log_capacity)),
                run: None,
            })
            .logs
            .clone()
    }

    fn current_run(&self, id: &str) -> Option<Run> {
        self.inner
            .slots
            .lock()
            .expect("slots lock poisoned")
            .get(id)
            .and_then(|s| s.run.clone())
    }

    fn running(&self, id: &str) -> Result<Run, PluginError> {
        match self.current_run(id) {
            Some(r) if r.shared.started.load(Ordering::Acquire) => Ok(r),
            Some(_) => Err(PluginError::NotRunning(
                "the plugin is still starting".to_owned(),
            )),
            None => Err(not_running()),
        }
    }

    fn row(&self, id: &str) -> Result<PluginRow, PluginError> {
        self.inner.store.plugin(id)?.ok_or(PluginError::NotFound)
    }

    fn manifest(&self, id: &str, version: &str) -> Result<PluginManifest, PluginError> {
        Ok(self
            .inner
            .store
            .version(id, version)?
            .ok_or_else(|| PluginError::Internal(format!("version {version} is missing")))?
            .manifest)
    }

    /// The schema in force: the active version's, or the pending one's before approval.
    fn settings_schema(&self, row: &PluginRow) -> Result<Option<serde_json::Value>, PluginError> {
        let version = row.active_version.as_ref().or(row.pending_version.as_ref());
        Ok(match version {
            Some(v) => self.manifest(&row.id, v)?.settings,
            None => None,
        })
    }

    fn effective_settings(&self, row: &PluginRow) -> Result<serde_json::Value, PluginError> {
        Ok(match self.settings_schema(row)? {
            Some(s) => schema::with_defaults(&s, &row.settings),
            None => row.settings.clone(),
        })
    }

    fn info(&self, row: &PluginRow) -> Result<PluginInfo, PluginError> {
        let active = match &row.active_version {
            Some(v) => Some(self.manifest(&row.id, v)?),
            None => None,
        };
        let pending = match &row.pending_version {
            Some(v) => {
                let manifest = self.manifest(&row.id, v)?;
                let granted = if active.is_some() {
                    row.granted.clone()
                } else {
                    PluginCapabilities::default()
                };
                Some(PendingPlugin {
                    new_capabilities: capabilities::expansion(&manifest.capabilities, &granted),
                    manifest,
                })
            }
            None => None,
        };
        let run = self.current_run(&row.id);
        let status = if active.is_none() {
            PluginStatus::AwaitingApproval
        } else if !row.enabled {
            PluginStatus::Disabled
        } else {
            match run {
                Some(r) if r.shared.started.load(Ordering::Acquire) => PluginStatus::Running,
                Some(_) => PluginStatus::Starting,
                None if row.error.is_some() => PluginStatus::Failed,
                None => PluginStatus::Starting,
            }
        };
        Ok(PluginInfo {
            id: row.id.clone(),
            status,
            enabled: row.enabled,
            active,
            granted: row.granted.clone(),
            pending,
            error: row.error.clone(),
            installed_at: row.installed_at,
            updated_at: row.updated_at,
        })
    }

    fn info_by_id(&self, id: &str) -> Result<PluginInfo, PluginError> {
        self.info(&self.row(id)?)
    }

    /// Start the active version on its own thread.
    fn start_run(&self, id: &str) -> Result<(), PluginError> {
        if self.current_run(id).is_some() {
            return Ok(());
        }
        let row = self.row(id)?;
        let version = row
            .active_version
            .clone()
            .ok_or_else(|| PluginError::Conflict("no version has been approved yet".into()))?;
        let stored = self
            .inner
            .store
            .version(id, &version)?
            .ok_or_else(|| PluginError::Internal(format!("version {version} is missing")))?;
        let code = self.inner.store.read_code(&stored.sha256)?;
        let manifest = Arc::new(stored.manifest);
        let settings = self.effective_settings(&row)?;
        if let Some(s) = &manifest.settings
            && let Err(errors) = schema::validate(s, &settings)
        {
            self.slot_logs(id).host(
                PluginLogLevel::Warn,
                &format!(
                    "its settings don't match its schema; fix them in the settings: {}",
                    errors.join("; ")
                ),
            );
        }
        // The manifest's own capabilities, which approval made a subset of the grants.
        let caps = manifest.capabilities.clone();
        let net = Arc::new(std::sync::RwLock::new(grants_for(&caps, &settings)));
        let generation = self.inner.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let shared = Arc::new(RunShared::new(generation));
        let (tx, inbox) = mpsc::unbounded_channel();
        let logs = self.slot_logs(id);
        let ctx = PluginCtx {
            id: id.to_owned(),
            version: version.clone(),
            caps,
            net: net.clone(),
            settings,
            app: self.inner.app.clone(),
            main: self.inner.main.clone(),
            store: self.inner.store.clone(),
            logs: logs.clone(),
            fetcher: self.inner.fetcher.clone(),
            config: self.inner.config.clone(),
            tx: tx.clone(),
            inbox,
            run: shared.clone(),
        };
        let (exit_tx, exited) = watch::channel(false);
        let stopping = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let weak: Weak<Inner> = Arc::downgrade(&self.inner);
        let (exit_id, exit_stopping) = (id.to_owned(), stopping.clone());
        // Signal the exit before clearing the slot: `start_run` checks the signal under the
        // slots lock before recording the run, so a run that ends at once is never recorded
        // after `finish_run` has looked for it.
        let on_exit = Box::new(move |result: runtime::Exit| {
            let _ = exit_tx.send(true);
            if let Some(inner) = weak.upgrade() {
                PluginHost { inner }.finish_run(
                    &exit_id,
                    generation,
                    exit_stopping.load(Ordering::Acquire),
                    result,
                );
            }
        });
        logs.host(PluginLogLevel::Info, &format!("starting version {version}"));
        // Cleared before the thread starts, so a failure it records isn't overwritten.
        self.inner.store.set_error(id, None)?;
        let limits = Limits {
            heap: self.inner.config.heap_limit,
            buffers: self.inner.config.buffer_limit,
        };
        let isolate = runtime::spawn(ctx, code, limits, on_exit).map_err(|e| {
            let msg = format!("could not start its thread: {e}");
            let _ = self.inner.store.set_error(id, Some(&msg));
            PluginError::Internal(msg)
        })?;
        self.inner.watchdog.watch(&shared, isolate.clone());
        let mut slots = self.inner.slots.lock().expect("slots lock poisoned");
        if let Some(slot) = slots.get_mut(id) {
            // The run may already have ended (a plugin that fails at once).
            if !*exited.borrow() {
                slot.run = Some(Run {
                    shared,
                    tx,
                    isolate,
                    exited,
                    manifest,
                    stopping,
                    net,
                });
            }
        }
        Ok(())
    }

    /// Called on the plugin's thread when a run ends.
    fn finish_run(&self, id: &str, generation: u64, stopping: bool, result: runtime::Exit) {
        let logs = self.slot_logs(id);
        {
            let mut slots = self.inner.slots.lock().expect("slots lock poisoned");
            if let Some(slot) = slots.get_mut(id)
                && slot
                    .run
                    .as_ref()
                    .is_some_and(|r| r.shared.generation == generation)
            {
                slot.run = None;
            }
        }
        match result {
            Ok(()) => logs.host(PluginLogLevel::Info, "stopped"),
            Err(reason) if stopping => {
                logs.host(PluginLogLevel::Warn, &format!("stopped: {reason}"));
            }
            Err(reason) => {
                logs.host(PluginLogLevel::Error, &format!("stopped: {reason}"));
                if let Err(e) = self.inner.store.set_error(id, Some(&reason)) {
                    tracing::error!(plugin = id, error = %e, "could not record a plugin failure");
                }
            }
        }
    }

    /// Stop the plugin's run, if any: ask it to stop, then end it after a grace period.
    async fn stop_run(&self, id: &str) {
        let Some(run) = self.current_run(id) else {
            return;
        };
        run.stopping.store(true, Ordering::Release);
        let _ = run.tx.send(HostMsg::Stop);
        let mut exited = run.exited.clone();
        if tokio::time::timeout(STOP_GRACE, exited.wait_for(|e| *e))
            .await
            .is_err()
        {
            run.shared
                .set_kill_reason(format!("it didn't stop within {} s", STOP_GRACE.as_secs()));
            run.shared.finish_stop();
            run.isolate.terminate_execution();
            let _ = tokio::time::timeout(Duration::from_secs(5), exited.wait_for(|e| *e)).await;
        }
        // If the thread is truly stuck, forget it rather than block the host.
        let mut slots = self.inner.slots.lock().expect("slots lock poisoned");
        if let Some(slot) = slots.get_mut(id)
            && slot
                .run
                .as_ref()
                .is_some_and(|r| r.shared.generation == run.shared.generation)
        {
            slot.run = None;
        }
    }

    async fn restart_if_enabled(&self, id: &str) -> Result<(), PluginError> {
        self.stop_run(id).await;
        if self.row(id)?.enabled {
            self.start_run(id)?;
        }
        Ok(())
    }

    async fn install_inner(
        &self,
        bundle: PluginBundle,
    ) -> Result<InstallPluginResponse, PluginError> {
        validate::bundle(&bundle).map_err(|e| PluginError::InvalidRequest(e.to_string()))?;
        let _lock = self.inner.lifecycle.lock().await;
        let m = &bundle.manifest;
        let sha = store::sha256_hex(bundle.code.as_bytes());
        let existing = self.inner.store.plugin(&m.id)?;
        if let Some(row) = &existing {
            for (slot, pending) in [(&row.active_version, false), (&row.pending_version, true)] {
                if slot.as_deref() == Some(m.version.as_str()) {
                    let stored = self.inner.store.version(&m.id, &m.version)?;
                    if stored.is_some_and(|s| s.sha256 == sha && &s.manifest == m) {
                        return Ok(InstallPluginResponse {
                            plugin: self.info(row)?,
                            needs_approval: pending,
                        });
                    }
                    return Err(PluginError::Conflict(format!(
                        "version {} of {} is already installed with different contents; give \
                         the new build a new version",
                        m.version, m.id
                    )));
                }
            }
        }
        self.inner.store.put_code(&bundle.code)?;
        self.inner.store.add_version(m, &sha)?;
        let logs = self.slot_logs(&m.id);
        let needs_approval = match &existing {
            Some(row) if row.active_version.is_some() => {
                let more = capabilities::expansion(&m.capabilities, &row.granted);
                if more.is_empty() {
                    self.inner
                        .store
                        .set_versions(&m.id, Some(&m.version), &row.granted, None)?;
                    logs.host(
                        PluginLogLevel::Info,
                        &format!("upgraded to version {} (no new capabilities)", m.version),
                    );
                    self.restart_if_enabled(&m.id).await?;
                    false
                } else {
                    self.inner.store.set_versions(
                        &m.id,
                        row.active_version.as_deref(),
                        &row.granted,
                        Some(&m.version),
                    )?;
                    logs.host(
                        PluginLogLevel::Info,
                        &format!(
                            "version {} is waiting for approval: it asks for more ({})",
                            m.version,
                            more.join("; ")
                        ),
                    );
                    true
                }
            }
            _ => {
                self.inner.store.set_versions(
                    &m.id,
                    None,
                    &PluginCapabilities::default(),
                    Some(&m.version),
                )?;
                logs.host(
                    PluginLogLevel::Info,
                    &format!("installed version {}; waiting for approval", m.version),
                );
                true
            }
        };
        if let Err(e) = self.inner.store.collect_garbage() {
            tracing::warn!(error = %e, "could not clean up old plugin bundles");
        }
        Ok(InstallPluginResponse {
            plugin: self.info_by_id(&m.id)?,
            needs_approval,
        })
    }

    async fn approve_inner(&self, id: &str, version: &str) -> Result<PluginInfo, PluginError> {
        let _lock = self.inner.lifecycle.lock().await;
        let row = self.row(id)?;
        if row.pending_version.as_deref() != Some(version) {
            return Err(PluginError::Conflict(match &row.pending_version {
                Some(p) => format!("version {version} isn't pending; {p} is"),
                None => "nothing is waiting for approval".to_owned(),
            }));
        }
        let manifest = self.manifest(id, version)?;
        // The grants become exactly what this version declares.
        self.inner
            .store
            .set_versions(id, Some(version), &manifest.capabilities, None)?;
        self.slot_logs(id)
            .host(PluginLogLevel::Info, &format!("version {version} approved"));
        self.restart_if_enabled(id).await?;
        if let Err(e) = self.inner.store.collect_garbage() {
            tracing::warn!(error = %e, "could not clean up old plugin bundles");
        }
        self.info_by_id(id)
    }

    async fn set_enabled_inner(&self, id: &str, enabled: bool) -> Result<PluginInfo, PluginError> {
        let _lock = self.inner.lifecycle.lock().await;
        let row = self.row(id)?;
        if enabled && row.active_version.is_none() {
            return Err(PluginError::Conflict(
                "approve the plugin before enabling it".to_owned(),
            ));
        }
        self.inner.store.set_enabled(id, enabled)?;
        if enabled {
            self.start_run(id)?;
        } else {
            self.stop_run(id).await;
            self.inner.store.set_error(id, None)?;
        }
        self.info_by_id(id)
    }

    async fn uninstall_inner(&self, id: &str) -> Result<(), PluginError> {
        let _lock = self.inner.lifecycle.lock().await;
        self.row(id)?;
        self.stop_run(id).await;
        self.inner.store.remove(id)?;
        self.inner
            .slots
            .lock()
            .expect("slots lock poisoned")
            .remove(id);
        if let Err(e) = self.inner.store.collect_garbage() {
            tracing::warn!(error = %e, "could not clean up old plugin bundles");
        }
        tracing::info!(plugin = id, "plugin uninstalled");
        Ok(())
    }

    fn settings_inner(&self, id: &str) -> Result<PluginSettings, PluginError> {
        let row = self.row(id)?;
        let schema = self.settings_schema(&row)?;
        let mut values = self.effective_settings(&row)?;
        let mut secrets_set = Vec::new();
        if let (Some(s), Some(obj)) = (&schema, values.as_object_mut()) {
            for k in schema::secrets(s) {
                if obj
                    .remove(&k)
                    .is_some_and(|v| v.as_str().is_some_and(|s| !s.is_empty()))
                {
                    secrets_set.push(k);
                }
            }
        }
        Ok(PluginSettings {
            schema,
            values,
            secrets_set,
        })
    }

    async fn set_settings_inner(
        &self,
        id: &str,
        values: serde_json::Value,
    ) -> Result<PluginSettings, PluginError> {
        let _lock = self.inner.lifecycle.lock().await;
        let row = self.row(id)?;
        let schema = self
            .settings_schema(&row)?
            .ok_or_else(|| PluginError::InvalidRequest("this plugin has no settings".to_owned()))?;
        let serde_json::Value::Object(mut incoming) = values else {
            return Err(PluginError::InvalidRequest(
                "settings must be an object".to_owned(),
            ));
        };
        // Secrets the client left out keep their value; `null` clears them.
        for k in schema::secrets(&schema) {
            match incoming.get(&k) {
                None => {
                    if let Some(old) = row.settings.get(&k) {
                        incoming.insert(k, old.clone());
                    }
                }
                Some(serde_json::Value::Null) => {
                    incoming.remove(&k);
                }
                Some(_) => {}
            }
        }
        let merged = serde_json::Value::Object(incoming);
        if serde_json::to_vec(&merged).map_or(usize::MAX, |v| v.len()) > schema::MAX_SETTINGS_BYTES
        {
            return Err(PluginError::InvalidRequest(format!(
                "settings are limited to {} bytes",
                schema::MAX_SETTINGS_BYTES
            )));
        }
        let effective = schema::with_defaults(&schema, &merged);
        schema::validate(&schema, &effective)
            .map_err(|errors| PluginError::InvalidRequest(errors.join("; ")))?;
        self.inner.store.set_settings(id, &merged)?;
        if let Some(run) = self.current_run(id) {
            *run.net.write().expect("grants lock poisoned") =
                grants_for(&run.manifest.capabilities, &effective);
            let _ = run.tx.send(HostMsg::Settings(effective));
        }
        self.slot_logs(id)
            .host(PluginLogLevel::Info, "settings changed");
        self.settings_inner(id)
    }

    async fn http_inner(
        &self,
        id: &str,
        req: PluginHttpRequest,
    ) -> Result<PluginHttpResponse, PluginError> {
        let row = self.row(id)?;
        let Some(version) = &row.active_version else {
            return Err(PluginError::NoRoute);
        };
        let manifest = self.manifest(id, version)?;
        let declared = manifest.capabilities.routes.iter().any(|r| {
            validate::route_matches(&r.path, &req.path) && r.methods.contains(&req.method)
        });
        if !declared {
            return Err(PluginError::NoRoute);
        }
        let run = self.running(id)?;
        let (reply, rx) = oneshot::channel();
        run.tx
            .send(HostMsg::Http(req, reply))
            .map_err(|_| not_running())?;
        match tokio::time::timeout(self.inner.config.http_timeout, rx).await {
            Err(_) => Err(PluginError::Timeout),
            Ok(Err(_)) => Err(not_running()),
            Ok(Ok(r)) => r,
        }
    }

    async fn shutdown_inner(&self) {
        let ids: Vec<String> = self
            .inner
            .slots
            .lock()
            .expect("slots lock poisoned")
            .iter()
            .filter(|(_, s)| s.run.is_some())
            .map(|(id, _)| id.clone())
            .collect();
        let stops = ids.iter().map(|id| self.stop_run(id));
        futures_join_all(stops).await;
    }
}

/// A plugin's network grants: its manifest's, plus the hosts named in the settings fields
/// it declared in `networkFromSettings`.
fn grants_for(caps: &PluginCapabilities, settings: &serde_json::Value) -> NetworkGrants {
    let from_settings: Vec<String> = caps
        .network_from_settings
        .iter()
        .flat_map(|path| schema::strings_at(settings, path))
        .collect();
    NetworkGrants::combine(&caps.network, &from_settings)
}

fn not_running() -> PluginError {
    PluginError::NotRunning("the plugin isn't running".to_owned())
}

/// Await futures concurrently (a small `join_all`, to avoid a dependency for one call).
async fn futures_join_all<F: Future<Output = ()>>(futs: impl Iterator<Item = F>) {
    let mut set: Vec<std::pin::Pin<Box<F>>> = futs.map(Box::pin).collect();
    std::future::poll_fn(|cx| {
        set.retain_mut(|f| f.as_mut().poll(cx).is_pending());
        if set.is_empty() {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
}

impl PluginService for PluginHost {
    fn list(&self) -> Vec<PluginInfo> {
        match self.inner.store.plugins() {
            Ok(rows) => rows
                .iter()
                .filter_map(|r| match self.info(r) {
                    Ok(i) => Some(i),
                    Err(e) => {
                        tracing::error!(plugin = %r.id, error = %e, "could not describe a plugin");
                        None
                    }
                })
                .collect(),
            Err(e) => {
                tracing::error!(error = %e, "could not list plugins");
                Vec::new()
            }
        }
    }

    fn get(&self, id: &str) -> Result<PluginInfo, PluginError> {
        self.info_by_id(id)
    }

    fn install(
        &self,
        bundle: PluginBundle,
    ) -> BoxFuture<Result<InstallPluginResponse, PluginError>> {
        let host = self.clone();
        Box::pin(async move { host.install_inner(bundle).await })
    }

    fn approve(&self, id: String, version: String) -> BoxFuture<Result<PluginInfo, PluginError>> {
        let host = self.clone();
        Box::pin(async move { host.approve_inner(&id, &version).await })
    }

    fn set_enabled(&self, id: String, enabled: bool) -> BoxFuture<Result<PluginInfo, PluginError>> {
        let host = self.clone();
        Box::pin(async move { host.set_enabled_inner(&id, enabled).await })
    }

    fn uninstall(&self, id: String) -> BoxFuture<Result<(), PluginError>> {
        let host = self.clone();
        Box::pin(async move { host.uninstall_inner(&id).await })
    }

    fn logs(&self, id: &str, after: Option<u64>) -> Result<Vec<PluginLogEntry>, PluginError> {
        self.row(id)?;
        Ok(self.slot_logs(id).since(after))
    }

    fn settings(&self, id: &str) -> Result<PluginSettings, PluginError> {
        self.settings_inner(id)
    }

    fn set_settings(
        &self,
        id: String,
        values: serde_json::Value,
    ) -> BoxFuture<Result<PluginSettings, PluginError>> {
        let host = self.clone();
        Box::pin(async move { host.set_settings_inner(&id, values).await })
    }

    fn http(
        &self,
        id: String,
        req: PluginHttpRequest,
    ) -> BoxFuture<Result<PluginHttpResponse, PluginError>> {
        let host = self.clone();
        Box::pin(async move { host.http_inner(&id, req).await })
    }

    fn shutdown(&self) -> BoxFuture<()> {
        let host = self.clone();
        Box::pin(async move { host.shutdown_inner().await })
    }
}

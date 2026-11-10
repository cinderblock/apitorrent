//! The ops a plugin's JavaScript can call, and the JavaScript that wraps them.
//!
//! Every privileged op checks the plugin's grants here, in Rust; the JavaScript wrappers
//! are convenience, not security. A denied call throws `NotCapable`. The plugin's id comes
//! from its own [`PluginCtx`], never from JavaScript, so one plugin can't name another's
//! data.
//!
//! Structured data crosses as JSON strings rather than through `serde_v8`, so 64-bit
//! numbers arrive as plain numbers and the wire shape is exactly the API's JSON.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use deno_core::{OpState, ToJsBuffer, op2, v8};
use deno_error::JsErrorBox;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::AbortHandle;

use apitorrent_plugin_api::network::NetworkGrants;
use apitorrent_plugin_api::{
    AppApi, AppError, PluginApiScope, PluginCaller, PluginCapabilities, PluginError,
    PluginHttpResponse, PluginLogLevel, PluginRole, TorrentControl, VerifiedToken,
};
use apitorrent_types::{AddTorrentRequest, QueueMove, SeedLimits};

use crate::HostConfig;
use crate::fetch::{FetchArgs, FetchError, Fetcher};
use crate::logs::LogRing;
use crate::runtime::{HostMsg, RunShared};
use crate::store::{Store, now_ms};

/// Longest KV key, in bytes.
const MAX_KEY: usize = 512;
/// Largest KV value, serialised.
const MAX_VALUE: usize = 1024 * 1024;
/// Most login sessions a plugin holds at once; the least recently used goes first.
const MAX_SESSIONS: usize = 1024;

/// Everything a plugin's ops need, fixed for the life of one run.
pub(crate) struct PluginCtx {
    pub id: String,
    /// Where this instance's key-value data and schedule are kept: the plugin id, or
    /// `<id>@<space>` for a space-aware plugin's instance outside `default`.
    pub store_key: String,
    pub version: String,
    pub caps: PluginCapabilities,
    /// The manifest's grants plus those from settings; replaced when settings change.
    pub net: Arc<std::sync::RwLock<NetworkGrants>>,
    /// Settings with defaults filled in, as of the start of the run.
    pub settings: serde_json::Value,
    pub app: Arc<dyn AppApi>,
    /// The daemon's runtime: app calls and HTTP requests run there, not on the plugin's
    /// thread, so nothing they spawn dies with the plugin.
    pub main: tokio::runtime::Handle,
    pub store: Arc<Store>,
    pub logs: Arc<LogRing>,
    pub fetcher: Arc<Fetcher>,
    pub config: Arc<HostConfig>,
    /// The plugin's own inbox, for the event and schedule tasks to post into.
    pub tx: mpsc::UnboundedSender<HostMsg>,
    pub inbox: mpsc::UnboundedReceiver<HostMsg>,
    pub run: Arc<RunShared>,
}

impl std::fmt::Debug for PluginCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginCtx")
            .field("id", &self.id)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

/// The bootstrap's `start` function, taken by the runtime after the plugin's module ran.
pub(crate) struct StartFn(pub v8::Global<v8::Function>);

struct Inbox(Rc<tokio::sync::Mutex<mpsc::UnboundedReceiver<HostMsg>>>);

enum Reply {
    Http(oneshot::Sender<Result<PluginHttpResponse, PluginError>>),
    Role(oneshot::Sender<Result<serde_json::Value, String>>),
}

#[derive(Default)]
struct Replies {
    next: u32,
    pending: HashMap<u32, Reply>,
}

/// Login sessions a plugin made from verified API tokens (`api.auth`). In memory only:
/// they end with the run, and clients sign in again.
#[derive(Default)]
struct Sessions(HashMap<String, Session>);

struct Session {
    token: VerifiedToken,
    last_used: Instant,
}

#[derive(Default)]
struct Tasks {
    events: Option<AbortHandle>,
    schedules: HashMap<String, AbortHandle>,
    fetches: HashMap<u32, AbortHandle>,
}

impl Tasks {
    fn abort_all(&mut self) {
        if let Some(e) = self.events.take() {
            e.abort();
        }
        for (_, h) in self.schedules.drain() {
            h.abort();
        }
        for (_, h) in self.fetches.drain() {
            h.abort();
        }
    }
}

deno_core::extension!(
    apitorrent_plugin,
    ops = [
        op_at_info,
        op_at_register,
        op_at_log,
        op_at_started,
        op_at_stopped,
        op_at_next,
        op_at_reply_http,
        op_at_reply_role,
        op_at_reply_error,
        op_at_settings,
        op_at_kv_get,
        op_at_kv_set,
        op_at_kv_delete,
        op_at_kv_list,
        op_at_schedule,
        op_at_unschedule,
        op_at_events,
        op_at_route,
        op_at_role,
        op_at_app_info,
        op_at_auth_login,
        op_at_auth_session,
        op_at_auth_logout,
        op_at_torrents_list,
        op_at_torrent_get,
        op_at_torrent_add,
        op_at_torrent_control,
        op_at_torrent_remove,
        op_at_candidates_submit,
        op_at_fetch,
        op_at_fetch_abort,
        op_at_url_parse,
        op_at_url_set,
        op_at_encoding_name,
        op_at_text_decode,
        op_at_random,
        op_at_uuid,
        op_at_digest,
        op_at_hmac,
        op_at_hmac_verify,
    ],
    esm_entry_point = "ext:apitorrent_plugin/99_main.js",
    esm = [
        dir "src/js",
        "00_util.js",
        "01_web.js",
        "02_fetch.js",
        "03_console.js",
        "04_api.js",
        "99_main.js",
    ],
    options = { ctx: PluginCtx },
    state = |state, options| {
        let mut ctx = options.ctx;
        let (_, empty) = mpsc::unbounded_channel();
        let inbox = std::mem::replace(&mut ctx.inbox, empty);
        state.put(Inbox(Rc::new(tokio::sync::Mutex::new(inbox))));
        state.put(Replies::default());
        state.put(Tasks::default());
        state.put(Sessions::default());
        state.put(Rc::new(ctx));
    },
);

fn ctx(state: &OpState) -> Rc<PluginCtx> {
    state.borrow::<Rc<PluginCtx>>().clone()
}

fn not_capable(msg: impl Into<String>) -> JsErrorBox {
    JsErrorBox::new("NotCapable", msg.into())
}

fn type_error(msg: impl Into<String>) -> JsErrorBox {
    JsErrorBox::type_error(msg.into())
}

fn range_error(msg: impl Into<String>) -> JsErrorBox {
    JsErrorBox::new("RangeError", msg.into())
}

fn internal(e: impl std::fmt::Display) -> JsErrorBox {
    JsErrorBox::generic(format!("internal error in the plugin host: {e}"))
}

fn scope_name(s: PluginApiScope) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn require(c: &PluginCtx, scope: PluginApiScope) -> Result<(), JsErrorBox> {
    if c.caps.api.contains(&scope) {
        Ok(())
    } else {
        Err(not_capable(format!(
            "this needs the `{}` capability, which the plugin's manifest doesn't declare",
            scope_name(scope)
        )))
    }
}

fn to_json<T: Serialize>(v: &T) -> Result<String, JsErrorBox> {
    serde_json::to_string(v).map_err(internal)
}

fn from_json<T: serde::de::DeserializeOwned>(text: &str, what: &str) -> Result<T, JsErrorBox> {
    serde_json::from_str(text).map_err(|e| type_error(format!("invalid {what}: {e}")))
}

/// App calls resolve to `{ ok: true, value }` or `{ ok: false, code, message }`, which the
/// JavaScript turns into a value or an `ApitorrentError` with a code.
#[derive(Serialize)]
#[serde(untagged)]
enum AppResult<T> {
    Ok {
        ok: bool,
        value: T,
    },
    Err {
        ok: bool,
        code: String,
        message: String,
    },
}

impl<T> From<Result<T, AppError>> for AppResult<T> {
    fn from(r: Result<T, AppError>) -> Self {
        match r {
            Ok(value) => Self::Ok { ok: true, value },
            Err(e) => Self::Err {
                ok: false,
                code: e.code,
                message: e.message,
            },
        }
    }
}

/// Run an app call on the daemon's runtime.
async fn on_main<T: Send + 'static + Serialize>(
    main: &tokio::runtime::Handle,
    fut: apitorrent_plugin_api::BoxFuture<Result<T, AppError>>,
) -> Result<String, JsErrorBox> {
    let r = main.spawn(fut).await.map_err(internal)?;
    to_json(&AppResult::from(r))
}

// --- Lifecycle and messages -------------------------------------------------------------

#[op2]
#[string]
fn op_at_info(state: &OpState) -> Result<String, JsErrorBox> {
    let c = ctx(state);
    to_json(&serde_json::json!({
        "id": c.id,
        "version": c.version,
        "capabilities": c.caps,
    }))
}

#[op2]
fn op_at_register(state: &mut OpState, #[scoped] start: v8::Global<v8::Function>) {
    state.put(StartFn(start));
}

#[op2(fast)]
fn op_at_log(state: &OpState, level: u32, #[string] message: &str) {
    let level = match level {
        0 => PluginLogLevel::Debug,
        1 => PluginLogLevel::Info,
        2 => PluginLogLevel::Warn,
        _ => PluginLogLevel::Error,
    };
    ctx(state).logs.push(level, false, message);
}

#[op2(fast)]
fn op_at_started(state: &OpState) {
    let c = ctx(state);
    c.run.started.store(true, Ordering::Release);
    c.logs.host(
        PluginLogLevel::Info,
        &format!("started version {}", c.version),
    );
}

#[op2(fast)]
fn op_at_stopped(state: &mut OpState) {
    state.borrow_mut::<Tasks>().abort_all();
    ctx(state).run.finish_stop();
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum ToJs {
    Event {
        event: apitorrent_plugin_api::AppEvent,
    },
    Schedule {
        name: String,
    },
    Http {
        id: u32,
        method: String,
        /// Relative to the plugin's prefix, for matching routes.
        path: String,
        url: String,
        headers: Vec<(String, String)>,
        caller: Option<PluginCaller>,
    },
    Role {
        id: u32,
        role: String,
        method: String,
        args: serde_json::Value,
    },
    Settings {
        values: serde_json::Value,
    },
    Stop,
}

/// The next message for the plugin: its JSON, plus a request body when there is one.
#[derive(Serialize)]
struct Next {
    json: String,
    body: Option<ToJsBuffer>,
}

#[op2]
#[serde]
async fn op_at_next(state: Rc<RefCell<OpState>>) -> Result<Option<Next>, JsErrorBox> {
    let inbox = state.borrow().borrow::<Inbox>().0.clone();
    let msg = inbox.lock().await.recv().await;
    let Some(msg) = msg else {
        return Ok(None);
    };
    let mut state = state.borrow_mut();
    let mut add = |reply: Reply| {
        let replies = state.borrow_mut::<Replies>();
        replies.next = replies.next.wrapping_add(1).max(1);
        let id = replies.next;
        replies.pending.insert(id, reply);
        id
    };
    let (msg, body) = match msg {
        HostMsg::Event(event) => (ToJs::Event { event }, None),
        HostMsg::Schedule(name) => (ToJs::Schedule { name }, None),
        HostMsg::Http(req, reply) => {
            let id = add(Reply::Http(reply));
            (
                ToJs::Http {
                    id,
                    method: req.method,
                    path: req.path,
                    url: req.url,
                    headers: req.headers,
                    caller: req.caller,
                },
                Some(req.body),
            )
        }
        HostMsg::Role {
            role,
            method,
            args,
            reply,
        } => {
            let id = add(Reply::Role(reply));
            (
                ToJs::Role {
                    id,
                    role,
                    method,
                    args,
                },
                None,
            )
        }
        HostMsg::Settings(values) => (ToJs::Settings { values }, None),
        HostMsg::Stop => (ToJs::Stop, None),
    };
    Ok(Some(Next {
        json: to_json(&msg)?,
        body: body.map(ToJsBuffer::from),
    }))
}

#[derive(Deserialize)]
struct HttpReply {
    status: u16,
    headers: Vec<(String, String)>,
}

#[op2(fast)]
fn op_at_reply_http(
    state: &mut OpState,
    #[smi] id: u32,
    #[string] meta: &str,
    #[buffer(copy)] body: Vec<u8>,
) -> Result<(), JsErrorBox> {
    let meta: HttpReply = from_json(meta, "response")?;
    if let Some(Reply::Http(tx)) = state.borrow_mut::<Replies>().pending.remove(&id) {
        let _ = tx.send(Ok(PluginHttpResponse {
            status: meta.status,
            headers: meta.headers,
            body,
        }));
    }
    Ok(())
}

#[op2(fast)]
fn op_at_reply_role(state: &mut OpState, #[smi] id: u32, #[string] value: &str) {
    if let Some(Reply::Role(tx)) = state.borrow_mut::<Replies>().pending.remove(&id) {
        let v = serde_json::from_str(value).map_err(|e| format!("bad role result: {e}"));
        let _ = tx.send(v);
    }
}

#[op2(fast)]
fn op_at_reply_error(state: &mut OpState, #[smi] id: u32, #[string] message: &str) {
    match state.borrow_mut::<Replies>().pending.remove(&id) {
        Some(Reply::Http(tx)) => {
            let _ = tx.send(Err(PluginError::Internal(message.to_owned())));
        }
        Some(Reply::Role(tx)) => {
            let _ = tx.send(Err(message.to_owned()));
        }
        None => {}
    }
}

#[op2]
#[string]
fn op_at_settings(state: &OpState) -> Result<String, JsErrorBox> {
    to_json(&ctx(state).settings)
}

// --- Key-value storage ------------------------------------------------------------------

fn kv_ctx(state: &OpState, key: Option<&str>) -> Result<Rc<PluginCtx>, JsErrorBox> {
    let c = ctx(state);
    if !c.caps.kv {
        return Err(not_capable(
            "storage needs the `kv` capability, which the plugin's manifest doesn't declare",
        ));
    }
    if let Some(k) = key
        && (k.is_empty() || k.len() > MAX_KEY)
    {
        return Err(type_error(format!("keys are 1 to {MAX_KEY} bytes")));
    }
    Ok(c)
}

/// The stored JSON, or an empty string when the key is unset (a stored value is never
/// empty).
#[op2]
#[string]
fn op_at_kv_get(state: &OpState, #[string] key: &str) -> Result<String, JsErrorBox> {
    let c = kv_ctx(state, Some(key))?;
    Ok(c.store
        .kv_get(&c.store_key, key)
        .map_err(internal)?
        .unwrap_or_default())
}

#[op2(fast)]
fn op_at_kv_set(
    state: &OpState,
    #[string] key: &str,
    #[string] value: &str,
) -> Result<(), JsErrorBox> {
    let c = kv_ctx(state, Some(key))?;
    if value.len() > MAX_VALUE {
        return Err(range_error(format!(
            "values are at most {MAX_VALUE} bytes as JSON"
        )));
    }
    if !c
        .store
        .kv_set(&c.store_key, key, value, c.config.kv_quota)
        .map_err(internal)?
    {
        return Err(range_error(format!(
            "the plugin's storage is full ({} bytes)",
            c.config.kv_quota
        )));
    }
    Ok(())
}

#[op2(fast)]
fn op_at_kv_delete(state: &OpState, #[string] key: &str) -> Result<bool, JsErrorBox> {
    let c = kv_ctx(state, Some(key))?;
    c.store.kv_delete(&c.store_key, key).map_err(internal)
}

#[op2]
#[serde]
fn op_at_kv_list(
    state: &OpState,
    #[string] prefix: &str,
    #[smi] limit: u32,
) -> Result<Vec<String>, JsErrorBox> {
    let c = kv_ctx(state, None)?;
    let limit = usize::try_from(limit)
        .unwrap_or(usize::MAX)
        .clamp(1, 10_000);
    c.store
        .kv_list(&c.store_key, prefix, limit)
        .map_err(internal)
}

// --- Schedule, events, routes, roles ----------------------------------------------------

fn valid_job_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}

#[op2(fast)]
fn op_at_schedule(
    state: &mut OpState,
    #[string] name: &str,
    #[number] interval_ms: u64,
) -> Result<(), JsErrorBox> {
    let c = ctx(state);
    if !c.caps.schedule {
        return Err(not_capable(
            "scheduling needs the `schedule` capability, which the plugin's manifest doesn't \
             declare",
        ));
    }
    if !valid_job_name(name) {
        return Err(type_error(
            "job names are 1 to 64 letters, digits, `-`, `_`, `.` or `:`",
        ));
    }
    let min = c.config.min_schedule_interval;
    let max = Duration::from_secs(30 * 24 * 3600);
    let interval = Duration::from_millis(interval_ms);
    if interval < min || interval > max || interval.is_zero() {
        return Err(range_error(format!(
            "the interval must be between {} s and 30 days",
            min.as_secs().max(1)
        )));
    }
    // Pick up where the last run (perhaps before a restart) left off.
    let last = c.store.last_run(&c.store_key, name).map_err(internal)?;
    let interval_i = i64::try_from(interval.as_millis()).unwrap_or(i64::MAX);
    let first = last.map_or(Duration::ZERO, |last| {
        let due = last.saturating_add(interval_i) - now_ms();
        Duration::from_millis(u64::try_from(due.max(0)).unwrap_or(0))
    });
    let (tx, store, id, job) = (
        c.tx.clone(),
        c.store.clone(),
        c.store_key.clone(),
        name.to_owned(),
    );
    let task = tokio::spawn(async move {
        tokio::time::sleep(first).await;
        loop {
            if let Err(e) = store.set_last_run(&id, &job, now_ms()) {
                tracing::warn!(plugin = %id, error = %e, "could not record a scheduled run");
            }
            if tx.send(HostMsg::Schedule(job.clone())).is_err() {
                return;
            }
            tokio::time::sleep(interval).await;
        }
    });
    if let Some(old) = state
        .borrow_mut::<Tasks>()
        .schedules
        .insert(name.to_owned(), task.abort_handle())
    {
        old.abort();
    }
    Ok(())
}

#[op2(fast)]
fn op_at_unschedule(state: &mut OpState, #[string] name: &str) {
    if let Some(h) = state.borrow_mut::<Tasks>().schedules.remove(name) {
        h.abort();
    }
}

#[op2(fast)]
fn op_at_events(state: &mut OpState) -> Result<(), JsErrorBox> {
    let c = ctx(state);
    require(&c, PluginApiScope::EventsRead)?;
    if state.borrow::<Tasks>().events.is_some() {
        return Ok(());
    }
    let mut rx = c.app.events();
    let (tx, logs) = (c.tx.clone(), c.logs.clone());
    let task = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(e) => {
                    if tx.send(HostMsg::Event(e)).is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => logs.host(
                    PluginLogLevel::Warn,
                    &format!("missed {n} events: the plugin handled them too slowly"),
                ),
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    state.borrow_mut::<Tasks>().events = Some(task.abort_handle());
    Ok(())
}

#[op2(fast)]
fn op_at_route(
    state: &OpState,
    #[string] method: &str,
    #[string] path: &str,
) -> Result<(), JsErrorBox> {
    let c = ctx(state);
    let declared = c
        .caps
        .routes
        .iter()
        .any(|r| r.path == path && r.methods.iter().any(|m| m == method));
    if declared {
        Ok(())
    } else {
        Err(not_capable(format!(
            "route {method} {path} isn't declared in the plugin's manifest"
        )))
    }
}

/// A provided role object as the host keeps it: `{ data, methods }`, the object's plain
/// (non-function) properties and the names of its methods. Checked for what the host
/// reads from it.
fn role_data(role: PluginRole, provided: &str) -> Result<serde_json::Value, JsErrorBox> {
    let provided: serde_json::Value = from_json(provided, "role object")?;
    let data = provided.get("data").cloned().unwrap_or_default();
    if role == PluginRole::TrackerAdapter {
        let trackers = data
            .get("trackers")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                type_error("a tracker adapter names the trackers it covers: `trackers: [\"host\"]`")
            })?;
        for t in trackers {
            let ok = t
                .as_str()
                .is_some_and(crate::tracker::valid_tracker_pattern);
            if !ok {
                return Err(type_error(format!(
                    "tracker `{t}` must be a host name, or `*.` and a domain"
                )));
            }
        }
    }
    Ok(provided)
}

#[op2(fast)]
fn op_at_role(
    state: &OpState,
    #[string] role: &str,
    #[string] data: &str,
) -> Result<(), JsErrorBox> {
    let c = ctx(state);
    let parsed: PluginRole = serde_json::from_value(serde_json::Value::String(role.to_owned()))
        .map_err(|_| type_error(format!("unknown role `{role}`")))?;
    if !c.caps.roles.contains(&parsed) {
        return Err(not_capable(format!(
            "the `{role}` role isn't declared in the plugin's manifest"
        )));
    }
    let data = role_data(parsed, data)?;
    if let Some(trackers) = data
        .pointer("/data/trackers")
        .and_then(serde_json::Value::as_array)
    {
        let names: Vec<&str> = trackers
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        c.logs.host(
            PluginLogLevel::Info,
            &format!("adapts behaviour for trackers {}", names.join(", ")),
        );
    }
    c.run
        .roles
        .lock()
        .expect("roles lock poisoned")
        .insert(role.to_owned(), data);
    Ok(())
}

// --- App info and sign-in ---------------------------------------------------------------

#[op2]
#[string]
fn op_at_app_info(state: &OpState) -> Result<String, JsErrorBox> {
    let c = ctx(state);
    require(&c, PluginApiScope::TorrentsRead)?;
    to_json(&c.app.info())
}

/// An unguessable session id: 32 random bytes, in hex.
fn session_id() -> String {
    use rand::RngCore as _;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LoginResult {
    session: String,
    caller: PluginCaller,
}

/// Check an API token a client signed in with. On success the host keeps a session for it
/// and the plugin gets only the session's id.
#[op2]
#[string]
async fn op_at_auth_login(
    state: Rc<RefCell<OpState>>,
    #[string] token: String,
) -> Result<String, JsErrorBox> {
    let c = ctx(&state.borrow());
    require(&c, PluginApiScope::TokensVerify)?;
    let Some(creds) = c.config.credentials.clone() else {
        return to_json(&AppResult::<()>::from(Err(AppError::new(
            "unavailable",
            "authentication is turned off on this server, so there are no API tokens to check",
        ))));
    };
    // A token works in its own space only: the instance's (`default` for a global plugin).
    let space = c.app.space();
    let verified = c
        .main
        .spawn_blocking(move || creds.verify_token(&token, space))
        .await
        .map_err(internal)?;
    let result = match verified {
        Ok(Some(token)) => {
            let id = session_id();
            let caller = token.caller.clone();
            let mut st = state.borrow_mut();
            let sessions = &mut st.borrow_mut::<Sessions>().0;
            if sessions.len() >= MAX_SESSIONS
                && let Some(oldest) = sessions
                    .iter()
                    .min_by_key(|(_, s)| s.last_used)
                    .map(|(k, _)| k.clone())
            {
                sessions.remove(&oldest);
            }
            sessions.insert(
                id.clone(),
                Session {
                    token,
                    last_used: Instant::now(),
                },
            );
            Ok(Some(LoginResult {
                session: id,
                caller,
            }))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(e),
    };
    to_json(&AppResult::from(result))
}

/// The caller behind a session, or `null` when it ended: unknown, idle too long, or its
/// token was revoked or expired. Using a session keeps it alive.
#[op2]
#[string]
async fn op_at_auth_session(
    state: Rc<RefCell<OpState>>,
    #[string] id: String,
) -> Result<String, JsErrorBox> {
    let c = ctx(&state.borrow());
    require(&c, PluginApiScope::TokensVerify)?;
    let idle = c.config.session_idle;
    let token = {
        let mut st = state.borrow_mut();
        let sessions = &mut st.borrow_mut::<Sessions>().0;
        match sessions.get(&id) {
            Some(s) if s.last_used.elapsed() <= idle => Some(s.token.clone()),
            Some(_) => {
                sessions.remove(&id);
                None
            }
            None => None,
        }
    };
    let Some(token) = token else {
        return Ok("null".to_owned());
    };
    let check = token.clone();
    let valid = c
        .main
        .spawn_blocking(move || check.still_valid())
        .await
        .map_err(internal)?;
    let mut st = state.borrow_mut();
    let sessions = &mut st.borrow_mut::<Sessions>().0;
    if !valid {
        sessions.remove(&id);
        return Ok("null".to_owned());
    }
    if let Some(s) = sessions.get_mut(&id) {
        s.last_used = Instant::now();
    }
    to_json(&token.caller)
}

#[op2(fast)]
fn op_at_auth_logout(state: &mut OpState, #[string] id: &str) -> Result<bool, JsErrorBox> {
    require(&ctx(state), PluginApiScope::TokensVerify)?;
    Ok(state.borrow_mut::<Sessions>().0.remove(id).is_some())
}

// --- Torrents ---------------------------------------------------------------------------

#[op2]
#[string]
fn op_at_torrents_list(state: &OpState) -> Result<String, JsErrorBox> {
    let c = ctx(state);
    require(&c, PluginApiScope::TorrentsRead)?;
    to_json(&c.app.torrents())
}

#[op2]
#[string]
async fn op_at_torrent_get(
    state: Rc<RefCell<OpState>>,
    #[string] id: String,
) -> Result<String, JsErrorBox> {
    let c = ctx(&state.borrow());
    require(&c, PluginApiScope::TorrentsRead)?;
    on_main(&c.main, c.app.torrent(id)).await
}

/// A plugin may name a directory under the download directory, never anywhere else: plugins
/// get no filesystem access, and a save path is one.
fn check_save_path(p: &str) -> Result<(), JsErrorBox> {
    let path = std::path::Path::new(p);
    let relative = path
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)));
    if p.is_empty() || !relative || p.contains(':') {
        return Err(not_capable(format!(
            "savePath `{p}` must be a relative path inside the download directory, with no \
             `..`"
        )));
    }
    Ok(())
}

#[op2]
#[string]
async fn op_at_torrent_add(
    state: Rc<RefCell<OpState>>,
    #[string] req: String,
) -> Result<String, JsErrorBox> {
    let c = ctx(&state.borrow());
    require(&c, PluginApiScope::TorrentsAdd)?;
    let req: AddTorrentRequest = from_json(&req, "torrent")?;
    if let Some(p) = &req.save_path {
        check_save_path(p)?;
    }
    on_main(&c.main, c.app.add_torrent(req)).await
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
enum ControlArg {
    Pause,
    Resume,
    SetLabel {
        label: Option<String>,
    },
    SetSeedLimits {
        limits: Option<SeedLimits>,
    },
    #[serde(rename_all = "camelCase")]
    SetRateLimits {
        download: u64,
        upload: u64,
    },
    Queue {
        to: QueueMove,
    },
}

#[op2]
#[string]
async fn op_at_torrent_control(
    state: Rc<RefCell<OpState>>,
    #[string] id: String,
    #[string] action: String,
) -> Result<String, JsErrorBox> {
    let c = ctx(&state.borrow());
    require(&c, PluginApiScope::TorrentsControl)?;
    let action = match from_json::<ControlArg>(&action, "action")? {
        ControlArg::Pause => TorrentControl::Pause,
        ControlArg::Resume => TorrentControl::Resume,
        ControlArg::SetLabel { label } => TorrentControl::SetLabel(label),
        ControlArg::SetSeedLimits { limits } => {
            if let Some(l) = &limits
                && !(l.ratio.is_finite() && l.ratio >= 0.0)
            {
                return Err(range_error("a seed ratio is a number, 0 or more"));
            }
            TorrentControl::SetSeedLimits(limits)
        }
        ControlArg::SetRateLimits { download, upload } => {
            TorrentControl::SetRateLimits { download, upload }
        }
        ControlArg::Queue { to } => TorrentControl::Queue(to),
    };
    on_main(&c.main, c.app.control(id, action)).await
}

#[op2]
#[string]
async fn op_at_torrent_remove(
    state: Rc<RefCell<OpState>>,
    #[string] id: String,
    delete_files: bool,
) -> Result<String, JsErrorBox> {
    let c = ctx(&state.borrow());
    require(&c, PluginApiScope::TorrentsRemove)?;
    on_main(&c.main, c.app.remove(id, delete_files)).await
}

// --- Release candidates ----------------------------------------------------------------

/// Hand release candidates to the core's rules. Any plugin that fills the `source` role
/// may; no API scope is involved, since candidates only become torrents through rules an
/// admin wrote.
#[op2]
#[string]
async fn op_at_candidates_submit(
    state: Rc<RefCell<OpState>>,
    #[string] candidates: String,
) -> Result<String, JsErrorBox> {
    let c = ctx(&state.borrow());
    if !c.caps.roles.contains(&PluginRole::Source) {
        return Err(not_capable(
            "submitting release candidates needs the `source` role, which the plugin's              manifest doesn't declare",
        ));
    }
    let list: Vec<apitorrent_types::automation::ReleaseCandidate> =
        from_json(&candidates, "candidates")?;
    on_main(&c.main, c.app.submit_candidates(&c.id, list)).await
}

// --- fetch ------------------------------------------------------------------------------

#[derive(Serialize)]
struct FetchResult {
    meta: String,
    body: ToJsBuffer,
}

#[op2]
#[serde]
async fn op_at_fetch(
    state: Rc<RefCell<OpState>>,
    #[string] args: String,
    has_body: bool,
    #[buffer(copy)] body: Vec<u8>,
    #[smi] abort_id: u32,
) -> Result<FetchResult, JsErrorBox> {
    let c = ctx(&state.borrow());
    let args: FetchArgs = from_json(&args, "request")?;
    let net = c.net.read().expect("grants lock poisoned").clone();
    let fetcher = c.fetcher.clone();
    let task = c
        .main
        .spawn(async move { fetcher.fetch(&net, args, has_body.then_some(body)).await });
    if abort_id != 0 {
        state
            .borrow_mut()
            .borrow_mut::<Tasks>()
            .fetches
            .insert(abort_id, task.abort_handle());
    }
    let result = task.await;
    if abort_id != 0 {
        state
            .borrow_mut()
            .borrow_mut::<Tasks>()
            .fetches
            .remove(&abort_id);
    }
    match result {
        Ok(Ok((meta, body))) => Ok(FetchResult {
            meta: to_json(&meta)?,
            body: body.into(),
        }),
        Ok(Err(FetchError::NotCapable(m))) => Err(not_capable(m)),
        Ok(Err(FetchError::Failed(m))) => Err(type_error(format!("fetch failed: {m}"))),
        Err(e) if e.is_cancelled() => Err(JsErrorBox::new(
            "DOMExceptionAbortError",
            "the request was aborted",
        )),
        Err(e) => Err(internal(e)),
    }
}

#[op2(fast)]
fn op_at_fetch_abort(state: &mut OpState, #[smi] abort_id: u32) {
    if let Some(h) = state.borrow_mut::<Tasks>().fetches.remove(&abort_id) {
        h.abort();
    }
}

// --- URL, encoding, crypto --------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UrlParts {
    href: String,
    origin: String,
    protocol: String,
    username: String,
    password: String,
    host: String,
    hostname: String,
    port: String,
    pathname: String,
    search: String,
    hash: String,
}

fn parts(u: &url::Url) -> UrlParts {
    use url::quirks as q;
    UrlParts {
        href: q::href(u).to_owned(),
        origin: q::origin(u),
        protocol: q::protocol(u).to_owned(),
        username: q::username(u).to_owned(),
        password: q::password(u).to_owned(),
        host: q::host(u).to_owned(),
        hostname: q::hostname(u).to_owned(),
        port: q::port(u).to_owned(),
        pathname: q::pathname(u).to_owned(),
        search: q::search(u).to_owned(),
        hash: q::hash(u).to_owned(),
    }
}

#[op2]
#[string]
fn op_at_url_parse(#[string] href: &str, #[string] base: &str) -> Result<String, JsErrorBox> {
    let parsed = if base.is_empty() {
        url::Url::parse(href)
    } else {
        url::Url::parse(base).and_then(|b| b.join(href))
    };
    match parsed {
        Ok(u) => to_json(&parts(&u)),
        Err(_) => Ok(String::new()),
    }
}

#[op2]
#[string]
fn op_at_url_set(
    #[string] href: &str,
    #[string] part: &str,
    #[string] value: &str,
) -> Result<String, JsErrorBox> {
    use url::quirks as q;
    let mut u = url::Url::parse(href).map_err(|e| type_error(e.to_string()))?;
    match part {
        "href" => {
            q::set_href(&mut u, value).map_err(|e| type_error(format!("invalid URL: {e}")))?;
        }
        "protocol" => {
            let _ = q::set_protocol(&mut u, value);
        }
        "username" => {
            let _ = q::set_username(&mut u, value);
        }
        "password" => {
            let _ = q::set_password(&mut u, value);
        }
        "host" => {
            let _ = q::set_host(&mut u, value);
        }
        "hostname" => {
            let _ = q::set_hostname(&mut u, value);
        }
        "port" => {
            let _ = q::set_port(&mut u, value);
        }
        "pathname" => q::set_pathname(&mut u, value),
        "search" => q::set_search(&mut u, value),
        "hash" => q::set_hash(&mut u, value),
        other => return Err(type_error(format!("no URL part `{other}`"))),
    }
    to_json(&parts(&u))
}

#[op2]
#[string]
fn op_at_encoding_name(#[string] label: &str) -> String {
    encoding_rs::Encoding::for_label(label.trim().as_bytes())
        .filter(|e| *e != encoding_rs::REPLACEMENT)
        .map(|e| e.name().to_ascii_lowercase())
        .unwrap_or_default()
}

#[op2]
#[string]
fn op_at_text_decode(
    #[string] label: &str,
    #[buffer] data: &[u8],
    fatal: bool,
    ignore_bom: bool,
) -> Result<String, JsErrorBox> {
    let enc = encoding_rs::Encoding::for_label(label.as_bytes())
        .ok_or_else(|| range_error(format!("unknown encoding `{label}`")))?;
    let mut data = data;
    if !ignore_bom {
        let bom: &[u8] = if enc == encoding_rs::UTF_8 {
            b"\xEF\xBB\xBF"
        } else if enc == encoding_rs::UTF_16LE {
            b"\xFF\xFE"
        } else if enc == encoding_rs::UTF_16BE {
            b"\xFE\xFF"
        } else {
            b""
        };
        if !bom.is_empty() && data.starts_with(bom) {
            data = &data[bom.len()..];
        }
    }
    if fatal {
        enc.decode_without_bom_handling_and_without_replacement(data)
            .map(std::borrow::Cow::into_owned)
            .ok_or_else(|| type_error(format!("the data is not valid {}", enc.name())))
    } else {
        Ok(enc.decode_without_bom_handling(data).0.into_owned())
    }
}

#[op2(fast)]
fn op_at_random(#[buffer] buf: &mut [u8]) {
    use rand::RngCore as _;
    rand::rng().fill_bytes(buf);
}

#[op2]
#[string]
fn op_at_uuid() -> String {
    use rand::RngCore as _;
    let mut b = [0u8; 16];
    rand::rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

fn digest_of(alg: &str, data: &[u8]) -> Result<Vec<u8>, JsErrorBox> {
    use sha2::Digest as _;
    Ok(match alg.to_ascii_uppercase().as_str() {
        "SHA-1" => sha1::Sha1::digest(data).to_vec(),
        "SHA-256" => sha2::Sha256::digest(data).to_vec(),
        "SHA-384" => sha2::Sha384::digest(data).to_vec(),
        "SHA-512" => sha2::Sha512::digest(data).to_vec(),
        other => {
            return Err(JsErrorBox::new(
                "DOMExceptionNotSupportedError",
                format!("unsupported digest `{other}`"),
            ));
        }
    })
}

#[op2]
#[buffer]
fn op_at_digest(#[string] alg: &str, #[buffer] data: &[u8]) -> Result<Vec<u8>, JsErrorBox> {
    digest_of(alg, data)
}

fn hmac_of(alg: &str, key: &[u8], data: &[u8]) -> Result<Vec<u8>, JsErrorBox> {
    use hmac::{Mac, SimpleHmac};
    fn run<D: sha2::Digest + sha2::digest::core_api::BlockSizeUser>(
        key: &[u8],
        data: &[u8],
    ) -> Vec<u8> {
        let mut mac = SimpleHmac::<D>::new_from_slice(key).expect("HMAC takes any key length");
        mac.update(data);
        mac.finalize().into_bytes().to_vec()
    }
    Ok(match alg.to_ascii_uppercase().as_str() {
        "SHA-1" => run::<sha1::Sha1>(key, data),
        "SHA-256" => run::<sha2::Sha256>(key, data),
        "SHA-384" => run::<sha2::Sha384>(key, data),
        "SHA-512" => run::<sha2::Sha512>(key, data),
        other => {
            return Err(JsErrorBox::new(
                "DOMExceptionNotSupportedError",
                format!("unsupported HMAC hash `{other}`"),
            ));
        }
    })
}

#[op2]
#[buffer]
fn op_at_hmac(
    #[string] alg: &str,
    #[buffer] key: &[u8],
    #[buffer] data: &[u8],
) -> Result<Vec<u8>, JsErrorBox> {
    hmac_of(alg, key, data)
}

#[op2(fast)]
fn op_at_hmac_verify(
    #[string] alg: &str,
    #[buffer] key: &[u8],
    #[buffer] signature: &[u8],
    #[buffer] data: &[u8],
) -> Result<bool, JsErrorBox> {
    let expected = hmac_of(alg, key, data)?;
    // Constant time in the signature's contents.
    let same = expected.len() == signature.len()
        && expected
            .iter()
            .zip(signature)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0;
    Ok(same)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_paths_stay_inside_the_download_directory() {
        for ok in ["tv", "tv/show", "a b/c"] {
            check_save_path(ok).unwrap();
        }
        for bad in [
            "",
            "/etc",
            "../x",
            "tv/../../x",
            "C:\\x",
            "C:x",
            "\\\\server\\share",
            "./x",
        ] {
            assert!(check_save_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn hmac_and_digest_known_answers() {
        // RFC 4231 test case 2.
        let mac = hmac_of("SHA-256", b"Jefe", b"what do ya want for nothing?").unwrap();
        assert_eq!(
            hex::encode(mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex::encode(digest_of("sha-1", b"abc").unwrap()),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert!(digest_of("MD5", b"").is_err());
    }
}

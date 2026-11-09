//! One plugin's thread: its isolate, its event loop, and how it ends.
//!
//! Each plugin runs on its own OS thread with a current-thread tokio runtime and one
//! `JsRuntime`. The host talks to it through [`HostMsg`]s, which the plugin's JavaScript
//! pulls with an async op. Every turn of the event loop is timed: the watchdog
//! terminates a turn that runs too long, and the heap-limit callback terminates a plugin
//! that runs out of memory, each recording why.

use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::Poll;
use std::time::{Duration, Instant};

use deno_core::{JsRuntime, ModuleSpecifier, NoopModuleLoader, PollEventLoopOptions, v8};
use tokio::sync::{Notify, oneshot};

use apitorrent_plugin_api::{AppEvent, PluginError, PluginHttpRequest, PluginHttpResponse};

use crate::alloc;
use crate::ops::{self, PluginCtx, StartFn};

/// Messages from the host to a running plugin.
#[derive(Debug)]
pub(crate) enum HostMsg {
    Event(AppEvent),
    Schedule(String),
    Http(
        PluginHttpRequest,
        oneshot::Sender<Result<PluginHttpResponse, PluginError>>,
    ),
    Role {
        role: String,
        method: String,
        args: serde_json::Value,
        reply: oneshot::Sender<Result<serde_json::Value, String>>,
    },
    Settings(serde_json::Value),
    Stop,
}

/// Milliseconds on a process-wide monotonic clock, starting at 1 (0 means "idle").
pub(crate) fn mono_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let epoch = *EPOCH.get_or_init(Instant::now);
    u64::try_from(epoch.elapsed().as_millis()).unwrap_or(u64::MAX) + 1
}

/// State shared between a run's thread, the host and the watchdog.
#[derive(Debug)]
pub(crate) struct RunShared {
    pub generation: u64,
    /// When the current event-loop turn began (`mono_ms`), or 0 between turns.
    pub busy_since: AtomicU64,
    /// Why the host terminated this run, if it did.
    pub kill_reason: Mutex<Option<String>>,
    /// The plugin's `start` resolved.
    pub started: AtomicBool,
    /// The plugin finished its `stop` hook, or the host gave up waiting.
    pub stop_done: AtomicBool,
    pub stop_notify: Notify,
    /// Each role the plugin provides, with the plain data of its object (a tracker
    /// adapter's `trackers`, say), as JSON.
    pub roles: Mutex<std::collections::HashMap<String, serde_json::Value>>,
}

impl RunShared {
    pub fn new(generation: u64) -> Self {
        Self {
            generation,
            busy_since: AtomicU64::new(0),
            kill_reason: Mutex::new(None),
            started: AtomicBool::new(false),
            stop_done: AtomicBool::new(false),
            stop_notify: Notify::new(),
            roles: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Record why the run is being killed; the first reason wins.
    pub fn set_kill_reason(&self, reason: String) {
        let mut k = self.kill_reason.lock().expect("kill reason lock poisoned");
        if k.is_none() {
            *k = Some(reason);
        }
    }

    pub fn kill_reason(&self) -> Option<String> {
        self.kill_reason
            .lock()
            .expect("kill reason lock poisoned")
            .clone()
    }

    /// The data of a role the plugin provides, if it provides it.
    pub fn role(&self, role: &str) -> Option<serde_json::Value> {
        self.roles
            .lock()
            .expect("roles lock poisoned")
            .get(role)
            .cloned()
    }

    pub fn finish_stop(&self) {
        self.stop_done.store(true, Ordering::Release);
        self.stop_notify.notify_one();
    }
}

/// Marks the isolate busy for as long as it lives.
struct Turn<'a>(&'a AtomicU64);

impl<'a> Turn<'a> {
    fn begin(busy: &'a AtomicU64) -> Self {
        busy.store(mono_ms(), Ordering::Release);
        Self(busy)
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.0.store(0, Ordering::Release);
    }
}

/// Limits for one isolate.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub heap: usize,
    pub buffers: usize,
}

/// How a run ended.
pub(crate) type Exit = Result<(), String>;

/// Start a plugin's thread. Returns once its isolate exists, with the handle the watchdog
/// and the host use to terminate it. `on_exit` runs on the plugin's thread when it ends.
pub(crate) fn spawn(
    ctx: PluginCtx,
    code: String,
    limits: Limits,
    on_exit: Box<dyn FnOnce(Exit) + Send>,
) -> std::io::Result<v8::IsolateHandle> {
    let (handle_tx, handle_rx) = std::sync::mpsc::sync_channel(1);
    let name = format!("plugin-{}", ctx.id);
    std::thread::Builder::new()
        .name(name)
        // V8 wants a generous native stack for deep recursion before it throws.
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(ctx, code, limits, &handle_tx)
            }))
            .unwrap_or_else(|p| {
                let msg = p
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_owned())
                    .or_else(|| p.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_owned());
                Err(format!("the plugin host panicked: {msg}"))
            });
            on_exit(result);
        })?;
    handle_rx
        .recv()
        .map_err(|_| std::io::Error::other("the plugin thread ended before creating its isolate"))
}

fn run(
    ctx: PluginCtx,
    code: String,
    limits: Limits,
    handle_tx: &std::sync::mpsc::SyncSender<v8::IsolateHandle>,
) -> Exit {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("could not start the plugin's runtime: {e}"))?;
    // V8 posts delayed tasks to the runtime that is current when the isolate is created.
    let _guard = rt.enter();
    let shared = ctx.run.clone();
    let id = ctx.id.clone();
    let version = ctx.version.clone();
    let (allocator, budget) = alloc::budgeted(limits.buffers);
    // V8 counts ArrayBuffer memory toward its heap limit when it decides it is out of
    // memory, so its limit is the heap plus the buffer budget; the allocator alone caps the
    // buffers. Objects can therefore use the heap limit plus whatever buffer budget is
    // unused, and buffers never pass their budget.
    let params = v8::CreateParams::default()
        .heap_limits(0, limits.heap.saturating_add(limits.buffers))
        .array_buffer_allocator(allocator);
    let mut js = JsRuntime::try_new(deno_core::RuntimeOptions {
        extensions: vec![ops::apitorrent_plugin::init(ctx)],
        module_loader: Some(Rc::new(NoopModuleLoader)),
        create_params: Some(params),
        ..Default::default()
    })
    .map_err(|e| format!("could not create the isolate: {e}"))?;
    let isolate = js.v8_isolate().thread_safe_handle();
    {
        let isolate = isolate.clone();
        let shared = shared.clone();
        let heap_mib = limits.heap / (1024 * 1024);
        js.add_near_heap_limit_callback(move |current, _initial| {
            // A refused buffer allocation lands here too; the plugin gets a RangeError for
            // it and carries on, and the heap limit stays where it is.
            if budget.take_refusal() {
                return current;
            }
            shared.set_kill_reason(format!(
                "it used up its memory ({heap_mib} MiB JavaScript heap limit)"
            ));
            isolate.terminate_execution();
            // Room to unwind; the run ends right after.
            current.saturating_mul(2)
        });
    }
    let _ = handle_tx.send(isolate);

    let result = rt.block_on(async {
        let spec = ModuleSpecifier::parse(&format!("plugin://{id}/{version}/plugin.js"))
            .map_err(|e| format!("bad module name: {e}"))?;
        let module = {
            let _turn = Turn::begin(&shared.busy_since);
            js.load_main_es_module_from_code(&spec, code)
                .await
                .map_err(|e| format!("the plugin's code doesn't load: {e}"))?
        };
        let evaluated = {
            let _turn = Turn::begin(&shared.busy_since);
            js.mod_evaluate(module)
        };
        settled(drive(&mut js, &shared, Some(Box::pin(evaluated))).await)
            .map_err(|e| format!("the plugin's code failed: {e}"))?;
        let namespace = js
            .get_module_namespace(module)
            .map_err(|e| format!("the plugin's module has no namespace: {e}"))?;
        let start = js
            .op_state()
            .borrow_mut()
            .try_take::<StartFn>()
            .ok_or("the plugin runtime didn't initialise")?;
        let namespace: v8::Global<v8::Value> = {
            deno_core::scope!(scope, &mut js);
            let local: v8::Local<v8::Value> = v8::Local::new(scope, &namespace).into();
            v8::Global::new(scope, local)
        };
        let started = {
            let _turn = Turn::begin(&shared.busy_since);
            js.call_with_args(&start.0, &[namespace])
        };
        settled(drive(&mut js, &shared, Some(Box::pin(started))).await)
            .map_err(|e| format!("start() failed: {e}"))?;
        // From here the plugin waits for messages until it is told to stop.
        match drive::<std::future::Pending<Result<(), String>>>(&mut js, &shared, None).await {
            Ok(_) | Err(_) if shared.stop_done.load(Ordering::Acquire) => Ok(()),
            Err(e) => Err(e),
            Ok(_) => Err("the plugin stopped on its own".to_owned()),
        }
    });
    // A terminated isolate reports only "execution terminated"; say why.
    match (result, shared.kill_reason()) {
        (Err(_), Some(reason)) => Err(reason),
        (r, _) => r,
    }
}

/// The outcome of driving a JavaScript future to completion: its own error, the event
/// loop's, or a stop that came first.
fn settled<T, E: std::fmt::Display>(r: Result<Option<Result<T, E>>, String>) -> Result<(), String> {
    match r? {
        Some(Ok(_)) => Ok(()),
        Some(Err(e)) => Err(e.to_string()),
        None => Err("it was stopped before it finished starting".to_owned()),
    }
}

/// Run the event loop, with every turn timed, until `until` resolves (returning its
/// output), the loop has nothing left to do (`Ok(None)`), it fails, or the run is told to
/// stop (`Ok(None)` with `stop_done` set).
async fn drive<F>(
    js: &mut JsRuntime,
    shared: &Arc<RunShared>,
    mut until: Option<std::pin::Pin<Box<F>>>,
) -> Result<Option<F::Output>, String>
where
    F: Future,
{
    let stop = shared.stop_notify.notified();
    tokio::pin!(stop);
    std::future::poll_fn(|cx| {
        if shared.stop_done.load(Ordering::Acquire) || stop.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Ok(None));
        }
        let _turn = Turn::begin(&shared.busy_since);
        if let Some(f) = until.as_mut()
            && let Poll::Ready(out) = f.as_mut().poll(cx)
        {
            return Poll::Ready(Ok(Some(out)));
        }
        match js.poll_event_loop(cx, PollEventLoopOptions::default()) {
            Poll::Ready(Err(e)) => Poll::Ready(Err(e.to_string())),
            Poll::Ready(Ok(())) => match until.as_mut().map(|f| f.as_mut().poll(cx)) {
                Some(Poll::Ready(out)) => Poll::Ready(Ok(Some(out))),
                Some(Poll::Pending) => Poll::Ready(Err(
                    "the plugin is waiting on a promise that can never settle".to_owned(),
                )),
                None => Poll::Ready(Ok(None)),
            },
            Poll::Pending => Poll::Pending,
        }
    })
    .await
}

/// How long the host waits for a plugin's `stop` hook before ending it anyway.
pub(crate) const STOP_GRACE: Duration = Duration::from_secs(5);

#[cfg(test)]
mod tests {
    /// `deno_core` panics at isolate creation on non-ASCII extension code; catch it here.
    #[test]
    fn extension_code_is_ascii() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/js");
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let text = std::fs::read_to_string(&path).unwrap();
            if let Some((line, _)) = text.lines().enumerate().find(|(_, l)| !l.is_ascii()) {
                panic!("{} line {} isn't ASCII", path.display(), line + 1);
            }
        }
    }
}

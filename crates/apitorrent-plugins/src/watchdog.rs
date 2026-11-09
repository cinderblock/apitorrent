//! Terminates a plugin whose JavaScript runs too long without yielding.
//!
//! A plugin's thread stamps the start of every event-loop turn (module evaluation, a
//! callback, a resolved promise) in [`RunShared::busy_since`]. One watchdog thread per host
//! checks those stamps; a turn older than the limit gets `terminate_execution` and a
//! recorded reason. Waiting (on timers, `fetch`, messages) is not a turn, so only code that
//! hogs the thread counts.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use deno_core::v8;

use crate::runtime::{RunShared, mono_ms};

const TICK: Duration = Duration::from_millis(20);

struct Watched {
    run: Weak<RunShared>,
    isolate: v8::IsolateHandle,
}

#[derive(Default)]
struct Shared {
    watched: Mutex<Vec<Watched>>,
    stop: AtomicBool,
}

pub(crate) struct Watchdog {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Watchdog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watchdog").finish_non_exhaustive()
    }
}

impl Watchdog {
    pub fn start(limit: Duration) -> std::io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let thread_shared = shared.clone();
        let limit_ms = u64::try_from(limit.as_millis()).unwrap_or(u64::MAX).max(1);
        std::thread::Builder::new()
            .name("plugin-watchdog".to_owned())
            .spawn(move || {
                while !thread_shared.stop.load(Ordering::Acquire) {
                    std::thread::sleep(TICK);
                    let now = mono_ms();
                    let mut watched = thread_shared
                        .watched
                        .lock()
                        .expect("watchdog lock poisoned");
                    watched.retain(|w| {
                        let Some(run) = w.run.upgrade() else {
                            return false;
                        };
                        let since = run.busy_since.load(Ordering::Acquire);
                        if since != 0 && now.saturating_sub(since) > limit_ms {
                            run.set_kill_reason(format!(
                                "the watchdog stopped it: its code ran for more than {} ms \
                                 without yielding",
                                limit.as_millis()
                            ));
                            w.isolate.terminate_execution();
                        }
                        true
                    });
                }
            })?;
        Ok(Self { shared })
    }

    pub fn watch(&self, run: &Arc<RunShared>, isolate: v8::IsolateHandle) {
        self.shared
            .watched
            .lock()
            .expect("watchdog lock poisoned")
            .push(Watched {
                run: Arc::downgrade(run),
                isolate,
            });
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
    }
}

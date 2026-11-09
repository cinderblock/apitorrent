//! Desktop notifications when a torrent finishes, from a sync socket of the app's own (so
//! they arrive while the window is hidden in the tray, too).

use std::time::Duration;

use futures_util::StreamExt as _;
use tauri::{AppHandle, Runtime};
use tauri_plugin_notification::NotificationExt as _;
use tokio_tungstenite::tungstenite::Message;

use apitorrent_types::SyncMessage;

use crate::backend::Backend;
use crate::finish::FinishWatcher;
use crate::state::AppState;

const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Start watching `backend` (replacing any earlier watcher).
pub fn watch<R: Runtime>(app: &AppHandle<R>, state: &AppState, backend: Backend) {
    let app = app.clone();
    let task = tokio::spawn(async move {
        let mut watcher = FinishWatcher::default();
        let mut backoff = Duration::from_secs(1);
        loop {
            match backend.sync().await {
                Ok(mut socket) => {
                    backoff = Duration::from_secs(1);
                    while let Some(Ok(msg)) = socket.next().await {
                        let Message::Text(text) = msg else { continue };
                        let Ok(msg) = serde_json::from_str::<SyncMessage>(text.as_str()) else {
                            continue;
                        };
                        for name in watcher.feed(&msg) {
                            let shown = app
                                .notification()
                                .builder()
                                .title("Download finished")
                                .body(name)
                                .show();
                            if let Err(e) = shown {
                                tracing::warn!(error = %e, "couldn't show a notification");
                            }
                        }
                    }
                }
                Err(e) => tracing::debug!(error = %e, "notification watcher can't connect yet"),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    });
    if let Some(old) = state
        .watcher
        .lock()
        .expect("not poisoned")
        .replace(task.abort_handle())
    {
        old.abort();
    }
}

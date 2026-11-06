//! The `/api/v1/sync` WebSocket: a snapshot, then patches.

use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast::error::RecvError;

use apitorrent_types::SyncMessage;

use crate::AppState;

const PING_EVERY: Duration = Duration::from_secs(30);

pub(crate) async fn handler(ws: WebSocketUpgrade, State(s): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| run(socket, s))
}

fn encode(msg: &SyncMessage) -> Message {
    Message::Text(
        serde_json::to_string(msg)
            .expect("sync messages always serialise")
            .into(),
    )
}

async fn run(socket: WebSocket, s: AppState) {
    let (mut tx, mut rx) = socket.split();
    let (snapshot, mut patches) = s.core.subscribe();
    let first = SyncMessage::Snapshot {
        revision: snapshot.revision,
        torrents: snapshot.torrents,
    };
    if tx.send(encode(&first)).await.is_err() {
        return;
    }
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    loop {
        tokio::select! {
            patch = patches.recv() => {
                let msg = match patch {
                    Ok(p) => SyncMessage::Patch {
                        revision: p.revision,
                        upserted: p.upserted.clone(),
                        removed: p.removed.clone(),
                    },
                    Err(RecvError::Lagged(_)) => {
                        // Too far behind to catch up with patches; start over from a snapshot.
                        let (snapshot, fresh) = s.core.subscribe();
                        patches = fresh;
                        SyncMessage::Snapshot {
                            revision: snapshot.revision,
                            torrents: snapshot.torrents,
                        }
                    }
                    Err(RecvError::Closed) => break,
                };
                if tx.send(encode(&msg)).await.is_err() {
                    break;
                }
            }
            incoming = rx.next() => match incoming {
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                // The protocol is server-to-client only for now.
                Some(Ok(_)) => {}
            },
            _ = ping.tick() => {
                if tx.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }
}

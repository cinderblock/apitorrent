//! The `/api/v1/sync` WebSocket: a snapshot, then patches.
//!
//! The upgrade is authenticated like any other request. Because the socket can stay open
//! for days, its credential is checked again whenever any session or token is revoked
//! (sign-out, revocation, rotation on sign-in) and on every ping, which catches expiry. A
//! credential that is no longer valid closes the socket with code 4401.

use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, State};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast::error::RecvError;

use apitorrent_auth::Principal;
use apitorrent_types::SyncMessage;

use crate::authn::RequestCtx;
use crate::{AppState, AuthMode};

const PING_EVERY: Duration = Duration::from_secs(30);

/// Close code: the credential the socket was opened with is no longer valid.
pub const CLOSE_UNAUTHORIZED: u16 = 4401;

pub(crate) async fn handler(
    ws: WebSocketUpgrade,
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
) -> Response {
    let principal = ctx.principal;
    ws.on_upgrade(move |socket| run(socket, s, principal))
}

fn encode(msg: &SyncMessage) -> Message {
    Message::Text(
        serde_json::to_string(msg)
            .expect("sync messages always serialise")
            .into(),
    )
}

fn still_valid(s: &AppState, principal: Option<&Principal>) -> bool {
    match (&s.auth, principal) {
        (AuthMode::DisabledOnLoopback, _) => true,
        (AuthMode::Enabled(auth), Some(p)) => auth.still_valid(p).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "could not re-check a sync credential");
            false
        }),
        (AuthMode::Enabled(_), None) => false,
    }
}

async fn run(socket: WebSocket, s: AppState, principal: Option<Principal>) {
    let (mut tx, mut rx) = socket.split();
    let (snapshot, mut patches) = s.core.subscribe();
    let first = SyncMessage::Snapshot {
        revision: snapshot.revision,
        torrents: snapshot.torrents,
    };
    if tx.send(encode(&first)).await.is_err() {
        return;
    }
    let mut revoked = match &s.auth {
        AuthMode::Enabled(auth) => Some(auth.revocations()),
        AuthMode::DisabledOnLoopback => None,
    };
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    loop {
        let recheck = tokio::select! {
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
                false
            }
            incoming = rx.next() => match incoming {
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                // The protocol is server-to-client only for now.
                Some(Ok(_)) => false,
            },
            changed = async { revoked.as_mut().expect("guarded").changed().await },
                if revoked.is_some() =>
            {
                if changed.is_err() {
                    // The auth service is gone; nothing can be checked any more.
                    break;
                }
                true
            }
            _ = ping.tick() => {
                if tx.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
                true
            }
        };
        if recheck && !still_valid(&s, principal.as_ref()) {
            let _ = tx
                .send(Message::Close(Some(CloseFrame {
                    code: CLOSE_UNAUTHORIZED,
                    reason: "credential no longer valid".into(),
                })))
                .await;
            break;
        }
    }
}

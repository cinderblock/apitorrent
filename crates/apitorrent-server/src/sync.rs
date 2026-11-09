//! The `/api/v1/sync` WebSocket: a snapshot, then patches.
//!
//! The snapshot holds every torrent (about 6 MiB of JSON for 10,000), and neither axum nor
//! tungstenite offers `permessage-deflate`. So a client that asks with `?compress=gzip`
//! gets a large snapshot as one gzip-compressed binary message instead, which browsers
//! unpack with `DecompressionStream`. Patches stay plain text.
//!
//! The upgrade is authenticated like any other request. Because the socket can stay open
//! for days, its credential is checked again whenever any session or token is revoked
//! (sign-out, revocation, rotation on sign-in) and on every ping, which catches expiry. A
//! credential that is no longer valid closes the socket with code 4401.

use std::time::Duration;

use std::io::Write as _;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, Query, State};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast::error::RecvError;

use apitorrent_auth::Principal;
use apitorrent_types::SyncMessage;

use crate::authn::RequestCtx;
use crate::{AppState, AuthMode};

const PING_EVERY: Duration = Duration::from_secs(30);

/// Messages smaller than this go out as text even when compression was asked for.
const COMPRESS_FROM: usize = 32 * 1024;

#[derive(Debug, serde::Deserialize)]
pub(crate) struct SyncQuery {
    /// `gzip`: send large messages gzip-compressed, as binary.
    compress: Option<String>,
}

/// Close code: the credential the socket was opened with is no longer valid.
pub const CLOSE_UNAUTHORIZED: u16 = 4401;

pub(crate) async fn handler(
    ws: WebSocketUpgrade,
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Query(q): Query<SyncQuery>,
) -> Response {
    let principal = ctx.principal;
    let gzip = q.compress.as_deref() == Some("gzip");
    ws.on_upgrade(move |socket| run(socket, s, principal, gzip))
}

fn encode(msg: &SyncMessage, gzip: bool) -> Message {
    let json = serde_json::to_string(msg).expect("sync messages always serialise");
    if gzip && json.len() >= COMPRESS_FROM {
        let mut z = flate2::write::GzEncoder::new(
            Vec::with_capacity(json.len() / 8),
            flate2::Compression::fast(),
        );
        if z.write_all(json.as_bytes()).is_ok()
            && let Ok(bytes) = z.finish()
        {
            return Message::Binary(bytes.into());
        }
    }
    Message::Text(json.into())
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

async fn run(socket: WebSocket, s: AppState, principal: Option<Principal>, gzip: bool) {
    let (mut tx, mut rx) = socket.split();
    let (snapshot, mut patches) = s.core.subscribe();
    let first = SyncMessage::Snapshot {
        revision: snapshot.revision,
        torrents: snapshot.torrents,
    };
    if tx.send(encode(&first, gzip)).await.is_err() {
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
                if tx.send(encode(&msg, gzip)).await.is_err() {
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

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use super::*;

    fn patch(ids: usize) -> SyncMessage {
        SyncMessage::Patch {
            revision: 7,
            upserted: Vec::new(),
            removed: (0..ids).map(|i| format!("{i:040x}")).collect(),
        }
    }

    #[test]
    fn large_messages_are_gzipped_only_when_asked() {
        let big = patch(2000);
        let json = serde_json::to_string(&big).unwrap();
        assert!(json.len() > COMPRESS_FROM);
        let Message::Binary(bytes) = encode(&big, true) else {
            panic!("a large message is binary when gzip was asked for");
        };
        assert!(
            bytes.len() < json.len() / 2,
            "{} of {}",
            bytes.len(),
            json.len()
        );
        let mut unpacked = String::new();
        flate2::read::GzDecoder::new(&bytes[..])
            .read_to_string(&mut unpacked)
            .unwrap();
        assert_eq!(unpacked, json);

        assert!(matches!(encode(&big, false), Message::Text(_)), "not asked");
        assert!(matches!(encode(&patch(3), true), Message::Text(_)), "small");
    }
}

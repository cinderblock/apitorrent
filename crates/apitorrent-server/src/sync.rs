//! The `/api/v1/sync` WebSocket: a snapshot, then patches.
//!
//! The snapshot holds every torrent (about 6 MiB of JSON for 10,000), and neither axum nor
//! tungstenite offers `permessage-deflate`. So a client that asks with `?compress=gzip`
//! gets a large snapshot as one gzip-compressed binary message instead, which browsers
//! unpack with `DecompressionStream`. Patches stay plain text.
//!
//! A client that asks with `?chunks=1` gets the snapshot in parts instead, newest-added
//! first: a small first part so the list shows at once, then larger ones.
//!
//! The upgrade is authenticated like any other request. Because the socket can stay open
//! for days, its credential is checked again whenever any session or token is revoked
//! (sign-out, revocation, rotation on sign-in) and on every ping, which catches expiry. A
//! credential that is no longer valid closes the socket with code 4401.

use std::time::Duration;

use std::io::Write as _;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, Query, State};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast::error::RecvError;

use apitorrent_auth::Principal;
use apitorrent_types::SyncMessage;
use apitorrent_types::spaces::SpaceId;

use crate::authn::RequestCtx;
use crate::{AppState, AuthMode};

const PING_EVERY: Duration = Duration::from_secs(30);

/// Messages smaller than this go out as text even when compression was asked for.
const COMPRESS_FROM: usize = 32 * 1024;

/// Torrents in a snapshot's first part, and in each part after it.
const FIRST_PART: usize = 100;
const PART: usize = 1000;

#[derive(Debug, serde::Deserialize)]
pub(crate) struct SyncQuery {
    /// `gzip`: send large messages gzip-compressed, as binary.
    compress: Option<String>,
    /// `all`: every space's torrents (server admins only); otherwise the request's space.
    space: Option<String>,
    /// `1`: send a large snapshot in parts.
    chunks: Option<String>,
}

/// Close code: the credential the socket was opened with is no longer valid.
pub const CLOSE_UNAUTHORIZED: u16 = 4401;

pub(crate) async fn handler(
    ws: WebSocketUpgrade,
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Query(q): Query<SyncQuery>,
) -> Response {
    let space = if q.space.as_deref() == Some("all") {
        if let Err(e) = crate::space::server_admin(&ctx) {
            return e.into_response();
        }
        None
    } else {
        match &ctx.space {
            Some(here) => Some(here.id),
            None => {
                return crate::routes::error(
                    axum::http::StatusCode::NOT_FOUND,
                    "no_such_space",
                    "no such space",
                )
                .into_response();
            }
        }
    };
    let principal = ctx.principal;
    let gzip = q.compress.as_deref() == Some("gzip");
    let chunks = q.chunks.as_deref() == Some("1");
    ws.on_upgrade(move |socket| run(socket, s, principal, space, gzip, chunks))
}

/// A snapshot as messages: one, or (`chunks`) newest-added first in parts.
fn snapshot_messages(snapshot: apitorrent_core::Snapshot, chunks: bool) -> Vec<SyncMessage> {
    let apitorrent_core::Snapshot {
        revision,
        mut torrents,
    } = snapshot;
    if !chunks || torrents.len() <= FIRST_PART {
        return vec![SyncMessage::Snapshot {
            revision,
            torrents,
            total: None,
        }];
    }
    torrents.sort_by_key(|t| std::cmp::Reverse(t.added_at));
    let total = u32::try_from(torrents.len()).unwrap_or(u32::MAX);
    let mut rest = torrents.split_off(FIRST_PART);
    let mut out = vec![SyncMessage::Snapshot {
        revision,
        torrents,
        total: Some(total),
    }];
    while !rest.is_empty() {
        let tail = rest.split_off(rest.len().min(PART));
        out.push(SyncMessage::SnapshotPart { torrents: rest });
        rest = tail;
    }
    out
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

/// The patch as one socket sees it: its space's part, or (`None`) everything.
fn patch_message(p: &apitorrent_core::Patch, space: Option<SpaceId>) -> Option<SyncMessage> {
    let p = p.for_space(space)?;
    Some(SyncMessage::Patch {
        revision: p.revision,
        upserted: p.upserted,
        removed: p.removed.iter().map(|k| k.id.clone()).collect(),
        removed_spaces: if space.is_none() {
            p.removed.iter().map(|k| k.space).collect()
        } else {
            Vec::new()
        },
    })
}

async fn run(
    socket: WebSocket,
    s: AppState,
    principal: Option<Principal>,
    space: Option<SpaceId>,
    gzip: bool,
    chunks: bool,
) {
    let (mut tx, mut rx) = socket.split();
    let (snapshot, mut patches) = s.core.subscribe(space);
    for msg in snapshot_messages(snapshot, chunks) {
        if tx.send(encode(&msg, gzip)).await.is_err() {
            return;
        }
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
                let msgs = match patch {
                    Ok(p) => match patch_message(&p, space) {
                        Some(m) => vec![m],
                        None => continue,
                    },
                    Err(RecvError::Lagged(_)) => {
                        // Too far behind to catch up with patches; start over from a snapshot.
                        let (snapshot, fresh) = s.core.subscribe(space);
                        patches = fresh;
                        snapshot_messages(snapshot, chunks)
                    }
                    Err(RecvError::Closed) => break,
                };
                let mut sent = true;
                for msg in &msgs {
                    if tx.send(encode(msg, gzip)).await.is_err() {
                        sent = false;
                        break;
                    }
                }
                if !sent {
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
            removed_spaces: Vec::new(),
        }
    }

    fn summaries(n: usize) -> Vec<apitorrent_types::TorrentSummary> {
        (0..n)
            .map(|i| {
                serde_json::from_value(serde_json::json!({
                    "id": format!("{i:040x}"), "name": format!("t{i}"), "infoHashV1": null,
                    "infoHashV2": null, "state": "paused", "progress": 0.0, "size": 1,
                    "piecesHave": 0, "piecesTotal": 1, "downloaded": 0, "uploaded": 0,
                    "downloadRate": 0, "uploadRate": 0, "peers": 0, "seeds": 0,
                    "addedAt": i, "label": null, "savePath": "/x", "private": false,
                    "error": null, "queuePosition": 0, "group": null, "supersededBy": null,
                }))
                .expect("a summary")
            })
            .collect()
    }

    fn snapshot(n: usize) -> apitorrent_core::Snapshot {
        apitorrent_core::Snapshot {
            revision: 9,
            torrents: summaries(n),
        }
    }

    #[test]
    fn a_snapshot_comes_in_parts_only_when_asked() {
        let one = snapshot_messages(snapshot(2500), false);
        assert!(matches!(
            &one[..],
            [SyncMessage::Snapshot { torrents, total: None, .. }] if torrents.len() == 2500
        ));

        let small = snapshot_messages(snapshot(FIRST_PART), true);
        assert!(matches!(
            &small[..],
            [SyncMessage::Snapshot { total: None, .. }]
        ));

        let parts = snapshot_messages(snapshot(2500), true);
        let SyncMessage::Snapshot {
            revision: 9,
            torrents: first,
            total: Some(2500),
        } = &parts[0]
        else {
            panic!("a snapshot with its total first: {:?}", parts[0]);
        };
        let mut sizes = vec![first.len()];
        let mut added: Vec<i64> = first.iter().map(|t| t.added_at).collect();
        for p in &parts[1..] {
            let SyncMessage::SnapshotPart { torrents } = p else {
                panic!("only parts after the snapshot");
            };
            sizes.push(torrents.len());
            added.extend(torrents.iter().map(|t| t.added_at));
        }
        assert_eq!(sizes, [FIRST_PART, PART, PART, 400]);
        assert!(added.windows(2).all(|w| w[0] > w[1]), "newest-added first");
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

//! The in-process server the desktop app uses in Local mode.

mod common;

use axum::http::StatusCode;
use futures_util::StreamExt as _;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

use apitorrent_auth::PasswordMode;
use apitorrent_server::{AuthMode, Embedded};
use apitorrent_types::auth::AuthStatus;
use apitorrent_types::{SyncMessage, TorrentSummary};

use common::{AuthFixture, Req, app, core, send, torrent_json};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_process_callers_are_the_owner() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let auth = AuthMode::Enabled(f.auth.clone());
    let embedded = Embedded::new(core.clone(), auth.clone());

    // No credential needed in-process, for reads and writes.
    let resp = embedded
        .request(
            "POST",
            "/api/v1/torrents",
            Some(torrent_json().into_bytes()),
            Some("application/json"),
        )
        .await
        .unwrap();
    assert_eq!(resp.status, 201, "{}", String::from_utf8_lossy(&resp.body));
    let resp = embedded
        .request("GET", "/api/v1/torrents", None, None)
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.content_type.as_deref(),
        Some("application/json"),
        "content type is passed on"
    );
    let list: Vec<TorrentSummary> = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(list.len(), 1);

    let status: AuthStatus = serde_json::from_slice(
        &embedded
            .request("GET", "/api/v1/auth/status", None, None)
            .await
            .unwrap()
            .body,
    )
    .unwrap();
    assert_eq!(status.user.unwrap().display_name, "This computer");
    assert!(!status.auth_disabled);

    // The same core over the network still needs a credential.
    let network = app(&core, auth);
    let resp = send(&network, Req::get("/api/v1/torrents")).await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);

    // The sync socket runs over an in-memory connection.
    let io = embedded.connect().unwrap();
    let request = "ws://localhost/api/v1/sync".into_client_request().unwrap();
    let (mut ws, _) = tokio_tungstenite::client_async(request, io).await.unwrap();
    let first = ws.next().await.unwrap().unwrap().into_text().unwrap();
    match serde_json::from_str::<SyncMessage>(first.as_str()).unwrap() {
        SyncMessage::Snapshot { torrents, .. } => assert_eq!(torrents.len(), 1),
        other @ SyncMessage::Patch { .. } => panic!("expected a snapshot, got {other:?}"),
    }

    // Requests that aren't paths are refused before reaching the router.
    assert!(
        embedded
            .request("GET", "http://x/api", None, None)
            .await
            .is_err()
    );
    drop(ws);
    core.shutdown().await.unwrap();
}

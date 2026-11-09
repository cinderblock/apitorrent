//! `GET /api/v1/roots` and the marker files behind it.

mod common;

use axum::http::StatusCode;

use apitorrent_auth::PasswordMode;
use apitorrent_server::AuthMode;
use apitorrent_types::{RootMarker, StorageRoot};

use common::{AuthFixture, Req, app, core, send};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn roots_are_listed_and_marked() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let app = app(&core, AuthMode::Enabled(f.auth.clone()));

    // Like every other route, it needs a credential.
    let resp = send(&app, Req::get("/api/v1/roots")).await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);

    let resp = send(&app, Req::get("/api/v1/roots").bearer(&f.admin_token())).await;
    assert_eq!(resp.status, StatusCode::OK);
    let roots: Vec<StorageRoot> = resp.typed();
    assert_eq!(roots.len(), 1);
    let downloads = std::path::absolute(root.path().join("downloads")).unwrap();
    assert_eq!(roots[0].path, downloads.display().to_string());
    let marker = std::fs::read(downloads.join(RootMarker::FILE_NAME)).unwrap();
    assert_eq!(RootMarker::parse(&marker).unwrap().id, roots[0].id);

    // The id survives a restart.
    core.shutdown().await.unwrap();
    let again = common::core(root.path()).await;
    assert_eq!(again.storage_roots(), roots);
    again.shutdown().await.unwrap();
}

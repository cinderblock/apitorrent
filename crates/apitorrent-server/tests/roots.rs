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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admins_set_share_locators() {
    use apitorrent_auth::{ClientMeta, Role};
    use apitorrent_types::spaces::{DEFAULT_SPACE, SpaceRole};
    use serde_json::json;

    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let app = app(&core, AuthMode::Enabled(f.auth.clone()));
    let admin = f.admin_token();
    let id = core.storage_roots()[0].id.clone();
    let path = format!("/api/v1/roots/{id}/locators");

    let resp = send(
        &app,
        Req::new("PUT", &path)
            .bearer(&admin)
            .json(&json!({ "locators": ["//nas/torrents", "nfs://nas/srv/torrents", " "] })),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.json());
    let updated: StorageRoot = resp.typed();
    assert_eq!(
        updated.locators,
        [r"\\nas\torrents", "nfs://nas/srv/torrents"],
        "normalised, blanks dropped"
    );
    let listed: Vec<StorageRoot> = send(&app, Req::get("/api/v1/roots").bearer(&admin))
        .await
        .typed();
    assert_eq!(listed, std::slice::from_ref(&updated));

    // Refused: a bad locator (nothing changes), an unknown root.
    let resp = send(
        &app,
        Req::new("PUT", &path)
            .bearer(&admin)
            .json(&json!({ "locators": ["smb://user:pw@nas/t"] })),
    )
    .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    assert_eq!(core.storage_roots()[0].locators, updated.locators);
    let resp = send(
        &app,
        Req::new("PUT", "/api/v1/roots/nope/locators")
            .bearer(&admin)
            .json(&json!({ "locators": [] })),
    )
    .await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);

    // A space's own admin isn't a server admin: roots belong to the instance.
    let meta = ClientMeta::default();
    let s = f
        .auth
        .password_login("admin", common::PASSWORD, &meta, None)
        .unwrap();
    let boss = f.auth.authenticate_session(&s.secret).unwrap().unwrap();
    let link = f
        .auth
        .create_invitation(
            &boss,
            Role::User,
            &[(DEFAULT_SPACE, SpaceRole::Admin)],
            None,
            None,
            &meta,
        )
        .unwrap();
    let sam = f
        .auth
        .enroll_with_password(&link.secret, "sam", None, common::PASSWORD, &meta, None)
        .unwrap();
    let resp = send(
        &app,
        Req::new("PUT", &path)
            .browser()
            .cookie(&format!("apitorrent-session={}", sam.secret))
            .json(&json!({ "locators": [] })),
    )
    .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN);

    // They survive a restart.
    core.shutdown().await.unwrap();
    let again = common::core(root.path()).await;
    assert_eq!(again.storage_roots()[0].locators, updated.locators);
    again.shutdown().await.unwrap();
}

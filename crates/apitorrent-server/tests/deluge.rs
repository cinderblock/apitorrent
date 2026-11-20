//! The Deluge import routes' access rules. What they do is tested in
//! `apitorrent-core/tests/lifecycle.rs` and the `apitorrent-deluge` crate.

mod common;

use axum::http::StatusCode;
use serde_json::json;

use apitorrent_auth::{ClientMeta, CreateTokenRequest, PasswordMode, Principal, Role, TokenScope};
use apitorrent_server::AuthMode;
use apitorrent_types::spaces::{DEFAULT_SPACE, SpaceId, SpaceRole};

use common::{AuthFixture, PASSWORD, Req, app, core, send};

/// The Deluge installations on the server: a space's admins see them, its users don't.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_a_spaces_admins_find_deluge() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let meta = ClientMeta::default();
    let session = f
        .auth
        .setup_with_password(&f.setup_token(), "admin", None, PASSWORD, &meta)
        .unwrap();
    let admin = f
        .auth
        .authenticate_session(&session.secret)
        .unwrap()
        .unwrap();
    let token = |p: &Principal, space: SpaceId| {
        f.auth
            .create_token_in(
                p,
                space,
                &CreateTokenRequest {
                    name: "t".into(),
                    scope: TokenScope::Full,
                    expires_in_days: None,
                },
                &meta,
            )
            .unwrap()
            .token
    };
    let app = app(&core, AuthMode::Enabled(f.auth.clone()));
    let admin_default = token(&admin, DEFAULT_SPACE);
    let r = send(
        &app,
        Req::post("/api/v1/spaces")
            .bearer(&admin_default)
            .json(&json!({ "slug": "tv", "name": "TV" })),
    )
    .await;
    let tv = r.json()["id"].as_i64().unwrap();

    let r = send(
        &app,
        Req::get("/api/v1/import/deluge/found").bearer(&admin_default),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.json().is_array(), "{}", r.json());

    let link = f
        .auth
        .create_invitation(
            &admin,
            Role::User,
            &[(tv, SpaceRole::User)],
            None,
            None,
            &meta,
        )
        .unwrap();
    let s = f
        .auth
        .enroll_with_password(&link.secret, "bob", None, PASSWORD, &meta, None)
        .unwrap();
    let bob = f.auth.authenticate_session(&s.secret).unwrap().unwrap();
    let r = send(
        &app,
        Req::get("/s/tv/api/v1/import/deluge/found").bearer(&token(&bob, tv)),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

/// Imports and take-overs run as background jobs: the request answers at once with the
/// job, a space's admins follow it, its users and other spaces can't see it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn imports_run_as_jobs_a_spaces_admins_follow() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let meta = ClientMeta::default();
    let session = f
        .auth
        .setup_with_password(&f.setup_token(), "admin", None, PASSWORD, &meta)
        .unwrap();
    let admin = f
        .auth
        .authenticate_session(&session.secret)
        .unwrap()
        .unwrap();
    let token = |p: &Principal, space: SpaceId| {
        f.auth
            .create_token_in(
                p,
                space,
                &CreateTokenRequest {
                    name: "t".into(),
                    scope: TokenScope::Full,
                    expires_in_days: None,
                },
                &meta,
            )
            .unwrap()
            .token
    };
    let app = app(&core, AuthMode::Enabled(f.auth.clone()));
    let admin_default = token(&admin, DEFAULT_SPACE);
    let r = send(
        &app,
        Req::post("/api/v1/spaces")
            .bearer(&admin_default)
            .json(&json!({ "slug": "tv", "name": "TV" })),
    )
    .await;
    let tv = r.json()["id"].as_i64().unwrap();

    // No Deluge there: the job starts, then ends failed, saying why.
    let missing = root.path().join("no-deluge-here");
    let r = send(
        &app,
        Req::post("/api/v1/import/deluge/jobs")
            .bearer(&admin_default)
            .json(&json!({ "configDir": missing.to_string_lossy() })),
    )
    .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.json());
    assert_eq!(r.json()["kind"], "delugeImport");
    let id = r.json()["id"].as_u64().unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    let job = loop {
        let r = send(
            &app,
            Req::get(format!("/api/v1/jobs/{id}")).bearer(&admin_default),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);
        if !r.json()["outcome"].is_null() {
            break r.json();
        }
        assert!(tokio::time::Instant::now() < deadline, "never ended");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(job["outcome"]["kind"], "failed", "{job}");
    let r = send(&app, Req::get("/api/v1/jobs").bearer(&admin_default)).await;
    assert_eq!(r.json().as_array().unwrap().len(), 1);

    // Another space doesn't see it.
    let admin_tv = token(&admin, tv);
    let r = send(
        &app,
        Req::get(format!("/s/tv/api/v1/jobs/{id}")).bearer(&admin_tv),
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = send(
        &app,
        Req::post("/s/tv/api/v1/import/deluge/take-over/jobs")
            .bearer(&admin_tv)
            .json(&json!({})),
    )
    .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.json());
    assert_eq!(r.json()["kind"], "takeOver");

    // A user of the space can't start or follow one.
    let link = f
        .auth
        .create_invitation(
            &admin,
            Role::User,
            &[(tv, SpaceRole::User)],
            None,
            None,
            &meta,
        )
        .unwrap();
    let s = f
        .auth
        .enroll_with_password(&link.secret, "bob", None, PASSWORD, &meta, None)
        .unwrap();
    let bob = f.auth.authenticate_session(&s.secret).unwrap().unwrap();
    let bob_tv = token(&bob, tv);
    for req in [
        Req::get("/s/tv/api/v1/jobs").bearer(&bob_tv),
        Req::get("/s/tv/api/v1/jobs/1").bearer(&bob_tv),
        Req::post("/s/tv/api/v1/import/deluge/jobs")
            .bearer(&bob_tv)
            .json(&json!({ "configDir": "x" })),
        Req::post("/s/tv/api/v1/import/deluge/take-over/jobs")
            .bearer(&bob_tv)
            .json(&json!({})),
    ] {
        assert_eq!(send(&app, req).await.status, StatusCode::FORBIDDEN);
    }
}

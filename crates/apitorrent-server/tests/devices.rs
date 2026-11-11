//! Device pairing over HTTP: an app asks without signing in, a browser session approves it
//! by its code, the app collects its credential and uses it as a bearer.

mod common;

use axum::http::StatusCode;
use serde_json::json;

use apitorrent_auth::{ClientMeta, PasswordMode};
use apitorrent_server::AuthMode;
use apitorrent_types::auth::{
    DeviceInfo, DevicePollResponse, DeviceRequestInfo, DeviceRequestStarted,
};

use common::{AuthFixture, PASSWORD, Req, app, core, send};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_device_is_paired_from_a_browser_session() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let app = app(&core, AuthMode::Enabled(f.auth.clone()));
    let _ = f.admin_token();
    let s = f
        .auth
        .password_login("admin", PASSWORD, &ClientMeta::default(), None)
        .unwrap();
    let cookie = format!("apitorrent-session={}", s.secret);
    let browser =
        |path: &str, body: serde_json::Value| Req::post(path).browser().cookie(&cookie).json(&body);

    // The app asks, without signing in. Its pin can't be checked or bound here: this server
    // doesn't serve TLS itself.
    let resp = send(
        &app,
        Req::post("/api/v1/auth/devices/requests").json(&json!({
            "name": "Laptop",
            "platform": "apitorrent on tests",
            "serverKey": "ab".repeat(32),
        })),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.json());
    let started: DeviceRequestStarted = resp.typed();
    assert!(!started.bound);
    let poll = || {
        Req::post("/api/v1/auth/devices/poll").json(&json!({ "pollSecret": started.poll_secret }))
    };
    let pending: DevicePollResponse = send(&app, poll()).await.typed();
    assert_eq!(pending, DevicePollResponse::Pending);

    // Approving needs a signed-in session.
    let resp = send(
        &app,
        Req::post("/api/v1/auth/devices/lookup").json(&json!({ "userCode": started.user_code })),
    )
    .await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
    let info: DeviceRequestInfo = send(
        &app,
        browser(
            "/api/v1/auth/devices/lookup",
            json!({ "userCode": started.user_code }),
        ),
    )
    .await
    .typed();
    assert_eq!(info.name, "Laptop");
    let resp = send(
        &app,
        browser(
            "/api/v1/auth/devices/approve",
            json!({ "userCode": started.user_code, "scope": "full" }),
        ),
    )
    .await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.json());

    let DevicePollResponse::Approved { credential, device } = send(&app, poll()).await.typed()
    else {
        panic!("not approved");
    };
    assert!(credential.starts_with("apd_"));

    // The credential is a bearer for the API, but not for account management.
    let resp = send(&app, Req::get("/api/v1/torrents").bearer(&credential)).await;
    assert_eq!(resp.status, StatusCode::OK);
    let resp = send(&app, Req::get("/api/v1/auth/tokens").bearer(&credential)).await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN);
    let mine: Vec<DeviceInfo> = send(&app, Req::get("/api/v1/auth/devices").bearer(&credential))
        .await
        .typed();
    assert_eq!(mine.len(), 1);
    assert!(mine[0].current);

    // The owner sees it, and the device can sign itself out.
    let listed: Vec<DeviceInfo> = send(
        &app,
        Req::get("/api/v1/auth/devices").browser().cookie(&cookie),
    )
    .await
    .typed();
    assert_eq!(listed[0].id, device.id);
    let resp = send(
        &app,
        Req::new("DELETE", format!("/api/v1/auth/devices/{}", device.id)).bearer(&credential),
    )
    .await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT);
    let resp = send(&app, Req::get("/api/v1/torrents").bearer(&credential)).await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
    core.shutdown().await.unwrap();
}

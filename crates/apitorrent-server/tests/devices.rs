//! Device pairing over HTTP: an app asks without signing in, a browser session approves it
//! by its code, the app collects its credential and uses it as a bearer.

mod common;

use axum::http::StatusCode;
use serde_json::json;

use apitorrent_auth::{ClientMeta, Clock as _, PasswordMode};
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

/// A headless server set up from the app, without a browser: the setup token from the
/// console registers the app and makes the first admin; the device works in every space;
/// a second device registers with a token from `apitorrentd register-device`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_app_sets_up_a_headless_server_with_console_tokens() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Off, &[]);
    let app = app(&core, AuthMode::Enabled(f.auth.clone()));
    let register = |token: String, username: Option<&str>| {
        Req::post("/api/v1/auth/devices/requests").json(&json!({
            "name": "Laptop",
            "platform": "apitorrent on tests",
            "registration": { "token": token, "username": username },
        }))
    };
    let collect = |started: &DeviceRequestStarted| {
        Req::post("/api/v1/auth/devices/poll").json(&json!({ "pollSecret": started.poll_secret }))
    };

    let resp = send(&app, register("not-the-token".into(), Some("cam"))).await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
    let resp = send(&app, register(f.setup_token(), Some("cam"))).await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.json());
    let started: DeviceRequestStarted = resp.typed();
    assert!(started.registered);
    let DevicePollResponse::Approved { credential, device } =
        send(&app, collect(&started)).await.typed()
    else {
        panic!("registered requests are approved at once");
    };
    assert_eq!(device.space, None);

    // The first admin, signed in as this device, in any space.
    let resp = send(&app, Req::get("/api/v1/auth/devices").bearer(&credential)).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.json());
    let resp = send(
        &app,
        Req::post("/api/v1/spaces")
            .bearer(&credential)
            .json(&json!({ "slug": "tv", "name": "TV" })),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.json());
    let resp = send(&app, Req::get("/s/tv/api/v1/torrents").bearer(&credential)).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.json());

    // Another device, with a token from the server's command line.
    let issued = apitorrent_auth::create_registration_token(
        &root.path().join("data").join("auth.db"),
        None,
        f.clock.now_ms(),
    )
    .unwrap();
    assert_eq!(issued.username, "cam");
    let resp = send(&app, register(issued.token.clone(), None)).await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.json());
    let resp = send(&app, register(issued.token, None)).await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED, "one use");
}

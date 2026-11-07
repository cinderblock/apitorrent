//! The API over real HTTP semantics: guards, handlers, error mapping and sync.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use futures_util::StreamExt;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use apitorrent_auth::PasswordMode;
use apitorrent_server::{AuthMode, ServeError, ServerConfig, TrustedProxies, router};
use apitorrent_types::{SyncMessage, TorrentDetail, TorrentSummary};

use common::{AuthFixture, Req, app, core, send, server_config, torrent_json};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crud_and_errors() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let token = f.admin_token();
    let app = app(&core, AuthMode::Enabled(f.auth.clone()));

    let resp = send(
        &app,
        Req::post("/api/v1/torrents")
            .bearer(&token)
            .raw_json(torrent_json()),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED);
    let id = resp.json()["id"].as_str().unwrap().to_owned();

    let resp = send(
        &app,
        Req::post("/api/v1/torrents")
            .host("localhost")
            .bearer(&token)
            .raw_json(torrent_json()),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CONFLICT);
    assert_eq!(resp.code(), "duplicate");

    let resp = send(
        &app,
        Req::get("/api/v1/torrents")
            .host("127.0.0.1:8742")
            .bearer(&token),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    let list: Vec<TorrentSummary> = resp.typed();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "hello.txt");

    let resp = send(
        &app,
        Req::get(format!("/api/v1/torrents/{id}")).bearer(&token),
    )
    .await;
    let detail: TorrentDetail = resp.typed();
    assert_eq!(detail.files.len(), 1);

    let resp = send(
        &app,
        Req::new("PUT", format!("/api/v1/torrents/{id}/files/priorities"))
            .bearer(&token)
            .raw_json(r#"{"priorities":[{"index":5,"priority":1}]}"#.into()),
    )
    .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);

    let resp = send(
        &app,
        Req::post("/api/v1/torrents/nope/pause").bearer(&token),
    )
    .await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);

    let resp = send(
        &app,
        Req::new("DELETE", format!("/api/v1/torrents/{id}?deleteFiles=true")).bearer(&token),
    )
    .await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT);
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guards() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let token = f.admin_token();
    let app = app(&core, AuthMode::Enabled(f.auth.clone()));

    // DNS rebinding: a foreign Host header is refused, even for reads.
    let resp = send(
        &app,
        Req::get("/api/v1/torrents")
            .host("evil.example.com")
            .bearer(&token),
    )
    .await;
    assert_eq!(resp.status, StatusCode::MISDIRECTED_REQUEST);
    // A configured public host is accepted, and `info` needs no sign-in.
    let resp = send(&app, Req::get("/api/v1/info").host("torrents.example.com")).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(
        resp.json()["capabilities"]
            .as_array()
            .unwrap()
            .contains(&"auth".into())
    );
    // CSRF: a state change from another site's page is refused...
    for origin in ["https://evil.example.com", "http://localhost:5173"] {
        let resp = send(
            &app,
            Req::post("/api/v1/auth/login/passkey/start").header("origin", origin),
        )
        .await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "{origin}");
        assert_eq!(resp.code(), "cross_origin");
    }
    // ...including from another port on localhost, which browsers call `same-site`.
    let resp = send(
        &app,
        Req::post("/api/v1/auth/login/passkey/start").header("sec-fetch-site", "same-site"),
    )
    .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN);
    // A bearer token can't be sent cross-site by a browser, so it skips those checks.
    let resp = send(
        &app,
        Req::post("/api/v1/torrents")
            .header("origin", "https://evil.example.com")
            .header("sec-fetch-site", "cross-site")
            .bearer(&token)
            .raw_json(torrent_json()),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED);
    // Without a credential, everything else is a 401, unknown paths included.
    for path in ["/api/v1/torrents", "/api/v1/nope", "/api/v1/sync"] {
        let resp = send(&app, Req::get(path)).await;
        assert_eq!(resp.status, StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(resp.headers["www-authenticate"], "Bearer");
    }
    // Unknown endpoints get a JSON 404 once signed in.
    let resp = send(&app, Req::get("/api/v1/nope").bearer(&token)).await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
    // A malformed or unknown bearer token is a 401, never a fall-back to the cookie.
    for auth in ["Basic Zm9vOmJhcg==", "Bearer", "Bearer apt_nope"] {
        let resp = send(
            &app,
            Req::get("/api/v1/torrents").header("authorization", auth),
        )
        .await;
        assert_eq!(resp.status, StatusCode::UNAUTHORIZED, "{auth}");
        assert_eq!(resp.code(), "invalid_token");
    }
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_auth_mode_is_loopback_only() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let mut cfg = server_config(AuthMode::DisabledOnLoopback);
    cfg.trusted_proxies = TrustedProxies::default();
    let app = router(core.clone(), &cfg);

    let resp = send(&app, Req::get("/api/v1/torrents")).await;
    assert_eq!(resp.status, StatusCode::OK);
    let resp = send(&app, Req::get("/api/v1/auth/status")).await;
    assert_eq!(resp.json()["authDisabled"], true);
    assert_eq!(resp.json()["user"]["name"], "local");
    // Not from this machine.
    let resp = send(&app, Req::get("/api/v1/torrents").peer("192.0.2.4")).await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
    // Through a local reverse proxy: the peer is loopback, but the client may not be.
    let resp = send(
        &app,
        Req::get("/api/v1/torrents").header("x-forwarded-for", "203.0.113.5"),
    )
    .await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
    // A public host name means a proxy, too.
    let resp = send(
        &app,
        Req::get("/api/v1/torrents").host("torrents.example.com"),
    )
    .await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_rules() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Off, &[]);
    let lan = |auth, proxies: &[&str]| ServerConfig {
        listen: "0.0.0.0:0".parse().unwrap(),
        trusted_proxies: TrustedProxies::parse(
            &proxies.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
        )
        .unwrap(),
        ..server_config(auth)
    };
    // Plain HTTP on the LAN needs a trusted proxy in front, or an explicit opt-in.
    let err = apitorrent_server::serve(
        core.clone(),
        lan(AuthMode::Enabled(f.auth.clone()), &[]),
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ServeError::InsecureLan(_)), "{err}");
    // Never without authentication.
    let err = apitorrent_server::serve(
        core.clone(),
        lan(AuthMode::DisabledOnLoopback, &[]),
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ServeError::NoAuthNotLoopback(_)), "{err}");
    // And no-auth mode refuses proxies even on loopback.
    let cfg = ServerConfig {
        trusted_proxies: TrustedProxies::parse(&["127.0.0.1".into()]).unwrap(),
        ..server_config(AuthMode::DisabledOnLoopback)
    };
    let err = apitorrent_server::serve(core.clone(), cfg, std::future::pending())
        .await
        .unwrap_err();
    assert!(matches!(err, ServeError::NoAuthWithProxies), "{err}");
    core.shutdown().await.unwrap();
}

async fn next_sync<S>(ws: &mut S) -> SyncMessage
where
    S: futures_util::Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .expect("sync message timed out")
            .unwrap()
            .unwrap();
        if let WsMessage::Text(t) = msg {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_websocket() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let token = f.admin_token();
    let app = app(&core, AuthMode::Enabled(f.auth.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = app.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            served.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    // Without a credential the upgrade is refused.
    let err = tokio_tungstenite::connect_async(format!("ws://{addr}/api/v1/sync"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("401"), "{err}");

    let mut request = format!("ws://{addr}/api/v1/sync")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let SyncMessage::Snapshot { revision, torrents } = next_sync(&mut ws).await else {
        panic!("expected a snapshot first");
    };
    assert_eq!(torrents, [] as [TorrentSummary; 0]);

    // Adding a torrent produces a patch with the next revision.
    let resp = send(
        &app,
        Req::post("/api/v1/torrents")
            .bearer(&token)
            .raw_json(torrent_json()),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED);
    let SyncMessage::Patch {
        revision: next,
        upserted,
        ..
    } = next_sync(&mut ws).await
    else {
        panic!("expected a patch");
    };
    assert_eq!(next, revision + 1);
    assert_eq!(upserted[0].name, "hello.txt");
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serves_the_ui_with_security_headers() {
    let root = tempfile::tempdir().unwrap();
    let web = root.path().join("web");
    std::fs::create_dir_all(web.join("assets")).unwrap();
    std::fs::write(web.join("index.html"), "<!doctype html><title>ui</title>").unwrap();
    std::fs::write(web.join("assets").join("app.js"), "console.log(1)").unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Off, &[]);
    let cfg = ServerConfig {
        web_root: Some(web),
        ..server_config(AuthMode::Enabled(f.auth.clone()))
    };
    let app = router(core.clone(), &cfg);

    // The UI itself needs no sign-in: it shows the login page.
    let resp = send(&app, Req::get("/")).await;
    assert_eq!(resp.status, StatusCode::OK);
    let csp = resp.headers["content-security-policy"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    assert_eq!(resp.headers["x-content-type-options"], "nosniff");

    let resp = send(&app, Req::get("/assets/app.js")).await;
    assert_eq!(resp.status, StatusCode::OK);
    // A client-side route falls back to index.html.
    let resp = send(&app, Req::get("/setup")).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&resp.body).contains("<title>ui</title>"));
    // API paths stay JSON.
    let resp = send(&app, Req::get("/api/v1/nope")).await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
    assert_eq!(resp.code(), "unauthenticated");
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bans() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let app = router(core.clone(), &config());
    let host = "127.0.0.1:8742";
    let send = |r| app.clone().oneshot(r);

    let resp = send(req("GET", "/api/v1/bans", host, None, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(json::<Vec<apitorrent_types::Ban>>(resp).await.is_empty());

    let body = serde_json::json!({ "ip": "203.0.113.9" }).to_string();
    let resp = send(req("POST", "/api/v1/bans", host, None, Some(body)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let bans: Vec<apitorrent_types::Ban> = json(
        send(req("GET", "/api/v1/bans", host, None, None))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(bans.len(), 1);
    assert_eq!(bans[0].ip, "203.0.113.9");
    assert_eq!(bans[0].reason, apitorrent_types::BanReason::Manual);

    let bad = serde_json::json!({ "ip": "not-an-ip" }).to_string();
    let resp = send(req("POST", "/api/v1/bans", host, None, Some(bad)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let resp = send(req("DELETE", "/api/v1/bans/203.0.113.9", host, None, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = send(req("DELETE", "/api/v1/bans/203.0.113.9", host, None, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    core.shutdown().await.unwrap();
}

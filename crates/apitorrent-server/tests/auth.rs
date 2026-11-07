//! Authentication end to end over HTTP, with a software authenticator: bootstrap, passkey
//! sign-in, sessions and their expiry, re-authentication, passkey and token management,
//! scopes, the second factor, lockouts, proxies and the sync WebSocket.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use apitorrent_auth::PasswordMode;
use apitorrent_server::AuthMode;

use common::authenticator::SoftAuthenticator;
use common::{AuthFixture, PASSWORD, Req, Resp, app, core, send, torrent_json_named};

const ORIGIN: &str = "http://localhost:8742";
const MINUTE: Duration = Duration::from_secs(60);

async fn post(app: &axum::Router, path: &str, body: Value, cookie: Option<&str>) -> Resp {
    let mut r = Req::post(path).browser().json(&body);
    if let Some(c) = cookie {
        r = r.cookie(c);
    }
    send(app, r).await
}

async fn get(app: &axum::Router, path: &str, cookie: &str) -> Resp {
    send(app, Req::get(path).browser().cookie(cookie)).await
}

/// Enroll the first admin with a passkey; returns the session cookie.
async fn bootstrap(app: &axum::Router, f: &AuthFixture, soft: &mut SoftAuthenticator) -> String {
    let start = post(
        app,
        "/api/v1/auth/setup/start",
        json!({ "token": f.setup_token(), "username": "Admin", "displayName": "The Admin" }),
        None,
    )
    .await;
    assert_eq!(start.status, StatusCode::OK, "{}", start.json());
    let start = start.json();
    let cred = soft.register(&start["options"], ORIGIN).unwrap();
    let done = post(
        app,
        "/api/v1/auth/setup/finish",
        json!({ "ceremonyId": start["ceremonyId"], "credential": cred, "passkeyName": "Laptop" }),
        None,
    )
    .await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.json());
    done.cookie()
}

/// Sign in with the authenticator's first passkey for localhost.
async fn login(app: &axum::Router, soft: &mut SoftAuthenticator, cookie: Option<&str>) -> Resp {
    let start = post(app, "/api/v1/auth/login/passkey/start", json!({}), cookie).await;
    assert_eq!(start.status, StatusCode::OK, "{}", start.json());
    let start = start.json();
    let cred = soft.authenticate(&start["options"], ORIGIN).unwrap();
    post(
        app,
        "/api/v1/auth/login/passkey/finish",
        json!({ "ceremonyId": start["ceremonyId"], "credential": cred }),
        cookie,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn passkey_flow_end_to_end() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Off, &[]);
    let app = app(&core, AuthMode::Enabled(f.auth.clone()));
    let mut soft = SoftAuthenticator::default();

    // ----- Before setup -----
    let status = send(&app, Req::get("/api/v1/auth/status")).await.json();
    assert_eq!(status["setupRequired"], true);
    assert_eq!(status["passkeysAvailable"], true);
    assert_eq!(status["passwordMode"], "off");
    assert!(status["user"].is_null());
    assert_eq!(
        send(&app, Req::get("/api/v1/torrents")).await.status,
        StatusCode::UNAUTHORIZED
    );
    let wrong = post(
        &app,
        "/api/v1/auth/setup/start",
        json!({ "token": "aaaa-bbbb", "username": "admin" }),
        None,
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert_eq!(wrong.code(), "invalid_setup_token");

    // ----- Setup -----
    let start = post(
        &app,
        "/api/v1/auth/setup/start",
        json!({ "token": f.setup_token(), "username": "Admin" }),
        None,
    )
    .await
    .json();
    let options = &start["options"];
    assert_eq!(options["rp"]["id"], "localhost");
    assert_eq!(options["authenticatorSelection"]["residentKey"], "required");
    assert_eq!(
        options["authenticatorSelection"]["userVerification"],
        "required"
    );
    // Without user verification (a PIN or biometric), the server refuses the credential.
    let mut no_uv = SoftAuthenticator {
        skip_user_verification: true,
        ..SoftAuthenticator::default()
    };
    let cred = no_uv.register(options, ORIGIN).unwrap();
    let r = post(
        &app,
        "/api/v1/auth/setup/finish",
        json!({ "ceremonyId": start["ceremonyId"], "credential": cred }),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // Ceremonies are single-use.
    let cred = soft.register(options, ORIGIN).unwrap();
    let r = post(
        &app,
        "/api/v1/auth/setup/finish",
        json!({ "ceremonyId": start["ceremonyId"], "credential": cred }),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    soft.credentials.clear();

    let start = post(
        &app,
        "/api/v1/auth/setup/start",
        json!({ "token": f.setup_token(), "username": "Admin" }),
        None,
    )
    .await
    .json();
    let cred = soft.register(&start["options"], ORIGIN).unwrap();
    let done = post(
        &app,
        "/api/v1/auth/setup/finish",
        json!({ "ceremonyId": start["ceremonyId"], "credential": cred, "passkeyName": "Laptop" }),
        None,
    )
    .await;
    assert_eq!(done.status, StatusCode::OK);
    let set_cookie = done.set_cookie().unwrap();
    assert!(
        set_cookie.starts_with("apitorrent-session="),
        "{set_cookie}"
    );
    for attr in ["HttpOnly", "SameSite=Strict", "Path=/", "Max-Age="] {
        assert!(set_cookie.contains(attr), "{set_cookie}");
    }
    assert!(
        !set_cookie.contains("Secure"),
        "plain HTTP on loopback: {set_cookie}"
    );
    assert_eq!(done.json()["user"]["name"], "admin");
    assert_eq!(done.json()["user"]["role"], "admin");
    let c1 = done.cookie();

    let status = get(&app, "/api/v1/auth/status", &c1).await.json();
    assert_eq!(status["setupRequired"], false);
    assert_eq!(status["session"]["method"], "passkey");
    let again = post(
        &app,
        "/api/v1/auth/setup/start",
        json!({ "token": f.setup_token(), "username": "eve" }),
        None,
    )
    .await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(again.code(), "already_set_up");

    // The per-address limit allows bursts of 15 sign-in calls; let the bucket refill.
    f.advance(Duration::from_secs(30));

    // ----- Cookie sessions and CSRF -----
    assert_eq!(
        get(&app, "/api/v1/torrents", &c1).await.status,
        StatusCode::OK
    );
    let add = send(
        &app,
        Req::post("/api/v1/torrents")
            .browser()
            .cookie(&c1)
            .raw_json(torrent_json_named("a.txt")),
    )
    .await;
    assert_eq!(add.status, StatusCode::CREATED);
    let forged = send(
        &app,
        Req::post("/api/v1/torrents")
            .header("origin", "http://localhost:5173")
            .cookie(&c1)
            .raw_json(torrent_json_named("b.txt")),
    )
    .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);

    // ----- Logout -----
    let out = post(&app, "/api/v1/auth/logout", json!({}), Some(&c1)).await;
    assert_eq!(out.status, StatusCode::NO_CONTENT);
    assert!(out.set_cookie().unwrap().contains("Max-Age=0"));
    let stale = get(&app, "/api/v1/torrents", &c1).await;
    assert_eq!(stale.status, StatusCode::UNAUTHORIZED);
    assert!(
        stale.set_cookie().unwrap().contains("Max-Age=0"),
        "a stale cookie is cleared"
    );

    // ----- Passkey sign-in (usernameless) -----
    let start = post(&app, "/api/v1/auth/login/passkey/start", json!({}), None)
        .await
        .json();
    let options = &start["options"];
    assert_eq!(options["rpId"], "localhost");
    assert!(
        options["allowCredentials"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "{options}"
    );
    let assertion = soft.authenticate(options, ORIGIN).unwrap();
    let ok = post(
        &app,
        "/api/v1/auth/login/passkey/finish",
        json!({ "ceremonyId": start["ceremonyId"], "credential": assertion.clone() }),
        None,
    )
    .await;
    assert_eq!(ok.status, StatusCode::OK);
    let c2 = ok.cookie();
    assert_ne!(c1, c2);
    // Replaying the assertion against a new challenge fails.
    let start = post(&app, "/api/v1/auth/login/passkey/start", json!({}), None)
        .await
        .json();
    let replay = post(
        &app,
        "/api/v1/auth/login/passkey/finish",
        json!({ "ceremonyId": start["ceremonyId"], "credential": assertion }),
        None,
    )
    .await;
    assert_eq!(replay.status, StatusCode::UNAUTHORIZED);
    // An assertion made for another origin fails.
    let start = post(&app, "/api/v1/auth/login/passkey/start", json!({}), None)
        .await
        .json();
    let phished = soft
        .authenticate(&start["options"], "http://localhost.evil.example:8742")
        .unwrap();
    let r = post(
        &app,
        "/api/v1/auth/login/passkey/finish",
        json!({ "ceremonyId": start["ceremonyId"], "credential": phished }),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // A forged signature fails.
    let start = post(&app, "/api/v1/auth/login/passkey/start", json!({}), None)
        .await
        .json();
    let mut forged = soft.authenticate(&start["options"], ORIGIN).unwrap();
    // Flip a bit in the last byte of the DER signature (inside S): still well-formed, but
    // no longer a valid signature. (Editing the base64 text could hit padding bits.)
    let b64 = data_encoding::BASE64URL_NOPAD;
    let mut sig = b64
        .decode(forged["response"]["signature"].as_str().unwrap().as_bytes())
        .unwrap();
    *sig.last_mut().unwrap() ^= 0x01;
    forged["response"]["signature"] = Value::from(b64.encode(&sig));
    let r = post(
        &app,
        "/api/v1/auth/login/passkey/finish",
        json!({ "ceremonyId": start["ceremonyId"], "credential": forged }),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    f.advance(Duration::from_secs(30));

    // ----- Adding, renaming and removing passkeys -----
    let start = post(&app, "/api/v1/auth/passkeys/start", json!({}), Some(&c2))
        .await
        .json();
    assert_eq!(
        start["options"]["excludeCredentials"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // The same authenticator can't register twice.
    assert!(
        soft.register(&start["options"], ORIGIN)
            .unwrap_err()
            .contains("InvalidStateError")
    );
    let mut phone = SoftAuthenticator::default();
    let cred = phone.register(&start["options"], ORIGIN).unwrap();
    let added = post(
        &app,
        "/api/v1/auth/passkeys/finish",
        json!({ "ceremonyId": start["ceremonyId"], "credential": cred, "name": "Phone" }),
        Some(&c2),
    )
    .await;
    assert_eq!(added.status, StatusCode::CREATED, "{}", added.json());
    assert_eq!(added.json()["name"], "Phone");
    let phone_id = added.json()["id"].as_str().unwrap().to_owned();
    // The phone signs in too.
    assert_eq!(login(&app, &mut phone, None).await.status, StatusCode::OK);

    let list = get(&app, "/api/v1/auth/passkeys", &c2).await.json();
    assert_eq!(list.as_array().unwrap().len(), 2);
    let laptop_id = list[0]["id"].as_str().unwrap().to_owned();
    assert_eq!(list[0]["name"], "Laptop");
    let r = send(
        &app,
        Req::new("PATCH", format!("/api/v1/auth/passkeys/{laptop_id}"))
            .browser()
            .cookie(&c2)
            .json(&json!({ "name": "Work laptop" })),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = send(
        &app,
        Req::new("DELETE", format!("/api/v1/auth/passkeys/{phone_id}"))
            .browser()
            .cookie(&c2),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    // A revoked passkey no longer signs in.
    assert_eq!(
        login(&app, &mut phone, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    // The last one can't be removed.
    let r = send(
        &app,
        Req::new("DELETE", format!("/api/v1/auth/passkeys/{laptop_id}"))
            .browser()
            .cookie(&c2),
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let list = get(&app, "/api/v1/auth/passkeys", &c2).await.json();
    assert_eq!(list[0]["name"], "Work laptop");

    // ----- Re-authentication for sensitive changes -----
    f.advance(11 * MINUTE);
    let r = post(&app, "/api/v1/auth/passkeys/start", json!({}), Some(&c2)).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.code(), "reauth_required");
    let start = post(
        &app,
        "/api/v1/auth/reauth/passkey/start",
        json!({}),
        Some(&c2),
    )
    .await
    .json();
    assert_eq!(
        start["options"]["allowCredentials"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let cred = soft.authenticate(&start["options"], ORIGIN).unwrap();
    let r = post(
        &app,
        "/api/v1/auth/reauth/passkey/finish",
        json!({ "ceremonyId": start["ceremonyId"], "credential": cred }),
        Some(&c2),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let c3 = r.cookie();
    assert_eq!(
        get(&app, "/api/v1/torrents", &c2).await.status,
        StatusCode::UNAUTHORIZED,
        "re-authentication rotates the session"
    );

    // ----- API tokens and scopes -----
    let read = post(
        &app,
        "/api/v1/auth/tokens",
        json!({ "name": "monitor", "scope": "read" }),
        Some(&c3),
    )
    .await;
    assert_eq!(read.status, StatusCode::CREATED);
    assert_eq!(read.headers["cache-control"], "no-store");
    let read_token = read.json()["token"].as_str().unwrap().to_owned();
    let read_id = read.json()["info"]["id"].as_str().unwrap().to_owned();
    let full = post(
        &app,
        "/api/v1/auth/tokens",
        json!({ "name": "script", "scope": "full", "expiresInDays": 30 }),
        Some(&c3),
    )
    .await
    .json();
    let full_token = full["token"].as_str().unwrap().to_owned();

    let r = send(&app, Req::get("/api/v1/torrents").bearer(&read_token)).await;
    assert_eq!(r.status, StatusCode::OK);
    let id = r.json()[0]["id"].as_str().unwrap().to_owned();
    let r = send(
        &app,
        Req::post(format!("/api/v1/torrents/{id}/resume")).bearer(&read_token),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.code(), "read_only");
    let r = send(
        &app,
        Req::post(format!("/api/v1/torrents/{id}/pause")).bearer(&full_token),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    // Tokens can't manage credentials, not even full ones.
    let r = send(
        &app,
        Req::post("/api/v1/auth/tokens")
            .bearer(&full_token)
            .json(&json!({ "name": "escalate", "scope": "full" })),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let tokens = get(&app, "/api/v1/auth/tokens", &c3).await.json();
    assert_eq!(tokens.as_array().unwrap().len(), 2);
    assert!(read_token.starts_with(tokens[0]["prefix"].as_str().unwrap()));
    let r = send(
        &app,
        Req::new("DELETE", format!("/api/v1/auth/tokens/{read_id}"))
            .browser()
            .cookie(&c3),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = send(&app, Req::get("/api/v1/torrents").bearer(&read_token)).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    f.advance(Duration::from_secs(30));

    // ----- Sessions on other devices -----
    let other = login(&app, &mut soft, None).await.cookie();
    let sessions = get(&app, "/api/v1/auth/sessions", &c3).await.json();
    let sessions = sessions.as_array().unwrap();
    assert!(sessions.len() >= 2);
    let mine: Vec<_> = sessions.iter().filter(|s| s["current"] == true).collect();
    assert_eq!(mine.len(), 1);
    for s in sessions.iter().filter(|s| s["current"] == false) {
        let r = send(
            &app,
            Req::new(
                "DELETE",
                format!("/api/v1/auth/sessions/{}", s["id"].as_str().unwrap()),
            )
            .browser()
            .cookie(&c3),
        )
        .await;
        assert_eq!(r.status, StatusCode::NO_CONTENT);
    }
    assert_eq!(
        get(&app, "/api/v1/torrents", &other).await.status,
        StatusCode::UNAUTHORIZED
    );

    // ----- Expiry -----
    f.advance(Duration::from_secs(7 * 24 * 3600 + 60));
    assert_eq!(
        get(&app, "/api/v1/torrents", &c3).await.status,
        StatusCode::UNAUTHORIZED,
        "idle for over a week"
    );
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_socket_closes_when_signed_out() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Off, &[]);
    let app = app(&core, AuthMode::Enabled(f.auth.clone()));
    let mut soft = SoftAuthenticator::default();
    let cookie = bootstrap(&app, &f, &mut soft).await;

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
    let url = format!("ws://localhost:{}/api/v1/sync", addr.port());
    let request = |origin: Option<&str>| {
        let mut r = url.as_str().into_client_request().unwrap();
        r.headers_mut().insert("cookie", cookie.parse().unwrap());
        if let Some(o) = origin {
            r.headers_mut().insert("origin", o.parse().unwrap());
        }
        r
    };
    let connect = |r| async move {
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        tokio_tungstenite::client_async(r, stream).await
    };

    // A cookie without an Origin header isn't a page of ours.
    let err = connect(request(None)).await.unwrap_err();
    assert!(err.to_string().contains("403"), "{err}");
    // Another site's page can't open it with the cookie either.
    let err = connect(request(Some("http://localhost:5173")))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("403"), "{err}");

    let page = format!("http://localhost:{}", addr.port());
    let (mut ws, _) = connect(request(Some(&page))).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(first, WsMessage::Text(_)));

    let out = post(&app, "/api/v1/auth/logout", json!({}), Some(&cookie)).await;
    assert_eq!(out.status, StatusCode::NO_CONTENT);
    let closed = loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .expect("the socket closes promptly")
            .unwrap()
            .unwrap();
        if let WsMessage::Close(frame) = msg {
            break frame;
        }
    };
    assert_eq!(u16::from(closed.unwrap().code), 4401);
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn password_as_second_factor() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::SecondFactor, &[]);
    let app = app(&core, AuthMode::Enabled(f.auth.clone()));
    let mut soft = SoftAuthenticator::default();
    let c1 = bootstrap(&app, &f, &mut soft).await;

    let short = send(
        &app,
        Req::new("PUT", "/api/v1/auth/password")
            .browser()
            .cookie(&c1)
            .json(&json!({ "password": "short" })),
    )
    .await;
    assert_eq!(short.status, StatusCode::BAD_REQUEST);
    let set = send(
        &app,
        Req::new("PUT", "/api/v1/auth/password")
            .browser()
            .cookie(&c1)
            .json(&json!({ "password": PASSWORD })),
    )
    .await;
    assert_eq!(set.status, StatusCode::NO_CONTENT);
    post(&app, "/api/v1/auth/logout", json!({}), Some(&c1)).await;

    // A password alone never signs in here.
    let r = post(
        &app,
        "/api/v1/auth/login/password",
        json!({ "username": "admin", "password": PASSWORD }),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    let step1 = login(&app, &mut soft, None).await;
    assert_eq!(step1.status, StatusCode::OK);
    assert_eq!(step1.json()["secondFactorRequired"], true);
    assert!(step1.json()["user"].is_null());
    let pending = step1.cookie();
    let r = get(&app, "/api/v1/torrents", &pending).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.code(), "second_factor_required");
    let r = post(
        &app,
        "/api/v1/auth/login/password",
        json!({ "password": "not the password" }),
        Some(&pending),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = post(
        &app,
        "/api/v1/auth/login/password",
        json!({ "password": PASSWORD }),
        Some(&pending),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["session"]["method"], "passkeyAndPassword");
    let full = r.cookie();
    assert_eq!(
        get(&app, "/api/v1/torrents", &full).await.status,
        StatusCode::OK
    );
    assert_eq!(
        get(&app, "/api/v1/torrents", &pending).await.status,
        StatusCode::UNAUTHORIZED,
        "the pending session is replaced"
    );

    // A pending session that isn't finished in time expires.
    let step1 = login(&app, &mut soft, None).await.cookie();
    f.advance(6 * MINUTE);
    let r = post(
        &app,
        "/api/v1/auth/login/password",
        json!({ "password": PASSWORD }),
        Some(&step1),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.json());
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn password_fallback_lockout_and_rate_limits() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let app = app(&core, AuthMode::Enabled(f.auth.clone()));

    // Passkeys don't work on an IP address; the status says where they do.
    let status = send(&app, Req::get("/api/v1/auth/status").host("127.0.0.1:8742"))
        .await
        .json();
    assert_eq!(status["passkeysAvailable"], false);
    assert_eq!(status["passkeyHint"], "http://localhost:8742");
    assert_eq!(status["passwordMode"], "fallback");

    let setup = post(
        &app,
        "/api/v1/auth/setup/password",
        json!({ "token": f.setup_token(), "username": "admin", "password": PASSWORD }),
        None,
    )
    .await;
    assert_eq!(setup.status, StatusCode::OK);
    assert_eq!(setup.json()["session"]["method"], "password");

    let attempt = |pw: &'static str, peer: String| {
        let app = app.clone();
        async move {
            send(
                &app,
                Req::post("/api/v1/auth/login/password")
                    .browser()
                    .peer(&peer)
                    .json(&json!({ "username": "admin", "password": pw })),
            )
            .await
        }
    };
    for i in 0..5 {
        let r = attempt("wrong password", format!("198.51.100.{i}")).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    }
    let r = attempt(PASSWORD, "198.51.100.99".into()).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.headers["retry-after"], "60");
    f.advance(Duration::from_secs(61));
    let r = attempt(PASSWORD, "198.51.100.99".into()).await;
    assert_eq!(r.status, StatusCode::OK);

    // One address can't hammer the sign-in endpoints.
    let mut limited = false;
    for _ in 0..40 {
        let r = send(
            &app,
            Req::post("/api/v1/auth/login/passkey/start")
                .browser()
                .peer("192.0.2.200"),
        )
        .await;
        if r.status == StatusCode::TOO_MANY_REQUESTS {
            limited = true;
            break;
        }
    }
    assert!(limited);
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn behind_a_tls_proxy() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(
        root.path(),
        PasswordMode::Off,
        &["https://torrents.example.com"],
    );
    let app = app(&core, AuthMode::Enabled(f.auth.clone()));
    let public = "https://torrents.example.com";
    let via_proxy = |r: Req| {
        r.host("torrents.example.com")
            .peer("10.0.0.1")
            .header("x-forwarded-proto", "https")
            .header("x-forwarded-for", "203.0.113.9")
            .browser()
    };
    let mut soft = SoftAuthenticator::default();

    let start = send(
        &app,
        via_proxy(Req::post("/api/v1/auth/setup/start"))
            .json(&json!({ "token": f.setup_token(), "username": "admin" })),
    )
    .await
    .json();
    assert_eq!(start["options"]["rp"]["id"], "torrents.example.com");
    let cred = soft.register(&start["options"], public).unwrap();
    let done = send(
        &app,
        via_proxy(Req::post("/api/v1/auth/setup/finish"))
            .json(&json!({ "ceremonyId": start["ceremonyId"], "credential": cred })),
    )
    .await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.json());
    let sc = done.set_cookie().unwrap();
    assert!(sc.starts_with("__Host-apitorrent-session="), "{sc}");
    assert!(sc.contains("; Secure"), "{sc}");
    let cookie = done.cookie();
    let value = cookie.split_once('=').unwrap().1.to_owned();

    let me = send(
        &app,
        via_proxy(Req::get("/api/v1/auth/status")).cookie(&cookie),
    )
    .await
    .json();
    assert_eq!(me["user"]["name"], "admin");
    assert_eq!(me["insecureTransport"], false);
    // The audit trail has the real client, not the proxy.
    let sessions = send(
        &app,
        via_proxy(Req::get("/api/v1/auth/sessions")).cookie(&cookie),
    )
    .await
    .json();
    assert_eq!(sessions[0]["ip"], "203.0.113.9");

    // Over HTTPS only the __Host- name counts...
    let plain_name = format!("apitorrent-session={value}");
    let r = send(
        &app,
        via_proxy(Req::get("/api/v1/torrents")).cookie(&plain_name),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // ...and over plain HTTP only the plain one.
    let r = send(&app, Req::get("/api/v1/torrents").browser().cookie(&cookie)).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // A client that isn't a trusted proxy can't claim HTTPS.
    let r = send(
        &app,
        Req::post("/api/v1/auth/logout")
            .peer("192.0.2.1")
            .header("x-forwarded-proto", "https")
            .browser(),
    )
    .await;
    assert!(r.set_cookie().unwrap().starts_with("apitorrent-session="));

    // The passkey belongs to the public host: localhost has none to offer...
    let start = post(&app, "/api/v1/auth/login/passkey/start", json!({}), None)
        .await
        .json();
    assert!(soft.authenticate(&start["options"], ORIGIN).is_err());
    // ...and a ceremony started on one host can't be finished on another.
    let start = send(
        &app,
        via_proxy(Req::post("/api/v1/auth/login/passkey/start")),
    )
    .await
    .json();
    let cred = soft.authenticate(&start["options"], public).unwrap();
    let r = post(
        &app,
        "/api/v1/auth/login/passkey/finish",
        json!({ "ceremonyId": start["ceremonyId"], "credential": cred }),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    core.shutdown().await.unwrap();
}

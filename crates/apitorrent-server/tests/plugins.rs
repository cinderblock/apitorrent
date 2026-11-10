//! `/api/v1/plugins`: who may manage plugins, and how plugin routes are forwarded. A fake
//! plugin service stands in for the V8 host (tested in `apitorrent-plugins`).

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{Value, json};

use apitorrent_auth::{ClientMeta, CreateTokenRequest, PasswordMode, TokenScope};
use apitorrent_plugin_api::{
    BoxFuture, InstallPluginResponse, PluginBundle, PluginCapabilities, PluginError,
    PluginHttpRequest, PluginHttpResponse, PluginInfo, PluginLogEntry, PluginRouteAuth,
    PluginService, PluginSettings, PluginStatus,
};
use apitorrent_server::{AuthMode, PluginListener, ServerConfig, plugin_credentials, router};

use common::{AuthFixture, PASSWORD, Req, core, send, server_config};

#[derive(Default)]
struct Fake {
    seen: Mutex<Vec<PluginHttpRequest>>,
    calls: Mutex<Vec<String>>,
}

fn info(id: &str) -> PluginInfo {
    PluginInfo {
        id: id.to_owned(),
        status: PluginStatus::Running,
        enabled: true,
        active: None,
        granted: PluginCapabilities::default(),
        pending: None,
        error: None,
        installed_at: 0,
        updated_at: 0,
    }
}

impl PluginService for Fake {
    fn list(&self) -> Vec<PluginInfo> {
        vec![info("rss")]
    }
    fn get(&self, id: &str) -> Result<PluginInfo, PluginError> {
        if id == "rss" {
            Ok(info(id))
        } else {
            Err(PluginError::NotFound)
        }
    }
    fn install(&self, b: PluginBundle) -> BoxFuture<Result<InstallPluginResponse, PluginError>> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("install {}", b.manifest.id));
        Box::pin(async move {
            Ok(InstallPluginResponse {
                plugin: info(&b.manifest.id),
                needs_approval: true,
            })
        })
    }
    fn approve(&self, id: String, v: String) -> BoxFuture<Result<PluginInfo, PluginError>> {
        self.calls.lock().unwrap().push(format!("approve {id} {v}"));
        Box::pin(async move { Ok(info(&id)) })
    }
    fn set_enabled(&self, id: String, on: bool) -> BoxFuture<Result<PluginInfo, PluginError>> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("enabled {id} {on}"));
        Box::pin(async move { Ok(info(&id)) })
    }
    fn uninstall(&self, id: String) -> BoxFuture<Result<(), PluginError>> {
        self.calls.lock().unwrap().push(format!("uninstall {id}"));
        Box::pin(async { Ok(()) })
    }
    fn logs(&self, _: &str, _: i64, _: Option<u64>) -> Result<Vec<PluginLogEntry>, PluginError> {
        Ok(Vec::new())
    }
    fn settings(&self, _: &str, _: i64) -> Result<PluginSettings, PluginError> {
        Ok(PluginSettings {
            schema: None,
            values: json!({}),
            secrets_set: vec![],
        })
    }
    fn set_settings(
        &self,
        _: String,
        _: i64,
        v: Value,
    ) -> BoxFuture<Result<PluginSettings, PluginError>> {
        Box::pin(async move {
            Ok(PluginSettings {
                schema: None,
                values: v,
                secrets_set: vec![],
            })
        })
    }
    fn route_auth(&self, id: &str, _method: &str, path: &str) -> Option<PluginRouteAuth> {
        match (id, path) {
            ("rss", "/missing") => None,
            ("rss", p) if p.starts_with("/api/v2/") => Some(PluginRouteAuth::Plugin),
            ("rss", _) => Some(PluginRouteAuth::Api),
            _ => None,
        }
    }
    fn http(
        &self,
        id: String,
        req: PluginHttpRequest,
    ) -> BoxFuture<Result<PluginHttpResponse, PluginError>> {
        self.seen.lock().unwrap().push(req.clone());
        Box::pin(async move {
            if id != "rss" {
                return Err(PluginError::NotFound);
            }
            if req.path == "/missing" {
                return Err(PluginError::NoRoute);
            }
            if req.path.starts_with("/api/v2/") {
                return Ok(PluginHttpResponse {
                    status: 200,
                    headers: vec![
                        (
                            "set-cookie".into(),
                            "SID=abc; HttpOnly; SameSite=Strict; path=/".into(),
                        ),
                        (
                            "set-cookie".into(),
                            "apitorrent-session=evil; Path=/".into(),
                        ),
                        (
                            "set-cookie".into(),
                            "__Host-apitorrent-session=evil; Path=/; Secure".into(),
                        ),
                        ("set-cookie".into(), "wide=1; Domain=example.com".into()),
                        ("content-type".into(), "text/plain".into()),
                    ],
                    body: b"Ok.".to_vec(),
                });
            }
            Ok(PluginHttpResponse {
                status: 200,
                headers: vec![
                    ("content-type".into(), "text/html".into()),
                    ("set-cookie".into(), "stolen=1".into()),
                    ("content-security-policy".into(), "default-src *".into()),
                    ("x-plugin".into(), "yes".into()),
                ],
                body: b"<script>alert(1)</script>".to_vec(),
            })
        })
    }
    fn shutdown(&self) -> BoxFuture<()> {
        Box::pin(async {})
    }
    /// `rss` runs per space; anything else is shared.
    fn space_aware(&self, id: &str) -> bool {
        id == "rss"
    }
}

fn config(auth: AuthMode, fake: &Arc<Fake>) -> ServerConfig {
    ServerConfig {
        plugins: Some(fake.clone() as Arc<dyn PluginService>),
        updates: None,
        ..server_config(auth)
    }
}

fn bundle() -> Value {
    json!({
        "format": 1,
        "manifest": { "id": "rss", "name": "RSS", "version": "1.0.0", "apiVersion": 1 },
        "code": "export default { start() {} };"
    })
}

/// An admin session cookie (fresh) and a full-scope token.
fn admin(f: &AuthFixture) -> (String, String) {
    let meta = ClientMeta::default();
    let s = f
        .auth
        .setup_with_password(&f.setup_token(), "admin", None, PASSWORD, &meta)
        .unwrap();
    let p = f.auth.authenticate_session(&s.secret).unwrap().unwrap();
    let token = f
        .auth
        .create_token(
            &p,
            &CreateTokenRequest {
                name: "script".into(),
                scope: TokenScope::Full,
                expires_in_days: None,
            },
            &meta,
        )
        .unwrap()
        .token;
    (format!("apitorrent-session={}", s.secret), token)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn management_needs_an_admin_and_recent_sign_in_for_grants() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let fake = Arc::new(Fake::default());
    let app = router(
        core.clone(),
        &config(AuthMode::Enabled(f.auth.clone()), &fake),
    );
    let (cookie, token) = admin(&f);

    // Nobody signed in: nothing.
    assert_eq!(
        send(&app, Req::get("/api/v1/plugins")).await.status,
        StatusCode::UNAUTHORIZED
    );
    // A token can list, enable and disable…
    let r = send(&app, Req::get("/api/v1/plugins").bearer(&token)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()[0]["id"], "rss");
    let r = send(
        &app,
        Req::post("/api/v1/plugins/rss/-/disable").bearer(&token),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    // …but never install or approve: those grant privileges.
    let r = send(
        &app,
        Req::post("/api/v1/plugins").bearer(&token).json(&bundle()),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.json());
    let r = send(
        &app,
        Req::post("/api/v1/plugins/rss/-/approve")
            .bearer(&token)
            .json(&json!({ "version": "1.0.0" })),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // A fresh admin session can.
    let r = send(
        &app,
        Req::post("/api/v1/plugins")
            .browser()
            .cookie(&cookie)
            .json(&bundle()),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json());
    assert_eq!(r.json()["needsApproval"], true);
    let r = send(
        &app,
        Req::post("/api/v1/plugins/rss/-/approve")
            .browser()
            .cookie(&cookie)
            .json(&json!({ "version": "1.0.0" })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    // Ten minutes later it needs to sign in again.
    f.advance(Duration::from_secs(11 * 60));
    let r = send(
        &app,
        Req::new("DELETE", "/api/v1/plugins/rss")
            .browser()
            .cookie(&cookie),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.code(), "reauth_required");
    assert_eq!(
        *fake.calls.lock().unwrap(),
        ["enabled rss false", "install rss", "approve rss 1.0.0"]
    );
    // Unknown plugins and actions are 404s.
    let r = send(&app, Req::get("/api/v1/plugins/nope").bearer(&token)).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = send(
        &app,
        Req::post("/api/v1/plugins/rss/-/explode").bearer(&token),
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugin_routes_are_forwarded_without_credentials_and_sandboxed() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let fake = Arc::new(Fake::default());
    let app = router(
        core.clone(),
        &config(AuthMode::Enabled(f.auth.clone()), &fake),
    );
    let (_, token) = admin(&f);

    assert_eq!(
        send(&app, Req::get("/api/v1/plugins/rss/status"))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let r = send(
        &app,
        Req::get("/api/v1/plugins/rss/feeds/a?x=1")
            .bearer(&token)
            .header("cookie", "apitorrent-session=secret")
            .header("x-custom", "kept"),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers["x-plugin"], "yes");
    assert!(r.headers.get("set-cookie").is_none());
    let csp = r.headers["content-security-policy"].to_str().unwrap();
    assert!(csp.starts_with("sandbox"), "{csp}");
    let seen = fake.seen.lock().unwrap()[0].clone();
    assert_eq!(seen.path, "/feeds/a");
    assert_eq!(seen.query.as_deref(), Some("x=1"));
    assert_eq!(
        seen.url,
        "http://localhost:8742/api/v1/plugins/rss/feeds/a?x=1"
    );
    let caller = seen.caller.expect("an ordinary route always has a caller");
    assert_eq!(caller.user_name, "admin");
    assert!(caller.may_write);
    let names: Vec<&str> = seen.headers.iter().map(|(k, _)| k.as_str()).collect();
    assert!(names.contains(&"x-custom"));
    assert!(
        !names.contains(&"cookie") && !names.contains(&"authorization"),
        "{names:?}"
    );

    let r = send(&app, Req::get("/api/v1/plugins/rss/missing").bearer(&token)).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.code(), "no_route");
}

/// A read-only token for the fixture's admin.
fn read_token(f: &AuthFixture) -> String {
    let meta = ClientMeta::default();
    let s = f
        .auth
        .password_login("admin", PASSWORD, &meta, None)
        .unwrap();
    let p = f.auth.authenticate_session(&s.secret).unwrap().unwrap();
    f.auth
        .create_token(
            &p,
            &CreateTokenRequest {
                name: "reader".into(),
                scope: TokenScope::Read,
                expires_in_days: None,
            },
            &meta,
        )
        .unwrap()
        .token
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugin_authenticated_routes_take_requests_without_sign_in() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let fake = Arc::new(Fake::default());
    let app = router(
        core.clone(),
        &config(AuthMode::Enabled(f.auth.clone()), &fake),
    );
    let (cookie, token) = admin(&f);

    // No sign-in: the plugin gets the request, with no caller. It sees its own cookie,
    // never apitorrent's session.
    let r = send(
        &app,
        Req::post("/api/v1/plugins/rss/api/v2/auth/login")
            .header(
                "cookie",
                "SID=old; apitorrent-session=secret; __Host-apitorrent-session=x",
            )
            .header("authorization", "Basic dXNlcjpwYXNz")
            .header("content-type", "application/x-www-form-urlencoded"),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body, b"Ok.");
    let seen = fake.seen.lock().unwrap().pop().unwrap();
    assert_eq!(seen.caller, None);
    assert_eq!(seen.path, "/api/v2/auth/login");
    let cookies: Vec<&str> = seen
        .headers
        .iter()
        .filter(|(k, _)| k == "cookie")
        .map(|(_, v)| v.as_str())
        .collect();
    assert_eq!(cookies, ["SID=old"]);
    assert!(!seen.headers.iter().any(|(k, _)| k == "authorization"));
    // It may set its own cookie, but not apitorrent's, and not for other hosts.
    let set: Vec<&str> = r
        .headers
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect();
    assert_eq!(set, ["SID=abc; HttpOnly; SameSite=Strict; path=/"]);
    let csp = r.headers["content-security-policy"].to_str().unwrap();
    assert!(csp.starts_with("sandbox"), "still sandboxed: {csp}");

    // A request that did sign in tells the plugin who called, and whether it may write.
    let r = send(
        &app,
        Req::get("/api/v1/plugins/rss/api/v2/app/version").bearer(&token),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let caller = fake.seen.lock().unwrap().pop().unwrap().caller.unwrap();
    assert!(caller.may_write);
    let reader = read_token(&f);
    // A read-only token writing to a plugin-authenticated route reaches the plugin, which
    // must refuse it itself: it is told the caller may not write.
    let r = send(
        &app,
        Req::post("/api/v1/plugins/rss/api/v2/torrents/pause").bearer(&reader),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let caller = fake.seen.lock().unwrap().pop().unwrap().caller.unwrap();
    assert!(!caller.may_write);
    // A browser session counts as signing in too.
    let r = send(
        &app,
        Req::get("/api/v1/plugins/rss/api/v2/app/version")
            .browser()
            .cookie(&cookie),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(fake.seen.lock().unwrap().pop().unwrap().caller.is_some());
    // A bad bearer token is still refused, even here.
    let r = send(
        &app,
        Req::get("/api/v1/plugins/rss/api/v2/app/version").bearer("apt_bogus"),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    // Ordinary routes are unchanged: sign-in needed, a stray Basic header refused, and the
    // plugin's cookies dropped.
    let r = send(&app, Req::get("/api/v1/plugins/rss/status")).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = send(
        &app,
        Req::get("/api/v1/plugins/rss/status").header("authorization", "Basic dXNlcjpwYXNz"),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.code(), "invalid_token");
    // Cross-site writes are refused before any plugin sees them.
    let r = send(
        &app,
        Req::post("/api/v1/plugins/rss/api/v2/auth/login").header("origin", "https://evil.example"),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugins_check_api_tokens_through_the_auth_database() {
    let root = tempfile::tempdir().unwrap();
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    assert!(plugin_credentials(&AuthMode::DisabledOnLoopback).is_none());
    let creds = plugin_credentials(&AuthMode::Enabled(f.auth.clone())).unwrap();
    let (cookie, token) = admin(&f);
    let session = cookie.split_once('=').unwrap().1;

    let v = creds
        .verify_token(&token, 1)
        .unwrap()
        .expect("a live token");
    assert_eq!(v.caller.user_name, "admin");
    assert!(v.caller.may_write);
    assert!(v.still_valid());
    let reader = creds.verify_token(&read_token(&f), 1).unwrap().unwrap();
    assert!(!reader.caller.may_write);
    // Session secrets, passwords and junk are not tokens.
    for not in [session, PASSWORD, "apt_nope", ""] {
        assert!(creds.verify_token(not, 1).unwrap().is_none(), "{not}");
    }
    // Revoking the token: every later check says so.
    let p = f.auth.authenticate_session(session).unwrap().unwrap();
    f.auth
        .revoke_token(&p, &v.token_id, &ClientMeta::default())
        .unwrap();
    assert!(!v.still_valid());
    assert!(creds.verify_token(&token, 1).unwrap().is_none());
}

/// One HTTP/1.1 request on a fresh connection; the status line, headers and body.
async fn raw_http(addr: std::net::SocketAddr, request: &str) -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(request.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plugin_listener_serves_one_plugin_at_its_root() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let fake = Arc::new(Fake::default());
    let (_, token) = admin(&f);
    let cfg = config(AuthMode::Enabled(f.auth.clone()), &fake);

    // Not a plugin id; and a LAN address over plain HTTP follows the API's rules.
    assert!(
        PluginListener::bind(
            core.clone(),
            cfg.clone(),
            "Not An Id".into(),
            "127.0.0.1:0".parse().unwrap(),
            None
        )
        .await
        .is_err()
    );
    let lan = PluginListener::bind(
        core.clone(),
        ServerConfig {
            trusted_proxies: apitorrent_server::TrustedProxies::default(),
            ..cfg.clone()
        },
        "rss".into(),
        "0.0.0.0:0".parse().unwrap(),
        None,
    )
    .await;
    assert!(
        lan.is_err(),
        "plain HTTP on a LAN address needs a proxy or --allow-insecure-lan"
    );

    let listener = PluginListener::bind(
        core.clone(),
        cfg,
        "rss".into(),
        "127.0.0.1:0".parse().unwrap(),
        None,
    )
    .await
    .unwrap();
    let addr = listener.local_addr().unwrap();
    let running = tokio::spawn(listener.run(std::future::pending()));

    let r = raw_http(
        addr,
        "POST /api/v2/auth/login?x=1 HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    assert!(r.ends_with("Ok."), "{r}");
    assert!(
        r.to_ascii_lowercase().contains("set-cookie: sid=abc"),
        "{r}"
    );
    let seen = fake.seen.lock().unwrap().pop().unwrap();
    assert_eq!(seen.path, "/api/v2/auth/login");
    assert_eq!(seen.query.as_deref(), Some("x=1"));
    assert_eq!(seen.url, "http://localhost/api/v2/auth/login?x=1");

    // Ordinary plugin routes still need sign-in, and work with it.
    let r = raw_http(
        addr,
        "GET /status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(r.starts_with("HTTP/1.1 401"), "{r}");
    let r = raw_http(
        addr,
        &format!("GET /status HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    // Nothing but the plugin's routes: no management, no API, no host it doesn't answer to.
    for path in ["/-/approve", "/../../torrents", "/%2e%2e/torrents"] {
        let r = raw_http(
            addr,
            &format!("POST {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 404"), "{path}: {r}");
    }
    let r = raw_http(
        addr,
        "GET /api/v2/app/version HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(r.starts_with("HTTP/1.1 421"), "{r}");
    assert!(
        fake.calls.lock().unwrap().is_empty(),
        "no management call got through"
    );
    running.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_host_plugins_are_off() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let app = router(core.clone(), &server_config(AuthMode::DisabledOnLoopback));
    let r = send(&app, Req::get("/api/v1/plugins")).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(r.code(), "plugins_disabled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn space_admins_manage_their_space_aware_plugins_only() {
    use apitorrent_auth::Role;
    use apitorrent_types::spaces::{CreateSpaceRequest, SpaceRole, SpaceSettings};

    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let fake = Arc::new(Fake::default());
    let app = router(
        core.clone(),
        &config(AuthMode::Enabled(f.auth.clone()), &fake),
    );
    let meta = ClientMeta::default();
    let s = f
        .auth
        .setup_with_password(&f.setup_token(), "admin", None, PASSWORD, &meta)
        .unwrap();
    let admin = f.auth.authenticate_session(&s.secret).unwrap().unwrap();
    let tv = core
        .create_space(CreateSpaceRequest {
            slug: "tv".into(),
            name: "TV".into(),
            description: String::new(),
            settings: SpaceSettings::default(),
        })
        .await
        .unwrap()
        .id;
    // Sam administers tv; Una only uses it.
    let member = |name: &str, role| {
        let link = f
            .auth
            .create_invitation(&admin, Role::User, &[(tv, role)], None, None, &meta)
            .unwrap();
        let s = f
            .auth
            .enroll_with_password(&link.secret, name, None, PASSWORD, &meta, None)
            .unwrap();
        format!("apitorrent-session={}", s.secret)
    };
    let sam = member("sam", SpaceRole::Admin);
    let una = member("una", SpaceRole::User);
    let as_ = |method: &'static str, path: &str, cookie: &str| {
        Req::new(method, path)
            .browser()
            .cookie(cookie)
            .json(&json!({ "values": {} }))
    };

    // Sam sees the plugins and manages rss's instance in tv: settings and logs.
    for (method, path) in [
        ("GET", "/s/tv/api/v1/plugins"),
        ("GET", "/s/tv/api/v1/plugins/rss"),
        ("GET", "/s/tv/api/v1/plugins/rss/-/settings"),
        ("PUT", "/s/tv/api/v1/plugins/rss/-/settings"),
        ("GET", "/s/tv/api/v1/plugins/rss/-/logs"),
    ] {
        let r = send(&app, as_(method, path, &sam)).await;
        assert_eq!(r.status, StatusCode::OK, "{method} {path}: {}", r.json());
    }
    // A shared plugin's settings, and managing plugins at all, stay with server admins.
    for (method, path) in [
        ("GET", "/s/tv/api/v1/plugins/shared/-/settings"),
        ("GET", "/s/tv/api/v1/plugins/shared/-/logs"),
        ("POST", "/s/tv/api/v1/plugins/rss/-/disable"),
        ("POST", "/s/tv/api/v1/plugins/rss/-/enable"),
        ("DELETE", "/s/tv/api/v1/plugins/rss"),
    ] {
        let r = send(&app, as_(method, path, &sam)).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{method} {path}");
    }
    let r = send(
        &app,
        Req::post("/s/tv/api/v1/plugins")
            .browser()
            .cookie(&sam)
            .json(&bundle()),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "install");
    // Outside tv (default isn't Sam's), nothing.
    let r = send(&app, as_("GET", "/s/default/api/v1/plugins", &sam)).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    // A member who isn't tv's admin can't look either.
    for path in [
        "/s/tv/api/v1/plugins",
        "/s/tv/api/v1/plugins/rss/-/settings",
    ] {
        let r = send(&app, as_("GET", path, &una)).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{path}");
    }
    assert!(
        !fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("enabled")),
        "nothing was switched"
    );
    core.shutdown().await.unwrap();
}

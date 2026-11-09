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
    PluginHttpRequest, PluginHttpResponse, PluginInfo, PluginLogEntry, PluginService,
    PluginSettings, PluginStatus,
};
use apitorrent_server::{AuthMode, ServerConfig, router};

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
    fn logs(&self, _: &str, _: Option<u64>) -> Result<Vec<PluginLogEntry>, PluginError> {
        Ok(Vec::new())
    }
    fn settings(&self, _: &str) -> Result<PluginSettings, PluginError> {
        Ok(PluginSettings {
            schema: None,
            values: json!({}),
            secrets_set: vec![],
        })
    }
    fn set_settings(&self, _: String, v: Value) -> BoxFuture<Result<PluginSettings, PluginError>> {
        Box::pin(async move {
            Ok(PluginSettings {
                schema: None,
                values: v,
                secrets_set: vec![],
            })
        })
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
}

fn config(auth: AuthMode, fake: &Arc<Fake>) -> ServerConfig {
    ServerConfig {
        plugins: Some(fake.clone() as Arc<dyn PluginService>),
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
    assert_eq!(seen.caller.user_name, "admin");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_host_plugins_are_off() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let app = router(core.clone(), &server_config(AuthMode::DisabledOnLoopback));
    let r = send(&app, Req::get("/api/v1/plugins")).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(r.code(), "plugins_disabled");
}

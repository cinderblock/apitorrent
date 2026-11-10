//! Shared test support: a core, an auth service with a manual clock, a request builder, and
//! a software WebAuthn authenticator.

#![allow(dead_code)]

pub mod authenticator;

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, Response, StatusCode};
use base64::Engine as _;
use http_body_util::BodyExt;
use tower::ServiceExt;

use apitorrent_auth::{
    Auth, AuthConfig, ClientMeta, CreateTokenRequest, ManualClock, PasswordCost, PasswordMode,
    SetupToken, TokenScope,
};
use apitorrent_core::{Core, CoreConfig};
use apitorrent_engine::SessionConfig;
use apitorrent_metainfo::{FileSource, TorrentBuilder, Version};
use apitorrent_server::{AuthMode, ServerConfig, TrustedProxies, WebUi, router};

pub const CHEAP: PasswordCost = PasswordCost {
    memory_kib: 256,
    iterations: 1,
    parallelism: 1,
};

pub const PASSWORD: &str = "correct horse battery staple";

pub async fn core(root: &std::path::Path) -> Core {
    Core::start(CoreConfig {
        data_dir: root.join("data"),
        download_dir: root.join("downloads"),
        session: SessionConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            ..SessionConfig::default()
        },
    })
    .await
    .unwrap()
}

pub fn torrent_json() -> String {
    torrent_json_named("hello.txt")
}

pub fn torrent_json_named(name: &str) -> String {
    let t = TorrentBuilder::new(name, Version::Hybrid)
        .piece_length(16 * 1024)
        .file(FileSource::from_bytes(&[], b"hello world".to_vec()))
        .build()
        .unwrap();
    serde_json::json!({
        "source": {
            "kind": "torrentFile",
            "data": base64::engine::general_purpose::STANDARD.encode(t),
        },
        "paused": true,
    })
    .to_string()
}

/// An auth service in a temp dir, with a clock the test controls.
pub struct AuthFixture {
    pub auth: Auth,
    pub clock: Arc<ManualClock>,
    tokens: Arc<Mutex<Vec<String>>>,
}

impl AuthFixture {
    pub fn new(root: &std::path::Path, mode: PasswordMode, public_urls: &[&str]) -> Self {
        let clock = Arc::new(ManualClock::new(1_800_000_000_000));
        let tokens = Arc::new(Mutex::new(Vec::new()));
        let seen = tokens.clone();
        let mut c = AuthConfig::new(root.join("data").join("auth.db"));
        c.password_mode = mode;
        c.password_cost = CHEAP;
        c.public_urls = public_urls
            .iter()
            .map(|u| url::Url::parse(u).unwrap())
            .collect();
        c.clock = clock.clone();
        c.on_setup_token = Some(Arc::new(move |t: &SetupToken| {
            seen.lock().unwrap().push(t.token.clone());
        }));
        Self {
            auth: Auth::open(c).unwrap(),
            clock,
            tokens,
        }
    }

    pub fn setup_token(&self) -> String {
        self.tokens.lock().unwrap().last().unwrap().clone()
    }

    pub fn advance(&self, d: Duration) {
        self.clock.advance(d);
    }

    /// Bootstrap an admin with a password (fallback mode only) and return a full API token.
    pub fn admin_token(&self) -> String {
        let meta = ClientMeta::default();
        let s = self
            .auth
            .setup_with_password(&self.setup_token(), "admin", None, PASSWORD, &meta)
            .unwrap();
        let p = self.auth.authenticate_session(&s.secret).unwrap().unwrap();
        self.auth
            .create_token(
                &p,
                &CreateTokenRequest {
                    name: "tests".into(),
                    scope: TokenScope::Full,
                    expires_in_days: None,
                },
                &meta,
            )
            .unwrap()
            .token
    }
}

pub fn server_config(auth: AuthMode) -> ServerConfig {
    ServerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        public_hosts: vec!["torrents.example.com".into()],
        web_ui: WebUi::None,
        auth,
        trusted_proxies: TrustedProxies::parse(&["10.0.0.0/8".into()]).unwrap(),
        allow_insecure_lan: false,
        plugins: None,
        updates: None,
    }
}

/// A request builder. Defaults: `Host: localhost:8742`, from 127.0.0.1.
pub struct Req {
    method: &'static str,
    uri: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
    peer: IpAddr,
}

impl Req {
    pub fn new(method: &'static str, uri: impl Into<String>) -> Self {
        Self {
            method,
            uri: uri.into(),
            headers: vec![("host".into(), "localhost:8742".into())],
            body: None,
            peer: IpAddr::from([127, 0, 0, 1]),
        }
    }
    pub fn get(uri: impl Into<String>) -> Self {
        Self::new("GET", uri)
    }
    pub fn post(uri: impl Into<String>) -> Self {
        Self::new("POST", uri)
    }
    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.retain(|(name, _)| name != k);
        self.headers.push((k.into(), v.into()));
        self
    }
    pub fn host(self, h: &str) -> Self {
        self.header("host", h)
    }
    /// A same-origin browser request (the UI's own `fetch`).
    pub fn browser(self) -> Self {
        let host = self
            .headers
            .iter()
            .find(|(k, _)| k == "host")
            .map(|(_, v)| v.clone())
            .unwrap();
        let scheme = if host.contains("example.com") {
            "https"
        } else {
            "http"
        };
        self.header("origin", &format!("{scheme}://{host}"))
            .header("sec-fetch-site", "same-origin")
    }
    pub fn cookie(self, c: &str) -> Self {
        self.header("cookie", c)
    }
    pub fn bearer(self, t: &str) -> Self {
        self.header("authorization", &format!("Bearer {t}"))
    }
    pub fn peer(mut self, ip: &str) -> Self {
        self.peer = ip.parse().unwrap();
        self
    }
    pub fn json(mut self, v: &serde_json::Value) -> Self {
        self.body = Some(v.to_string());
        self.header("content-type", "application/json")
    }
    pub fn raw_json(mut self, s: String) -> Self {
        self.body = Some(s);
        self.header("content-type", "application/json")
    }
    pub fn build(self) -> Request<Body> {
        let mut b = Request::builder().method(self.method).uri(&self.uri);
        for (k, v) in &self.headers {
            b = b.header(k.as_str(), v.as_str());
        }
        let mut r = b
            .body(self.body.map_or_else(Body::empty, Body::from))
            .unwrap();
        r.extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(self.peer, 50_000)));
        r
    }
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&self.body)))
    }
    pub fn typed<T: serde::de::DeserializeOwned>(&self) -> T {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&self.body)))
    }
    pub fn code(&self) -> String {
        self.json()["code"].as_str().unwrap_or_default().to_owned()
    }
    pub fn set_cookie(&self) -> Option<String> {
        self.headers
            .get("set-cookie")
            .map(|v| v.to_str().unwrap().to_owned())
    }
    /// `name=value` from `Set-Cookie`, ready for a `Cookie` header.
    pub fn cookie(&self) -> String {
        let sc = self.set_cookie().expect("a Set-Cookie header");
        sc.split(';').next().unwrap().to_owned()
    }
}

pub async fn send(app: &axum::Router, r: Req) -> Resp {
    let resp: Response<Body> = app.clone().oneshot(r.build()).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    Resp {
        status,
        headers,
        body,
    }
}

pub fn app(core: &Core, auth: AuthMode) -> axum::Router {
    router(core.clone(), &server_config(auth))
}

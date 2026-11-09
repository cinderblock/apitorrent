//! End to end: the first-party `qbittorrent-api` plugin, built with Bun, in the real plugin
//! host, over the real core, auth database and HTTP server, spoken to the way Sonarr speaks
//! to qBittorrent: through the API's URL base and through a listener of its own.
//!
//! Needs `bun` on the PATH. Everything listens on 127.0.0.1.

#![allow(
    clippy::many_single_char_names,
    reason = "a long conversation of short-lived responses"
)]

mod common;

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use apitorrent_auth::{
    Auth, AuthConfig, ClientMeta, CreateTokenRequest, PasswordCost, PasswordMode, SetupToken,
    TokenScope,
};
use apitorrent_core::plugin_api::CorePluginApi;
use apitorrent_core::{Core, CoreConfig};
use apitorrent_engine::SessionConfig;
use apitorrent_metainfo::{FileSource, TorrentBuilder, Version};
use apitorrent_plugin_api::{PluginService, PluginStatus};
use apitorrent_plugins::{HostConfig, PluginHost};
use apitorrent_server::{
    AuthMode, PluginListener, ServerConfig, TrustedProxies, WebUi, plugin_credentials, router,
};
use apitorrent_types::TorrentState;
use common::build_plugin;

/// A response: status, headers (lowercase names), body.
struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.text()))
    }
}

/// One HTTP/1.1 request on its own connection.
async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Resp {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        let _ = write!(head, "{k}: {v}\r\n");
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).await.unwrap();
    s.write_all(body).await.unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.unwrap();
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a response head");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap()[9..12].parse().unwrap();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let mut body = raw[split + 4..].to_vec();
    if headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.contains("chunked"))
    {
        body = dechunk(&body);
    }
    Resp {
        status,
        headers,
        body,
    }
}

fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let line = data.windows(2).position(|w| w == b"\r\n").unwrap();
        let size =
            usize::from_str_radix(std::str::from_utf8(&data[..line]).unwrap().trim(), 16).unwrap();
        data = &data[line + 2..];
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

/// `application/x-www-form-urlencoded` for one value.
fn form_encode(v: &str) -> String {
    v.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

const FORM: (&str, &str) = ("content-type", "application/x-www-form-urlencoded");

/// A multipart body the way Sonarr's `HttpFormData` writes it.
fn multipart(fields: &[(&str, &str)], file: (&str, &[u8])) -> (String, Vec<u8>) {
    let boundary = "----------------------------8dd1f6a2b3c4d5e";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"torrents\"; filename=\"{}\"\r\nContent-Type: application/x-bittorrent\r\n\r\n",
            file.0
        )
        .as_bytes(),
    );
    body.extend_from_slice(file.1);
    body.extend_from_slice(b"\r\n");
    for (k, v) in fields {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                .as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

fn sid(r: &Resp) -> String {
    let c = r.header("set-cookie").expect("a session cookie");
    c.strip_prefix("SID=")
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sonarr_talks_to_apitorrent_as_if_it_were_qbittorrent() {
    let root = tempfile::tempdir().unwrap();
    let bundle = build_plugin("qbittorrent-api", &root.path().join("dist"));
    let downloads = root.path().join("downloads");
    let core = Core::start(CoreConfig {
        data_dir: root.path().join("data"),
        download_dir: downloads.clone(),
        session: SessionConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            ..SessionConfig::default()
        },
    })
    .await
    .unwrap();

    // An admin with a full token and a read-only one.
    let setup = Arc::new(Mutex::new(String::new()));
    let seen = setup.clone();
    let mut ac = AuthConfig::new(root.path().join("data").join("auth.db"));
    ac.password_mode = PasswordMode::Fallback;
    ac.password_cost = PasswordCost {
        memory_kib: 256,
        iterations: 1,
        parallelism: 1,
    };
    ac.on_setup_token = Some(Arc::new(move |t: &SetupToken| {
        *seen.lock().unwrap() = t.token.clone();
    }));
    let auth = Auth::open(ac).unwrap();
    let meta = ClientMeta::default();
    let s = auth
        .setup_with_password(
            &setup.lock().unwrap(),
            "admin",
            None,
            "correct horse battery staple",
            &meta,
        )
        .unwrap();
    let admin = auth.authenticate_session(&s.secret).unwrap().unwrap();
    let token = |scope| {
        auth.create_token(
            &admin,
            &CreateTokenRequest {
                name: format!("{scope:?}"),
                scope,
                expires_in_days: None,
            },
            &meta,
        )
        .unwrap()
    };
    let full = token(TokenScope::Full);
    let read = token(TokenScope::Read);
    let mode = AuthMode::Enabled(auth.clone());

    // The plugin host, with the token check, and the plugin approved and running.
    let mut hc = HostConfig::new(root.path().join("data").join("plugins"));
    hc.credentials = plugin_credentials(&mode);
    let host = PluginHost::start(hc, CorePluginApi::new(core.clone())).unwrap();
    let r = host.install(bundle).await.unwrap();
    assert!(r.needs_approval, "a plugin waits for approval");
    let wanted = r.plugin.pending.unwrap().new_capabilities;
    assert!(
        wanted
            .iter()
            .any(|c| c.contains("haven't signed in to apitorrent")),
        "{wanted:?}"
    );
    host.approve("qbittorrent-api".into(), "1.0.0".into())
        .await
        .unwrap();
    host.set_enabled("qbittorrent-api".into(), true)
        .await
        .unwrap();
    for _ in 0..200 {
        if host.get("qbittorrent-api").unwrap().status == PluginStatus::Running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        host.get("qbittorrent-api").unwrap().status,
        PluginStatus::Running
    );

    let config = ServerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        public_hosts: Vec::new(),
        web_ui: WebUi::None,
        auth: mode,
        trusted_proxies: TrustedProxies::default(),
        allow_insecure_lan: false,
        plugins: Some(Arc::new(host.clone()) as Arc<dyn PluginService>),
    };
    let api = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api.local_addr().unwrap();
    let app = router(core.clone(), &config);
    tokio::spawn(async move {
        axum::serve(api, app.into_make_service_with_connect_info::<SocketAddr>()).await
    });
    let own = PluginListener::bind(
        core.clone(),
        config.clone(),
        "qbittorrent-api".into(),
        "127.0.0.1:0".parse().unwrap(),
        None,
    )
    .await
    .unwrap();
    let own_addr = own.local_addr().unwrap();
    tokio::spawn(own.run(std::future::pending()));

    // Sonarr, with the URL base /api/v1/plugins/qbittorrent-api.
    let base = "/api/v1/plugins/qbittorrent-api/api/v2";
    let r = http(
        api_addr,
        "GET",
        &format!("{base}/app/webapiVersion"),
        &[],
        b"",
    )
    .await;
    assert_eq!(r.status, 403, "not signed in yet: {}", r.text());
    let r = http(
        api_addr,
        "POST",
        &format!("{base}/auth/login"),
        &[FORM],
        format!("username=sonarr&password={}", full.token).as_bytes(),
    )
    .await;
    assert_eq!((r.status, r.text().as_str()), (200, "Ok."));
    let cookie = format!("SID={}", sid(&r));
    let c = ("cookie", cookie.as_str());
    let r = http(api_addr, "GET", &format!("{base}/app/version"), &[c], b"").await;
    assert_eq!(r.text(), "v4.6.7");
    let prefs = http(
        api_addr,
        "GET",
        &format!("{base}/app/preferences"),
        &[c],
        b"",
    )
    .await
    .json();
    assert_eq!(
        Path::new(prefs["save_path"].as_str().unwrap()),
        downloads.as_path()
    );
    // The category's folder as an absolute path, which the plugin keeps relative to the
    // download directory.
    let folder = form_encode(&downloads.join("tv").to_string_lossy());
    let r = http(
        api_addr,
        "POST",
        &format!("{base}/torrents/createCategory"),
        &[c, FORM],
        format!("category=tv&savePath={folder}").as_bytes(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());

    // A .torrent whose data is already on disk, so it seeds as soon as it's checked.
    let data: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
    let torrent = TorrentBuilder::new("Show.S01E01.mkv", Version::Hybrid)
        .piece_length(16 * 1024)
        .file(FileSource::from_bytes(&[], data.clone()))
        .build()
        .unwrap();
    std::fs::create_dir_all(downloads.join("tv")).unwrap();
    std::fs::write(downloads.join("tv").join("Show.S01E01.mkv"), &data).unwrap();
    let (ct, body) = multipart(&[("category", "tv")], ("Show.S01E01.torrent", &torrent));
    let r = http(
        api_addr,
        "POST",
        &format!("{base}/torrents/add"),
        &[c, ("content-type", &ct)],
        &body,
    )
    .await;
    assert_eq!((r.status, r.text().as_str()), (200, "Ok."));
    let t = core.list().pop().expect("added to apitorrent");
    assert_eq!(t.label.as_deref(), Some("tv"));
    assert_eq!(Path::new(&t.save_path), downloads.join("tv"));
    let hash = t.info_hash_v1.clone().unwrap();
    for _ in 0..200 {
        if core
            .store()
            .get(&t.id)
            .is_some_and(|s| s.state == TorrentState::Seeding)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let info = http(
        api_addr,
        "GET",
        &format!("{base}/torrents/info?category=tv"),
        &[c],
        b"",
    )
    .await
    .json();
    let item = &info[0];
    assert_eq!(
        item["hash"],
        hash.as_str(),
        "qBittorrent's hash is the v1 info-hash"
    );
    assert_eq!(item["category"], "tv");
    assert_eq!(item["progress"], 1.0);
    assert!(
        ["stalledUP", "uploading"].contains(&item["state"].as_str().unwrap()),
        "{item}"
    );
    assert_eq!(
        Path::new(item["content_path"].as_str().unwrap()),
        downloads.join("tv").join("Show.S01E01.mkv")
    );
    let files = http(
        api_addr,
        "GET",
        &format!("{base}/torrents/files?hash={}", hash.to_uppercase()),
        &[c],
        b"",
    )
    .await
    .json();
    assert_eq!(files[0]["name"], "Show.S01E01.mkv");
    assert_eq!(files[0]["size"], 70_000);

    // Seed goals and a pause reach the core.
    let r = http(
        api_addr,
        "POST",
        &format!("{base}/torrents/setShareLimits"),
        &[c, FORM],
        format!("hashes={hash}&ratioLimit=2&seedingTimeLimit=-1&inactiveSeedingTimeLimit=-2")
            .as_bytes(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let d = core.detail(&t.id).await.unwrap();
    assert_eq!(d.seed_limits.map(|l| l.ratio), Some(2.0));
    let r = http(
        api_addr,
        "POST",
        &format!("{base}/torrents/pause"),
        &[c, FORM],
        format!("hashes={hash}").as_bytes(),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(core.store().get(&t.id).unwrap().state, TorrentState::Paused);

    // Its own listener, at the root, for clients without a URL base; a read-only token
    // reads but can't change anything.
    let r = http(
        own_addr,
        "POST",
        "/api/v2/auth/login",
        &[FORM],
        format!("username=x&password={}", read.token).as_bytes(),
    )
    .await;
    assert_eq!(r.text(), "Ok.");
    let reader = format!("SID={}", sid(&r));
    let rc = ("cookie", reader.as_str());
    let info = http(own_addr, "GET", "/api/v2/torrents/info", &[rc], b"")
        .await
        .json();
    assert_eq!(info[0]["state"], "pausedUP");
    let r = http(
        own_addr,
        "POST",
        "/api/v2/torrents/resume",
        &[rc, FORM],
        format!("hashes={hash}").as_bytes(),
    )
    .await;
    assert_eq!(r.status, 403);
    assert_eq!(core.store().get(&t.id).unwrap().state, TorrentState::Paused);
    // A bearer token works there without any login.
    let bearer = format!("Bearer {}", full.token);
    let r = http(
        own_addr,
        "GET",
        "/api/v2/torrents/categories",
        &[("authorization", &bearer)],
        b"",
    )
    .await;
    assert_eq!(r.json()["tv"]["name"], "tv");
    // Nothing else of the API is reachable through it.
    let r = http(
        own_addr,
        "GET",
        "/api/v1/torrents",
        &[("authorization", &bearer)],
        b"",
    )
    .await;
    assert_eq!(r.status, 404);

    // Revoking the full token ends Sonarr's session at once.
    auth.revoke_token(&admin, &full.info.id, &meta).unwrap();
    let r = http(api_addr, "GET", &format!("{base}/torrents/info"), &[c], b"").await;
    assert_eq!(r.status, 403);

    // A fresh token, and Sonarr removes the torrent with its data once imported.
    let again = token(TokenScope::Full);
    let r = http(
        api_addr,
        "POST",
        &format!("{base}/auth/login"),
        &[FORM],
        format!("username=sonarr&password={}", again.token).as_bytes(),
    )
    .await;
    let cookie = format!("SID={}", sid(&r));
    let r = http(
        api_addr,
        "POST",
        &format!("{base}/torrents/delete"),
        &[("cookie", &cookie), FORM],
        format!("hashes={hash}&deleteFiles=true").as_bytes(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(core.list().is_empty());

    host.shutdown().await;
    core.shutdown().await.unwrap();
}

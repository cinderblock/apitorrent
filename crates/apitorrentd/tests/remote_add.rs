//! End to end: the first-party `remote-add` plugin, built with Bun, in the real plugin host,
//! over the real core, auth and HTTP server, called the way a link on a tracker's page
//! calls it: a cross-site GET with the key and the `.torrent` link in the query string, no
//! apitorrent sign-in. The "tracker" is a local server the plugin is granted by IP.
//!
//! Needs `bun` on the PATH. Everything listens on 127.0.0.1.

mod common;

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use apitorrent_auth::{Auth, AuthConfig, ClientMeta, PasswordCost, PasswordMode, SetupToken};
use apitorrent_core::plugin_api::CorePluginApi;
use apitorrent_core::{Core, CoreConfig};
use apitorrent_engine::SessionConfig;
use apitorrent_metainfo::{FileSource, TorrentBuilder, Version};
use apitorrent_plugin_api::{PluginService, PluginStatus};
use apitorrent_plugins::{HostConfig, PluginHost};
use apitorrent_server::{
    AuthMode, ServerConfig, TrustedProxies, WebUi, plugin_credentials, router,
};
use apitorrent_types::spaces::DEFAULT_SPACE;
use common::build_plugin;

const KEY: &str = "0123456789abcdef0123456789abcdef";

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
}

/// A GET on its own connection, with the headers a browser sends for a link opened from
/// another site.
async fn get(addr: SocketAddr, path: &str, dest: &str) -> Resp {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut head = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    for (k, v) in [
        ("Origin", "https://tracker.example"),
        ("Referer", "https://tracker.example/torrents.php"),
        ("Sec-Fetch-Site", "cross-site"),
        ("Sec-Fetch-Mode", "navigate"),
        ("Sec-Fetch-Dest", dest),
    ] {
        let _ = write!(head, "{k}: {v}\r\n");
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).await.unwrap();
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

fn encode(v: &str) -> String {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tracker_link_adds_a_torrent_the_server_fetches() {
    let root = tempfile::tempdir().unwrap();
    let bundle = build_plugin("remote-add", &root.path().join("dist"));
    let core = Core::start(CoreConfig {
        data_dir: root.path().join("data"),
        download_dir: root.path().join("downloads"),
        session: SessionConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            ..SessionConfig::default()
        },
    })
    .await
    .unwrap();

    // Authentication on, so the route is shown to work without signing in.
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
    auth.setup_with_password(
        &setup.lock().unwrap(),
        "admin",
        None,
        "correct horse battery staple",
        &ClientMeta::default(),
    )
    .unwrap();
    let mode = AuthMode::Enabled(auth);

    // The "tracker": a .torrent behind a passkey link, and a sign-in page.
    let torrent = TorrentBuilder::new("Some.Release.mkv", Version::Hybrid)
        .piece_length(16 * 1024)
        .file(FileSource::from_bytes(&[], vec![7u8; 40_000]))
        .build()
        .unwrap();
    let served = torrent.clone();
    let tracker = axum::Router::new()
        .route(
            "/download.php",
            axum::routing::get(move || {
                let t = served.clone();
                async move { ([("content-type", "application/x-bittorrent")], t) }
            }),
        )
        .route(
            "/login.php",
            axum::routing::get(|| async {
                ([("content-type", "text/html")], "<form>sign in</form>")
            }),
        );
    let tracker_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tracker_addr = tracker_listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(tracker_listener, tracker).await });

    let mut hc = HostConfig::new(root.path().join("data").join("plugins"));
    hc.credentials = plugin_credentials(&mode);
    let host = PluginHost::start(hc, CorePluginApi::new(core.clone())).unwrap();
    host.install(bundle).await.unwrap();
    host.approve("remote-add".into(), "1.0.0".into())
        .await
        .unwrap();
    host.set_settings(
        "remote-add".into(),
        DEFAULT_SPACE,
        serde_json::json!({
            "key": KEY,
            "hosts": [format!("http://{tracker_addr}")],
            "label": "tracker",
        }),
    )
    .await
    .unwrap();
    host.set_enabled("remote-add".into(), true).await.unwrap();
    for _ in 0..200 {
        if host.get("remote-add").unwrap().status == PluginStatus::Running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        host.get("remote-add").unwrap().status,
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
        updates: None,
        space_hosts: apitorrent_server::SpaceHosts::default(),
    };
    let api = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = api.local_addr().unwrap();
    let app = router(core.clone(), &config);
    tokio::spawn(async move {
        axum::serve(api, app.into_make_service_with_connect_info::<SocketAddr>()).await
    });

    let link = |path: &str| format!("http://{tracker_addr}{path}");
    let add = |key: &str, url: &str, extra: &str| {
        format!(
            "/api/v1/plugins/remote-add/add?key={}&url={}{extra}",
            encode(key),
            encode(url)
        )
    };

    // A wrong key: refused, nothing fetched or added.
    let r = get(
        addr,
        &add("not-the-key", &link("/download.php?passkey=p"), ""),
        "document",
    )
    .await;
    assert_eq!(r.status, 403, "{}", r.text());
    assert!(core.list(None).is_empty());

    // A host that isn't granted.
    let r = get(
        addr,
        &add(KEY, "http://127.0.0.2:9/a.torrent", ""),
        "document",
    )
    .await;
    assert_eq!(r.status, 403, "{}", r.text());

    // A sign-in page instead of a torrent says so.
    let r = get(addr, &add(KEY, &link("/login.php"), ""), "document").await;
    assert_eq!(r.status, 502, "{}", r.text());
    assert!(r.text().contains("text/html"), "{}", r.text());

    // The popup: a page, sandboxed, not cached.
    let r = get(
        addr,
        &add(KEY, &link("/download.php?passkey=p"), ""),
        "document",
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.text().contains("Added"), "{}", r.text());
    assert!(
        r.header("content-security-policy")
            .unwrap()
            .contains("sandbox")
    );
    assert_eq!(r.header("cache-control"), Some("no-store"));
    let t = core.list(None).pop().expect("added to apitorrent");
    assert_eq!(t.name, "Some.Release.mkv");
    assert_eq!(t.label.as_deref(), Some("tracker"));

    // The image reply, for an <img> on the tracker's page: the same torrent again is fine.
    let r = get(
        addr,
        &add(KEY, &link("/download.php?passkey=p"), "&reply=image"),
        "image",
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("content-type"), Some("image/gif"));
    assert_eq!(core.list(None).len(), 1);

    host.shutdown().await;
}

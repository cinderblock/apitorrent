//! The API over real HTTP semantics: guards, handlers, error mapping and sync.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use futures_util::StreamExt;
use http_body_util::BodyExt;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tower::ServiceExt;

use apitorrent_core::{Core, CoreConfig};
use apitorrent_engine::SessionConfig;
use apitorrent_metainfo::{FileSource, TorrentBuilder, Version};
use apitorrent_server::{ServerConfig, router};
use apitorrent_types::{ApiError, SyncMessage, TorrentDetail, TorrentSummary};

async fn core(root: &std::path::Path) -> Core {
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

fn config() -> ServerConfig {
    ServerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        public_hosts: vec!["torrents.example.com".into()],
        web_root: None,
    }
}

fn torrent_json() -> String {
    let t = TorrentBuilder::new("hello.txt", Version::Hybrid)
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

fn req(
    method: &str,
    uri: &str,
    host: &str,
    origin: Option<&str>,
    body: Option<String>,
) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", host);
    if let Some(o) = origin {
        b = b.header("origin", o);
    }
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    b.body(body.map_or_else(Body::empty, Body::from)).unwrap()
}

async fn json<T: serde::de::DeserializeOwned>(resp: axum::response::Response) -> T {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&bytes)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crud_and_errors() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let app = router(core.clone(), &config());
    let call = |r| app.clone().oneshot(r);

    let resp = call(req(
        "POST",
        "/api/v1/torrents",
        "localhost:8742",
        Some("http://localhost:8742"),
        Some(torrent_json()),
    ))
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let id = json::<serde_json::Value>(resp).await["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let resp = call(req(
        "POST",
        "/api/v1/torrents",
        "localhost",
        None,
        Some(torrent_json()),
    ))
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(json::<ApiError>(resp).await.code, "duplicate");

    let resp = call(req("GET", "/api/v1/torrents", "127.0.0.1:8742", None, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<TorrentSummary> = json(resp).await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "hello.txt");

    let resp = call(req(
        "GET",
        &format!("/api/v1/torrents/{id}"),
        "localhost",
        None,
        None,
    ))
    .await
    .unwrap();
    let detail: TorrentDetail = json(resp).await;
    assert_eq!(detail.files.len(), 1);

    let resp = call(req(
        "PUT",
        &format!("/api/v1/torrents/{id}/files/priorities"),
        "localhost",
        None,
        Some(r#"{"priorities":[{"index":5,"priority":1}]}"#.into()),
    ))
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let resp = call(req(
        "POST",
        "/api/v1/torrents/nope/pause",
        "localhost",
        None,
        None,
    ))
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = call(req(
        "DELETE",
        &format!("/api/v1/torrents/{id}?deleteFiles=true"),
        "localhost",
        None,
        None,
    ))
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guards() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let app = router(core.clone(), &config());
    let call = |r| app.clone().oneshot(r);

    // DNS rebinding: a foreign Host header is refused, even for reads.
    let resp = call(req(
        "GET",
        "/api/v1/torrents",
        "evil.example.com",
        None,
        None,
    ))
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::MISDIRECTED_REQUEST);
    // A configured public host is accepted.
    let resp = call(req(
        "GET",
        "/api/v1/info",
        "torrents.example.com",
        None,
        None,
    ))
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // CSRF: a state change from another site's page is refused.
    let resp = call(req(
        "POST",
        "/api/v1/torrents",
        "localhost",
        Some("https://evil.example.com"),
        Some(torrent_json()),
    ))
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(json::<ApiError>(resp).await.code, "cross_origin");
    // Unknown endpoints get a JSON 404.
    let resp = call(req("GET", "/api/v1/nope", "localhost", None, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refuses_non_loopback_until_auth_exists() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let cfg = ServerConfig {
        listen: "0.0.0.0:0".parse().unwrap(),
        public_hosts: vec![],
        web_root: None,
    };
    let err = apitorrent_server::serve(core.clone(), cfg, std::future::pending())
        .await
        .unwrap_err();
    assert!(matches!(err, apitorrent_server::ServeError::NotLoopback(_)));
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(core.clone(), &config());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/v1/sync"))
        .await
        .unwrap();
    let SyncMessage::Snapshot { revision, torrents } = next_sync(&mut ws).await else {
        panic!("expected a snapshot first");
    };
    assert_eq!(torrents, [] as [apitorrent_types::TorrentSummary; 0]);

    // Adding a torrent produces a patch with the next revision.
    let resp = router(core.clone(), &config())
        .oneshot(req(
            "POST",
            "/api/v1/torrents",
            "localhost",
            None,
            Some(torrent_json()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
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
    let cfg = ServerConfig {
        web_root: Some(web),
        ..config()
    };
    let app = router(core.clone(), &cfg);
    let call = |r| app.clone().oneshot(r);

    let resp = call(req("GET", "/", "localhost", None, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let csp = resp.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    assert_eq!(resp.headers()["x-content-type-options"], "nosniff");

    let resp = call(req("GET", "/assets/app.js", "localhost", None, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // A client-side route falls back to index.html.
    let resp = call(req("GET", "/torrents/abc", "localhost", None, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("<title>ui</title>"));
    // API 404s stay JSON.
    let resp = call(req("GET", "/api/v1/nope", "localhost", None, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    core.shutdown().await.unwrap();
}

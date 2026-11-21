//! Against a fake daemon: the fixture certificate Deluge made, its framing, and replies shaped
//! like `deluge/core/rpcserver.py`'s. The real daemon is in `tests/deluge_daemon.rs`.

use std::sync::Arc;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use super::*;

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/ssl")
        .join(name)
}

#[derive(Debug)]
struct Fixed(Arc<CertifiedKey>);

impl ResolvesServerCert for Fixed {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
}

/// What the fake daemon does with one request: the messages to send back.
type Handler = Box<dyn FnMut(i64, &str, &[Value]) -> Vec<Value> + Send>;

/// Start a fake daemon. Every request it gets is also sent on the returned channel.
async fn fake_daemon(mut handler: Handler) -> (ConnectOptions, mpsc::UnboundedReceiver<String>) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cert = CertificateDer::from_pem_file(fixture("a-daemon.cert")).unwrap();
    let key = PrivateKeyDer::from_pem_file(fixture("a-daemon.pkey")).unwrap();
    let key = provider.key_provider.load_private_key(key).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(Fixed(Arc::new(CertifiedKey::new(
            vec![cert],
            key,
        )))));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut tls = tokio_rustls::TlsAcceptor::from(Arc::new(config))
            .accept(tcp)
            .await
            .unwrap();
        loop {
            let mut header = [0u8; HEADER_LEN];
            if tls.read_exact(&mut header).await.is_err() {
                return;
            }
            let len = frame::parse_header(header, &FrameLimits::default()).unwrap();
            let mut body = vec![0; len];
            tls.read_exact(&mut body).await.unwrap();
            let msg = frame::decode_body(&body, &FrameLimits::default()).unwrap();
            for req in msg.as_list().unwrap() {
                let r = req.as_list().unwrap();
                let id = r[0].as_i64().unwrap();
                let method = r[1].as_str().unwrap().to_owned();
                let _ = tx.send(method.clone());
                for reply in handler(id, &method, r[2].as_list().unwrap()) {
                    tls.write_all(&frame::encode(&reply).unwrap())
                        .await
                        .unwrap();
                }
            }
        }
    });
    let opts = ConnectOptions {
        port,
        call_timeout: Duration::from_millis(500),
        ..ConnectOptions::default()
    };
    (opts, rx)
}

fn pin() -> PinnedCert {
    PinnedCert::from_pem_file(&fixture("a-daemon.cert")).unwrap()
}

fn response(id: i64, v: Value) -> Value {
    Value::List(vec![Value::Int(RPC_RESPONSE), Value::Int(id), v])
}

fn error(id: i64, exception: &str, args: Vec<Value>) -> Value {
    Value::List(vec![
        Value::Int(RPC_ERROR),
        Value::Int(id),
        exception.into(),
        Value::List(args),
        Value::Dict(vec![]),
        "Traceback (most recent call last): ...".into(),
    ])
}

#[tokio::test]
async fn logs_in_and_calls() {
    let (opts, mut seen) = fake_daemon(Box::new(|id, method, args| match method {
        "daemon.login" if args[1].as_str() == Some("pw") => vec![response(id, Value::Int(10))],
        "daemon.login" => vec![error(
            id,
            "BadLoginError",
            vec!["Password does not match".into(), "localclient".into()],
        )],
        "daemon.info" => vec![
            // An event first: it is skipped.
            Value::List(vec![
                Value::Int(RPC_EVENT),
                "TorrentAddedEvent".into(),
                Value::List(vec![]),
            ]),
            response(id, "2.1.1".into()),
        ],
        "core.get_libtorrent_version" => vec![response(id, "2.0.11.0".into())],
        _ => vec![error(
            id,
            "WrappedException",
            vec![
                "RPC call on invalid function".into(),
                "AttributeError".into(),
                "tb".into(),
            ],
        )],
    }))
    .await;
    let mut c = DelugeClient::connect(&opts, &pin()).await.unwrap();
    assert_eq!(
        c.login(&Credentials::new("localclient", "pw"))
            .await
            .unwrap(),
        10
    );
    assert_eq!(c.auth_level(), Some(10));
    assert_eq!(
        c.versions().await.unwrap(),
        Versions {
            deluge: "2.1.1".into(),
            libtorrent: "2.0.11.0".into()
        }
    );
    let e = c.call("core.nope", vec![], vec![]).await.unwrap_err();
    let r = e.remote().expect("a remote error");
    assert_eq!(r.exception, "AttributeError");
    assert_eq!(r.message, "RPC call on invalid function");
    assert_eq!(
        e.to_string(),
        "Deluge's core.nope failed: AttributeError: RPC call on invalid function"
    );
    // A remote error leaves the connection usable.
    assert_eq!(c.deluge_version().await.unwrap(), "2.1.1");
    let e = c
        .login(&Credentials::new("localclient", "wrong"))
        .await
        .unwrap_err();
    assert_eq!(e.remote().unwrap().exception, "BadLoginError");
    assert!(!e.to_string().contains("wrong"), "{e}");
    let mut methods = vec![];
    while let Ok(m) = seen.try_recv() {
        methods.push(m);
    }
    assert_eq!(
        methods,
        [
            "daemon.login",
            "daemon.info",
            "core.get_libtorrent_version",
            "core.nope",
            "daemon.info",
            "daemon.login"
        ]
    );
}

#[tokio::test]
async fn lists_torrents() {
    let (opts, _) = fake_daemon(Box::new(|id, method, args| {
        assert_eq!(method, "core.get_torrents_status");
        assert_eq!(
            args[0],
            Value::dict([("id", Value::str_list(["aa"]))]),
            "filter"
        );
        let status = Value::dict([
            ("name", "Ubuntu".into()),
            ("state", "Seeding".into()),
            ("paused", false.into()),
            ("progress", Value::Float(100.0)),
            ("save_path", "/data".into()),
            ("label", "linux".into()),
            (
                "trackers",
                Value::List(vec![Value::dict([
                    ("url", "https://t.example/announce".into()),
                    ("tier", Value::Int(0)),
                ])]),
            ),
            ("tracker_host", "example".into()),
            ("total_size", Value::Int(5_000_000_000)),
            ("is_finished", true.into()),
            ("tracker_status", "Announce Sent".into()),
        ]);
        let unlabelled = Value::dict([
            ("state", "Weird".into()),
            ("label", "".into()),
            ("progress", Value::Int(3)),
        ]);
        vec![response(
            id,
            Value::Dict(vec![("aa".into(), status), ("bb".into(), unlabelled)]),
        )]
    }))
    .await;
    let mut c = DelugeClient::connect(&opts, &pin()).await.unwrap();
    let ts = c.torrents(Some(&["aa"])).await.unwrap();
    assert_eq!(ts.len(), 2);
    let a = &ts[0];
    assert_eq!(a.id, "aa");
    assert_eq!(a.state, TorrentState::Seeding);
    assert!(!a.is_stopped());
    assert_eq!(a.label.as_deref(), Some("linux"));
    assert_eq!(a.trackers[0].url, "https://t.example/announce");
    assert_eq!(a.total_size, 5_000_000_000);
    assert!(a.is_finished);
    assert!(a.is_announcing());
    let b = &ts[1];
    assert!(!b.is_announcing());
    assert_eq!(b.state, TorrentState::Other("Weird".into()));
    assert_eq!(b.label, None);
    assert!((b.progress - 3.0).abs() < f64::EPSILON);
}

#[tokio::test]
async fn pause_and_resume_send_only_known_ids() {
    // The daemon has aa and bb.
    let (opts, mut seen) = fake_daemon(Box::new(|id, method, args| {
        if method == "core.get_torrents_status" {
            assert_eq!(args[1], Value::str_list(["hash"]));
            let wanted = args[0].get("id").unwrap().as_list().unwrap();
            let have: Vec<(Value, Value)> = wanted
                .iter()
                .filter(|w| matches!(w.as_str(), Some("aa" | "bb")))
                .map(|w| (w.clone(), Value::dict([("hash", w.clone())])))
                .collect();
            return vec![response(id, Value::Dict(have))];
        }
        assert_eq!(args[0], Value::str_list(["aa", "bb"]), "{method}");
        vec![response(id, Value::None)]
    }))
    .await;
    let mut c = DelugeClient::connect(&opts, &pin()).await.unwrap();
    // Nothing, or nothing Deluge has: no pause or resume goes out (Deluge would take an
    // empty list as every torrent).
    assert!(c.pause::<&str>(&[]).await.unwrap().is_empty());
    assert!(c.resume(&["zz"]).await.unwrap().is_empty());
    assert_eq!(c.pause(&["aa", "zz", "bb"]).await.unwrap(), ["aa", "bb"]);
    c.resume(&["aa".to_string(), "bb".to_string()])
        .await
        .unwrap();
    let mut methods = vec![];
    while let Ok(m) = seen.try_recv() {
        methods.push(m);
    }
    assert_eq!(
        methods,
        [
            "core.get_torrents_status",
            "core.get_torrents_status",
            "core.pause_torrent",
            "core.get_torrents_status",
            "core.resume_torrent"
        ]
    );
}

/// A daemon whose torrent `aa` shows `statuses` as its `tracker_status` on successive
/// polls (the last one repeating), and `bb` never announces.
async fn announcing_daemon(statuses: &'static [&'static str]) -> ConnectOptions {
    let mut polls = 0;
    let (opts, _) = fake_daemon(Box::new(move |id, _, _| {
        let now = statuses[polls.min(statuses.len() - 1)];
        polls += 1;
        let t = |s: &str| Value::dict([("state", "Paused".into()), ("tracker_status", s.into())]);
        vec![response(
            id,
            Value::Dict(vec![("aa".into(), t(now)), ("bb".into(), t(""))]),
        )]
    }))
    .await;
    opts
}

#[tokio::test]
async fn waits_for_the_stopped_to_be_answered() {
    // The first poll still shows the answer to the last regular announce.
    let opts = announcing_daemon(&[
        "Announce OK",
        "Announce Sent",
        "Announce Sent",
        "Announce OK",
    ])
    .await;
    let mut c = DelugeClient::connect(&opts, &pin()).await.unwrap();
    let start = std::time::Instant::now();
    let waiting = c
        .wait_until_announced(
            &["aa", "bb"],
            Duration::from_millis(100),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(waiting.is_empty());
    // Three polls 200 ms apart: it waited for the answer, not just the settle time.
    assert!(
        start.elapsed() >= Duration::from_millis(600),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn an_unanswered_stopped_is_given_up_on() {
    let opts = announcing_daemon(&["Announce Sent"]).await;
    let mut c = DelugeClient::connect(&opts, &pin()).await.unwrap();
    let waiting = c
        .wait_until_announced(
            &["aa", "bb"],
            Duration::from_millis(100),
            Duration::from_millis(500),
        )
        .await
        .unwrap();
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].id, "aa");
}

#[tokio::test]
async fn a_timeout_breaks_the_connection() {
    let (opts, _) = fake_daemon(Box::new(|_, _, _| vec![])).await;
    let mut c = DelugeClient::connect(&opts, &pin()).await.unwrap();
    assert!(matches!(
        c.deluge_version().await,
        Err(RpcError::Timeout { .. })
    ));
    assert!(matches!(c.deluge_version().await, Err(RpcError::Broken)));
}

#[tokio::test]
async fn a_reply_to_another_request_is_a_protocol_error() {
    let (opts, _) = fake_daemon(Box::new(|id, _, _| vec![response(id + 5, Value::None)])).await;
    let mut c = DelugeClient::connect(&opts, &pin()).await.unwrap();
    assert!(matches!(
        c.deluge_version().await,
        Err(RpcError::Protocol(_))
    ));
}

#[tokio::test]
async fn refuses_a_daemon_with_another_certificate() {
    let (opts, _) = fake_daemon(Box::new(|_, _, _| vec![])).await;
    let other = PinnedCert::from_pem_file(&fixture("b-daemon.cert")).unwrap();
    let e = DelugeClient::connect(&opts, &other).await.unwrap_err();
    assert!(matches!(e, RpcError::Tls { .. }), "{e}");
    assert!(e.to_string().contains("ssl/daemon.cert"), "{e}");
}

#[tokio::test]
async fn connect_times_out() {
    // A listener that never completes the TLS handshake.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let opts = ConnectOptions {
        port: listener.local_addr().unwrap().port(),
        connect_timeout: Duration::from_millis(200),
        ..ConnectOptions::default()
    };
    let e = DelugeClient::connect(&opts, &pin()).await.unwrap_err();
    assert!(matches!(e, RpcError::ConnectTimeout(_)), "{e}");
    drop(listener);
}

#[test]
fn options_read_the_daemon_port() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        ConnectOptions::from_config_dir(dir.path()).port,
        DEFAULT_PORT
    );
    std::fs::write(
        dir.path().join("core.conf"),
        "{\"file\": 1, \"format\": 1}{\"daemon_port\": 58899}",
    )
    .unwrap();
    assert_eq!(ConnectOptions::from_config_dir(dir.path()).port, 58899);
}

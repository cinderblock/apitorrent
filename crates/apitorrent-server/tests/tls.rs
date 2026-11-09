//! Built-in TLS over real sockets on 127.0.0.1: a bring-your-own certificate served and
//! reloaded, HSTS, secure cookies, the HTTP redirect listener, the pairing endpoint, and
//! the sync WebSocket over HTTP/1.1 and HTTP/2. The client trusts only the test CA it was
//! given: nothing here skips certificate verification.

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt as _;
use http_body_util::{BodyExt as _, Full};
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::protocol::Role;

use apitorrent_auth::PasswordMode;
use apitorrent_server::{AuthMode, TlsServe, TlsServer};
use apitorrent_tls::{CertSource, CertStore};
use apitorrent_types::SyncMessage;
use apitorrent_types::tls::{TlsCertificateSource, TlsStatus};
use common::{AuthFixture, PASSWORD, core, server_config};

/// A test CA and leaves it signs.
struct Ca {
    issuer: rcgen::Issuer<'static, rcgen::KeyPair>,
    der: CertificateDer<'static>,
}

impl Ca {
    fn new() -> Self {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "apitorrent test CA");
        let cert = params.self_signed(&key).unwrap();
        let der = cert.der().clone();
        Self {
            issuer: rcgen::Issuer::new(params, key),
            der,
        }
    }

    /// A leaf for `names`: (chain PEM, key PEM, leaf DER).
    fn leaf(&self, names: &[&str]) -> (String, String, CertificateDer<'static>) {
        let key = rcgen::KeyPair::generate().unwrap();
        let params = rcgen::CertificateParams::new(
            names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>(),
        )
        .unwrap();
        let cert = params.signed_by(&key, &self.issuer).unwrap();
        (cert.pem(), key.serialize_pem(), cert.der().clone())
    }

    fn client(&self, alpn: &[&[u8]]) -> TlsConnector {
        let mut roots = RootCertStore::empty();
        roots.add(self.der.clone()).unwrap();
        let mut config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
        config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        TlsConnector::from(Arc::new(config))
    }
}

fn write_pair(dir: &Path, chain: &str, key: &str) {
    // Key first, then the chain: the watcher must cope with seeing them change separately.
    std::fs::write(dir.join("key.pem"), key).unwrap();
    std::fs::write(dir.join("cert.pem"), chain).unwrap();
}

async fn connect(
    connector: &TlsConnector,
    addr: SocketAddr,
    name: &str,
) -> std::io::Result<TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(addr).await?;
    connector
        .connect(ServerName::try_from(name.to_owned()).unwrap(), tcp)
        .await
}

fn peer_leaf(s: &TlsStream<TcpStream>) -> CertificateDer<'static> {
    s.get_ref().1.peer_certificates().unwrap()[0]
        .clone()
        .into_owned()
}

struct Resp {
    status: StatusCode,
    headers: hyper::HeaderMap,
    body: Bytes,
}

/// One HTTP/1.1 request on its own TLS connection.
async fn h1(
    connector: &TlsConnector,
    addr: SocketAddr,
    host: &str,
    req: Request<Full<Bytes>>,
) -> Resp {
    let stream = connect(connector, addr, host.split(':').next().unwrap())
        .await
        .unwrap();
    let (mut send, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let resp = send.send_request(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    Resp {
        status: parts.status,
        headers: parts.headers,
        body: body.collect().await.unwrap().to_bytes(),
    }
}

fn get(host: &str, path: &str, token: Option<&str>) -> Request<Full<Bytes>> {
    let mut b = Request::get(path).header("host", host);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Full::default()).unwrap()
}

struct Running {
    addr: SocketAddr,
    redirect: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<(), apitorrent_server::ServeError>>,
}

async fn start(core: &apitorrent_core::Core, auth: AuthMode, store: CertStore) -> Running {
    let tls = TlsServe {
        store,
        acme: None,
        http_redirect: Some("127.0.0.1:0".parse().unwrap()),
        https_port: 8443,
    };
    let server = TlsServer::bind(core.clone(), server_config(auth), tls)
        .await
        .unwrap();
    let addr = server.local_addr().unwrap();
    let redirect = server.redirect_addr().unwrap().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.run(async {
        let _ = stopped.await;
    }));
    Running {
        addr,
        redirect,
        stop: Some(stop),
        task,
    }
}

impl Running {
    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.task.await.unwrap().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bring_your_own_certificate_is_served_and_reloaded() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let token = f.admin_token();
    let ca = Ca::new();
    let certs = root.path().join("certs");
    std::fs::create_dir_all(&certs).unwrap();
    let (chain1, key1, leaf1) = ca.leaf(&["localhost", "torrents.example.com"]);
    write_pair(&certs, &chain1, &key1);

    let store = CertStore::new();
    store.install(
        apitorrent_tls::load_files(&certs.join("cert.pem"), &certs.join("key.pem")).unwrap(),
        CertSource::Provided,
    );
    let watcher = apitorrent_tls::spawn_file_watcher(
        store.clone(),
        certs.join("cert.pem"),
        certs.join("key.pem"),
        Duration::from_millis(100),
    );
    let server = start(&core, AuthMode::Enabled(f.auth.clone()), store.clone()).await;
    let client = ca.client(&[b"http/1.1"]);

    // The provided certificate is served, and verifies against the CA.
    let s = connect(&client, server.addr, "localhost").await.unwrap();
    assert_eq!(peer_leaf(&s), leaf1);
    drop(s);

    // The pairing endpoint describes it (and needs a credential).
    let r = h1(
        &client,
        server.addr,
        "localhost",
        get("localhost", "/api/v1/tls", None),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = h1(
        &client,
        server.addr,
        "localhost",
        get("localhost", "/api/v1/tls", Some(&token)),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let status: TlsStatus = serde_json::from_slice(&r.body).unwrap();
    let cert = status.certificate.unwrap();
    assert!(status.enabled);
    assert_eq!(cert.source, TlsCertificateSource::Provided);
    let info1 = apitorrent_tls::CertInfo::from_der(&leaf1).unwrap();
    assert_eq!(cert.pairing_code, info1.pairing_code());
    assert_eq!(cert.spki_sha256, info1.spki_sha256_hex());
    // No HSTS for localhost (it would cover every local port).
    assert!(r.headers.get("strict-transport-security").is_none());

    // HSTS for a real host name.
    let r = h1(
        &client,
        server.addr,
        "torrents.example.com",
        get("torrents.example.com", "/api/v1/info", None),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        r.headers.get("strict-transport-security").unwrap(),
        "max-age=31536000"
    );

    // Signing in over TLS sets the Secure, __Host- cookie.
    let login = Request::post("/api/v1/auth/login/password")
        .header("host", "localhost")
        .header(
            "origin",
            format!("https://localhost:{}", server.addr.port()),
        )
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(
            serde_json::json!({"username": "admin", "password": PASSWORD}).to_string(),
        )))
        .unwrap();
    // The Origin must match the Host exactly, port included.
    let host = format!("localhost:{}", server.addr.port());
    let login = {
        let (mut parts, body) = login.into_parts();
        parts.headers.insert("host", host.parse().unwrap());
        Request::from_parts(parts, body)
    };
    let r = h1(&client, server.addr, &host, login).await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    let cookie = r.headers.get("set-cookie").unwrap().to_str().unwrap();
    assert!(cookie.starts_with("__Host-apitorrent-session="), "{cookie}");
    assert!(cookie.contains("; Secure"), "{cookie}");

    // Replace the certificate: new connections get the new one.
    let (chain2, key2, leaf2) = ca.leaf(&["localhost", "torrents.example.com"]);
    write_pair(&certs, &chain2, &key2);
    let mut served = None;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let s = connect(&client, server.addr, "localhost").await.unwrap();
        if peer_leaf(&s) == leaf2 {
            served = Some(());
            break;
        }
    }
    assert!(served.is_some(), "the new certificate was never served");
    assert_eq!(
        store.current().unwrap().info.spki_sha256,
        apitorrent_tls::CertInfo::from_der(&leaf2)
            .unwrap()
            .spki_sha256
    );

    // A broken replacement is refused; the last good certificate keeps serving.
    std::fs::write(
        certs.join("cert.pem"),
        "-----BEGIN CERTIFICATE-----\ngarbage\n",
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(1800)).await;
    let s = connect(&client, server.addr, "localhost").await.unwrap();
    assert_eq!(peer_leaf(&s), leaf2);
    drop(s);
    // So is a certificate whose key doesn't match.
    let (chain3, _, _) = ca.leaf(&["localhost"]);
    std::fs::write(certs.join("cert.pem"), &chain3).unwrap();
    tokio::time::sleep(Duration::from_millis(1800)).await;
    let s = connect(&client, server.addr, "localhost").await.unwrap();
    assert_eq!(peer_leaf(&s), leaf2);
    drop(s);

    // A client that doesn't trust the CA is refused: verification is never skipped.
    let other = Ca::new().client(&[b"http/1.1"]);
    assert!(connect(&other, server.addr, "localhost").await.is_err());

    watcher.abort();
    server.stop().await;
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_listener_redirects_and_answers_nothing_else() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let ca = Ca::new();
    let (chain, key, _) = ca.leaf(&["localhost"]);
    let store = CertStore::new();
    store.install(
        apitorrent_tls::load_pem(chain.as_bytes(), key.as_bytes()).unwrap(),
        CertSource::Provided,
    );
    let server = start(&core, AuthMode::Enabled(f.auth.clone()), store).await;

    let redirect = server.redirect;
    let plain = |host: &'static str, path: &'static str| async move {
        let tcp = TcpStream::connect(redirect).await.unwrap();
        let (mut send, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp))
            .await
            .unwrap();
        tokio::spawn(conn);
        send.send_request(get(host, path, None)).await.unwrap()
    };

    let r = plain("torrents.example.com", "/torrents?x=1").await;
    assert_eq!(r.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(
        r.headers().get("location").unwrap(),
        "https://torrents.example.com:8443/torrents?x=1"
    );
    // Never an open redirect.
    let r = plain("evil.example.net", "/").await;
    assert_eq!(r.status(), StatusCode::MISDIRECTED_REQUEST);
    // No pending HTTP-01 challenge: nothing to answer.
    let r = plain("torrents.example.com", "/.well-known/acme-challenge/abc").await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);

    server.stop().await;
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
async fn sync_websocket_over_http1_and_http2() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let token = f.admin_token();
    let ca = Ca::new();
    let (chain, key, _) = ca.leaf(&["localhost"]);
    let store = CertStore::new();
    store.install(
        apitorrent_tls::load_pem(chain.as_bytes(), key.as_bytes()).unwrap(),
        CertSource::Provided,
    );
    let server = start(&core, AuthMode::Enabled(f.auth.clone()), store).await;

    // HTTP/1.1 Upgrade.
    let stream = connect(&ca.client(&[b"http/1.1"]), server.addr, "localhost")
        .await
        .unwrap();
    assert_eq!(stream.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
    let mut req = format!("wss://localhost:{}/api/v1/sync", server.addr.port())
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::client_async(req, stream).await.unwrap();
    assert!(matches!(
        next_sync(&mut ws).await,
        SyncMessage::Snapshot { .. }
    ));
    drop(ws);

    // HTTP/2: ALPN picks h2, ordinary requests work, and the socket opens with extended
    // CONNECT (RFC 8441).
    let stream = connect(&ca.client(&[b"h2", b"http/1.1"]), server.addr, "localhost")
        .await
        .unwrap();
    assert_eq!(stream.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
    let (mut send, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
            .await
            .unwrap();
    tokio::spawn(conn);
    let authority = format!("https://localhost:{}", server.addr.port());
    let r = send
        .send_request(
            Request::get(format!("{authority}/api/v1/info"))
                .body(Full::<Bytes>::default())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.version(), hyper::Version::HTTP_2);

    let mut connect_req = Request::builder()
        .method(Method::CONNECT)
        .uri(format!("{authority}/api/v1/sync"))
        .header("sec-websocket-version", "13")
        .header("authorization", format!("Bearer {token}"))
        .body(Full::<Bytes>::default())
        .unwrap();
    connect_req
        .extensions_mut()
        .insert(hyper::ext::Protocol::from_static("websocket"));
    let resp = send.send_request(connect_req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let upgraded = hyper::upgrade::on(resp).await.unwrap();
    let mut ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
        TokioIo::new(upgraded),
        Role::Client,
        None,
    )
    .await;
    assert!(matches!(
        next_sync(&mut ws).await,
        SyncMessage::Snapshot { .. }
    ));

    // Without a credential, the h2 socket is refused like any other request.
    let mut anon = Request::builder()
        .method(Method::CONNECT)
        .uri(format!("{authority}/api/v1/sync"))
        .header("sec-websocket-version", "13")
        .body(Full::<Bytes>::default())
        .unwrap();
    anon.extensions_mut()
        .insert(hyper::ext::Protocol::from_static("websocket"));
    let resp = send.send_request(anon).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    drop(ws);
    server.stop().await;
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn self_signed_certificate_pins_by_pairing_code() {
    // What a pairing client does: compute the code from the presented certificate and
    // compare it with the one the daemon printed, then trust exactly that key.
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let dir = root.path().join("tls");
    let loaded = apitorrent_tls::ensure_self_signed(
        &dir,
        &["localhost".to_owned(), "127.0.0.1".to_owned()],
        std::time::SystemTime::now(),
    )
    .unwrap();
    let printed = loaded.info.pairing_code();
    let store = CertStore::new();
    store.install(loaded, CertSource::SelfSigned);
    let server = start(&core, AuthMode::Enabled(f.auth.clone()), store).await;

    // A pinning client trusts the self-signed certificate as its own root.
    let presented_der = std::fs::read(dir.join("cert.pem")).unwrap();
    let presented = apitorrent_tls::read_pem_certificates(&presented_der).unwrap();
    let info = apitorrent_tls::CertInfo::from_der(&presented[0]).unwrap();
    assert_eq!(info.pairing_code(), printed);
    let mut roots = RootCertStore::empty();
    roots.add(presented[0].clone()).unwrap();
    let config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    let s = connect(
        &TlsConnector::from(Arc::new(config)),
        server.addr,
        "localhost",
    )
    .await
    .unwrap();
    assert_eq!(peer_leaf(&s), presented[0]);
    drop(s);

    server.stop().await;
    core.shutdown().await.unwrap();
}

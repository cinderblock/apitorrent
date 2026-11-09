//! The client against real TLS servers on 127.0.0.1: pinned keys, private CAs and the OS
//! verifier, for both HTTP requests and the sync WebSocket.

// The handshake callback's error type is tungstenite's, not ours.
#![allow(clippy::result_large_err)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt as _, StreamExt as _};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

use apitorrent_client::{ClientError, Remote, SpkiHash, Token, Trust, probe};

const TOKEN: &str = "apt_test_token";

struct Identity {
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    /// The pin of the leaf's key.
    pin: SpkiHash,
    /// The CA's certificate in PEM, when the leaf was issued by one.
    ca_pem: Option<String>,
}

fn self_signed() -> Identity {
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".into()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    Identity {
        pin: SpkiHash::of_certificate(cert.der()).unwrap(),
        chain: vec![cert.der().clone()],
        key: PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
        ca_pem: None,
    }
}

fn ca_issued() -> Identity {
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "test CA");
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let issuer = rcgen::Issuer::new(ca_params, ca_key);
    let key = rcgen::KeyPair::generate().unwrap();
    let leaf = rcgen::CertificateParams::new(vec!["localhost".into()])
        .unwrap()
        .signed_by(&key, &issuer)
        .unwrap();
    Identity {
        pin: SpkiHash::of_certificate(leaf.der()).unwrap(),
        chain: vec![leaf.der().clone()],
        key: PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
        ca_pem: Some(ca.pem()),
    }
}

/// What the server saw: the request head of each HTTP request.
type Seen = Arc<Mutex<Vec<String>>>;

/// A TLS server that answers HTTP requests with `{"ok":true}`, and WebSocket upgrades to
/// `/api/v1/sync` (with the right token) with one text message.
async fn serve(id: Identity) -> (SocketAddr, Seen) {
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(id.chain, id.key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Seen = Arc::default();
    let seen2 = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            let seen = seen2.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                // Peek at the request head without consuming it from the WebSocket's view:
                // read it, then decide. WebSocket requests are served on a fresh path.
                let mut head = Vec::new();
                let mut buf = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if tls.read(&mut buf).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(buf[0]);
                }
                let text = String::from_utf8_lossy(&head).into_owned();
                seen.lock().unwrap().push(text.clone());
                if text.to_ascii_lowercase().contains("upgrade: websocket") {
                    // Replay the head to tungstenite through a chained reader.
                    let stream = Replay {
                        head,
                        pos: 0,
                        inner: tls,
                    };
                    let check = |req: &Request, resp: Response| {
                        let ok = req
                            .headers()
                            .get("authorization")
                            .is_some_and(|v| v == &format!("Bearer {TOKEN}"));
                        if ok {
                            Ok(resp)
                        } else {
                            let mut r = tokio_tungstenite::tungstenite::http::Response::new(Some(
                                "no token".to_owned(),
                            ));
                            *r.status_mut() =
                                tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED;
                            Err(r)
                        }
                    };
                    if let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, check).await {
                        let _ = ws.send(Message::text("hello")).await;
                        let _ = ws.close(None).await;
                    }
                    return;
                }
                let body = br#"{"ok":true}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = tls.write_all(resp.as_bytes()).await;
                let _ = tls.write_all(body).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    (addr, seen)
}

/// A stream that first yields bytes already read from `inner`.
struct Replay<S> {
    head: Vec<u8>,
    pos: usize,
    inner: S,
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Replay<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.pos < self.head.len() {
            let n = (self.head.len() - self.pos).min(buf.remaining());
            let start = self.pos;
            buf.put_slice(&self.head[start..start + n]);
            self.pos += n;
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Replay<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

fn url(addr: SocketAddr) -> String {
    format!("https://localhost:{}", addr.port())
}

#[tokio::test]
async fn a_pinned_key_is_accepted_for_http_and_sync() {
    let id = self_signed();
    let pin = id.pin;
    let (addr, seen) = serve(id).await;
    let remote = Remote::new(
        &url(addr),
        &Trust::Pinned { spki_sha256: pin },
        Some(Token::new(TOKEN)),
    )
    .unwrap();

    let resp = remote
        .request("GET", "/api/v1/info", None, None)
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, br#"{"ok":true}"#);
    let head = seen.lock().unwrap()[0].to_ascii_lowercase();
    assert!(head.starts_with("get /api/v1/info http/1.1"), "{head}");
    assert!(
        head.contains(&format!("authorization: bearer {TOKEN}")),
        "{head}"
    );

    let mut socket = remote.sync().await.unwrap();
    let msg = socket.next().await.unwrap().unwrap();
    assert_eq!(msg.into_text().unwrap().as_str(), "hello");
}

#[tokio::test]
async fn sync_without_the_token_reports_the_status() {
    let id = self_signed();
    let pin = id.pin;
    let (addr, _) = serve(id).await;
    let remote = Remote::new(&url(addr), &Trust::Pinned { spki_sha256: pin }, None).unwrap();
    match remote.sync().await {
        Err(ClientError::Status { status: 401, .. }) => {}
        other => panic!("expected a 401, got {other:?}"),
    }
}

#[tokio::test]
async fn a_different_key_is_refused_and_reported() {
    let id = self_signed();
    let actual = id.pin;
    let (addr, seen) = serve(id).await;
    let wrong = SpkiHash([7; 32]);
    let remote = Remote::new(
        &url(addr),
        &Trust::Pinned { spki_sha256: wrong },
        Some(Token::new(TOKEN)),
    )
    .unwrap();
    match remote.request("GET", "/api/v1/info", None, None).await {
        Err(ClientError::PinMismatch(m)) => {
            assert_eq!(m.presented, actual);
            assert!(
                m.chain_error.is_none(),
                "a pin is strict: nothing else is tried"
            );
        }
        other => panic!("expected a pin mismatch, got {other:?}"),
    }
    match remote.sync().await {
        Err(ClientError::PinMismatch(m)) => assert_eq!(m.presented, actual),
        other => panic!("expected a pin mismatch, got {other:?}"),
    }
    // The token never left: no request reached the server.
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_private_ca_is_trusted_only_for_its_own_certificates() {
    let id = ca_issued();
    let ca_pem = id.ca_pem.clone().unwrap();
    let (addr, _) = serve(id).await;
    let trusted = Remote::new(
        &url(addr),
        &Trust::PrivateCa {
            ca_pem: ca_pem.clone(),
        },
        None,
    )
    .unwrap();
    assert_eq!(
        trusted
            .request("GET", "/api/v1/info", None, None)
            .await
            .unwrap()
            .status,
        200
    );

    // Another CA's certificate isn't trusted through this profile's CA.
    let other = ca_issued();
    let (other_addr, seen) = serve(other).await;
    let refused = Remote::new(&url(other_addr), &Trust::PrivateCa { ca_pem }, None).unwrap();
    assert!(matches!(
        refused.request("GET", "/api/v1/info", None, None).await,
        Err(ClientError::Connect(_))
    ));
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn os_trust_refuses_a_self_signed_server() {
    let (addr, seen) = serve(self_signed()).await;
    let remote = Remote::new(&url(addr), &Trust::Os, Some(Token::new(TOKEN))).unwrap();
    assert!(matches!(
        remote.request("GET", "/api/v1/info", None, None).await,
        Err(ClientError::Connect(_))
    ));
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn probing_shows_the_key_without_trusting_it() {
    let id = self_signed();
    let pin = id.pin;
    let (addr, seen) = serve(id).await;
    let p = probe(&url(addr), &Trust::Os).await.unwrap();
    assert_eq!(p.presented, Some(pin));
    assert!(p.verdict.is_err());
    let p = probe(&url(addr), &Trust::Pinned { spki_sha256: pin })
        .await
        .unwrap();
    assert_eq!(p.presented, Some(pin));
    assert!(p.verdict.is_ok());
    assert!(seen.lock().unwrap().is_empty(), "a probe sends no request");
}

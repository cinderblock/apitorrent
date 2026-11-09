//! Built-in HTTPS: the TLS listener, HSTS, the pairing endpoint, and the plain-HTTP
//! listener that answers ACME HTTP-01 challenges and redirects everything else.
//!
//! Certificates come from an [`apitorrent_tls::CertStore`], which the daemon fills from
//! bring-your-own files, a self-signed certificate or ACME. The store resolves the
//! certificate per handshake, so renewals and reloads apply to new connections at once.
//!
//! Handshakes run in their own tasks, off the accept loop, so a slow or stalled client
//! can't hold up others; at most [`MAX_HANDSHAKES`] run at once and each has
//! [`HANDSHAKE_TIMEOUT`]. ALPN offers `h2` and `http/1.1`. The sync WebSocket works over
//! both: HTTP/1.1 `Upgrade`, or HTTP/2 extended CONNECT (RFC 8441), which axum enables.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::Json;
use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::serve::ListenerExt as _;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc, watch};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

use apitorrent_core::Core;
use apitorrent_tls::acme::AcmeStatus;
use apitorrent_tls::{ACME_TLS_ALPN, CertSource, CertStore};
use apitorrent_types::tls::{AcmeState, TlsCertificate, TlsCertificateSource, TlsStatus};

use crate::guard::{Guard, strip_port};
use crate::routes::error;
use crate::{AppState, ServeError, ServerConfig};

/// Most TLS handshakes in progress at once; connections beyond it are closed at once.
pub const MAX_HANDSHAKES: usize = 512;
/// A client that hasn't finished its handshake by then is dropped.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// `Strict-Transport-Security` on HTTPS responses: one year, this host only.
const HSTS: &str = "max-age=31536000";

/// How the API serves HTTPS.
#[derive(Debug, Clone)]
pub struct TlsServe {
    /// The certificates to present.
    pub store: CertStore,
    /// ACME progress, shown by `GET /api/v1/tls`, when ACME supplies the certificate.
    pub acme: Option<watch::Receiver<AcmeStatus>>,
    /// Also listen for plain HTTP here: ACME HTTP-01 answers, and a permanent redirect to
    /// HTTPS for everything else.
    pub http_redirect: Option<SocketAddr>,
    /// The HTTPS port browsers use, for redirects. The listen port, unless a port forward
    /// maps another one (443) to it.
    pub https_port: u16,
}

/// What request handlers need to know about TLS.
#[derive(Debug, Clone)]
pub(crate) struct TlsCtx {
    store: CertStore,
    acme: Option<watch::Receiver<AcmeStatus>>,
}

/// A bound HTTPS server, not yet running. [`serve_tls`] is `bind` then `run`; binding
/// separately lets a caller learn the ports first (tests bind port 0).
#[derive(Debug)]
pub struct TlsServer {
    core: Core,
    config: ServerConfig,
    tls: TlsServe,
    listener: TcpListener,
    redirect: Option<TcpListener>,
}

/// Serve the API over HTTPS until `shutdown` resolves.
pub async fn serve_tls(
    core: Core,
    config: ServerConfig,
    tls: TlsServe,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServeError> {
    TlsServer::bind(core, config, tls)
        .await?
        .run(shutdown)
        .await
}

async fn bind(addr: SocketAddr) -> Result<TcpListener, ServeError> {
    match TcpListener::bind(addr).await {
        Ok(l) => Ok(l),
        Err(e) if cfg!(windows) && e.kind() == std::io::ErrorKind::PermissionDenied => {
            Err(ServeError::Reserved(addr))
        }
        Err(e) => Err(e.into()),
    }
}

impl TlsServer {
    /// Check the listen rules and bind the listeners.
    pub async fn bind(core: Core, config: ServerConfig, tls: TlsServe) -> Result<Self, ServeError> {
        crate::check_listen(&config, true)?;
        let listener = bind(config.listen).await?;
        let redirect = match tls.http_redirect {
            Some(addr) => Some(bind(addr).await?),
            None => None,
        };
        Ok(Self {
            core,
            config,
            tls,
            listener,
            redirect,
        })
    }

    /// The HTTPS listener's address.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// The plain-HTTP listener's address, if there is one.
    pub fn redirect_addr(&self) -> Option<std::io::Result<SocketAddr>> {
        self.redirect.as_ref().map(TcpListener::local_addr)
    }

    /// Serve until `shutdown` resolves, then let open requests finish.
    pub async fn run(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), ServeError> {
        let Self {
            core,
            config,
            tls,
            listener,
            redirect,
        } = self;
        let server_config = tls
            .store
            .server_config()
            .map_err(|e| ServeError::Tls(e.to_string()))?;
        let local = listener.local_addr()?;
        tracing::info!(addr = %local, "API listening (HTTPS)");
        if let Some(cert) = tls.store.current() {
            log_certificate(&cert.info, cert.source);
        } else {
            tracing::warn!("no TLS certificate yet; HTTPS handshakes fail until ACME gets one");
        }

        let (stop_tx, stop_rx) = watch::channel(false);
        tokio::spawn(async move {
            shutdown.await;
            let _ = stop_tx.send(true);
        });
        let stopped = |mut rx: watch::Receiver<bool>| async move {
            let _ = rx.wait_for(|s| *s).await;
        };

        let redirect_task = match redirect {
            Some(l) => {
                tracing::info!(addr = %l.local_addr()?, "HTTP listening (ACME challenges and redirects to HTTPS)");
                let app = redirect_router(
                    tls.store.clone(),
                    Guard::new(&config.public_hosts),
                    tls.https_port,
                );
                let stop = stopped(stop_rx.clone());
                Some(tokio::spawn(async move {
                    axum::serve(l, app).with_graceful_shutdown(stop).await
                }))
            }
            None => None,
        };

        let ctx = TlsCtx {
            store: tls.store.clone(),
            acme: tls.acme.clone(),
        };
        let app = crate::build_router(core, &config, Some(ctx));
        let result = serve_https(listener, server_config, app, stopped(stop_rx)).await;
        if let Some(t) = redirect_task {
            match t.await {
                Ok(Err(e)) => tracing::warn!(error = %e, "HTTP redirect listener failed"),
                Err(e) => tracing::warn!(error = %e, "HTTP redirect listener panicked"),
                Ok(Ok(())) => {}
            }
        }
        result?;
        Ok(())
    }
}

/// Serve `app` over HTTPS on `listener` until `stop` resolves: handshakes off the accept
/// loop, bounded and timed (see [`accept_loop`]), then axum with the peer's address.
pub(crate) async fn serve_https(
    listener: TcpListener,
    server_config: Arc<tokio_rustls::rustls::ServerConfig>,
    app: Router,
    stop: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let local = listener.local_addr()?;
    let (tx, rx) = mpsc::channel(64);
    let accept = tokio::spawn(accept_loop(listener, TlsAcceptor::from(server_config), tx));
    // `ConnectInfo<SocketAddr>` is implemented for `TapIo<L, _>` but can't be for our own
    // listener type (orphan rules), so wrap it in a no-op tap.
    let incoming = TlsListener { rx, local }.tap_io(|_| {});
    let result = axum::serve(
        incoming,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(stop)
    .await;
    accept.abort();
    result
}

impl TlsServe {
    /// The TLS context request handlers see, for another listener sharing these
    /// certificates.
    pub(crate) fn ctx(&self) -> TlsCtx {
        TlsCtx {
            store: self.store.clone(),
            acme: self.acme.clone(),
        }
    }
}

fn log_certificate(info: &apitorrent_tls::CertInfo, source: CertSource) {
    tracing::info!(
        source = ?source,
        names = ?info.dns_names,
        ips = ?info.ip_addresses,
        pairing_code = %info.pairing_code(),
        spki_sha256 = %info.spki_sha256_hex(),
        "serving TLS certificate"
    );
}

/// Accept TCP connections and run each TLS handshake in its own task.
async fn accept_loop(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    tx: mpsc::Sender<(TlsStream<TcpStream>, SocketAddr)>,
) {
    let handshakes = Arc::new(Semaphore::new(MAX_HANDSHAKES));
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                use std::io::ErrorKind::{ConnectionAborted, ConnectionRefused, ConnectionReset};
                if !matches!(
                    e.kind(),
                    ConnectionAborted | ConnectionRefused | ConnectionReset
                ) {
                    // Out of file descriptors and the like: back off instead of spinning.
                    tracing::warn!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                continue;
            }
        };
        let Ok(permit) = handshakes.clone().try_acquire_owned() else {
            tracing::debug!(%peer, "too many TLS handshakes in progress; dropping a connection");
            continue;
        };
        let acceptor = acceptor.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let _ = tcp.set_nodelay(true);
            let stream = match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    tracing::debug!(%peer, error = %e, "TLS handshake failed");
                    return;
                }
                Err(_) => {
                    tracing::debug!(%peer, "TLS handshake timed out");
                    return;
                }
            };
            drop(permit);
            // A TLS-ALPN-01 validation is complete once the handshake is: it never speaks
            // HTTP (RFC 8737 §3).
            if stream.get_ref().1.alpn_protocol() == Some(ACME_TLS_ALPN) {
                tracing::debug!(%peer, "answered an ACME TLS-ALPN-01 validation");
                return;
            }
            let _ = tx.send((stream, peer)).await;
        });
    }
}

/// Hands completed TLS connections to axum.
struct TlsListener {
    rx: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
    local: SocketAddr,
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.rx.recv().await {
            Some(conn) => conn,
            // The accept loop is gone (shutting down): accept nothing more.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local)
    }
}

/// The request's host, from `Host` (HTTP/1.1) or the URI authority (HTTP/2).
fn request_host(req: &Request) -> Option<&str> {
    req.headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            req.uri()
                .authority()
                .map(axum::http::uri::Authority::as_str)
        })
}

/// Add `Strict-Transport-Security`, except for loopback names and IP addresses. HSTS covers
/// every port of a host, so on `localhost` it would force the user's other local servers
/// to HTTPS; browsers ignore it for IP addresses anyway.
pub(crate) async fn hsts(req: Request, next: Next) -> Response {
    let send = request_host(&req).is_some_and(|h| {
        let host = strip_port(h).trim_start_matches('[').trim_end_matches(']');
        host.parse::<std::net::IpAddr>().is_err() && !crate::authn::is_loopback_host(host)
    });
    let mut resp = next.run(req).await;
    if send {
        resp.headers_mut().insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static(HSTS),
        );
    }
    resp
}

fn unix_ms(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// `GET /api/v1/tls`: the served certificate's pairing code and validity, and ACME progress.
pub(crate) async fn status(State(s): State<AppState>) -> Json<TlsStatus> {
    let Some(tls) = &s.tls else {
        return Json(TlsStatus {
            enabled: false,
            certificate: None,
            acme: None,
        });
    };
    let certificate = tls.store.current().map(|c| TlsCertificate {
        source: match c.source {
            CertSource::Provided => TlsCertificateSource::Provided,
            CertSource::SelfSigned => TlsCertificateSource::SelfSigned,
            CertSource::Acme => TlsCertificateSource::Acme,
        },
        spki_sha256: c.info.spki_sha256_hex(),
        pairing_code: c.info.pairing_code(),
        not_before_ms: unix_ms(c.info.not_before),
        not_after_ms: unix_ms(c.info.not_after),
        dns_names: c.info.dns_names.clone(),
        ip_addresses: c
            .info
            .ip_addresses
            .iter()
            .map(ToString::to_string)
            .collect(),
    });
    let acme = tls.acme.as_ref().map(|rx| {
        let a = rx.borrow();
        AcmeState {
            domains: a.domains.clone(),
            directory_url: a.directory_url.clone(),
            next_attempt_ms: a.next_attempt_ms,
            last_issued_ms: a.last_issued_ms,
            last_error: a.last_error.clone(),
            consecutive_failures: a.consecutive_failures,
        }
    });
    Json(TlsStatus {
        enabled: true,
        certificate,
        acme,
    })
}

#[derive(Debug, Clone)]
struct RedirectState {
    store: CertStore,
    guard: Guard,
    https_port: u16,
}

/// The plain-HTTP side: ACME HTTP-01 answers, and a redirect to HTTPS for anything else.
fn redirect_router(store: CertStore, guard: Guard, https_port: u16) -> Router {
    Router::new()
        .route("/.well-known/acme-challenge/{token}", get(http01))
        .fallback(redirect)
        .with_state(RedirectState {
            store,
            guard,
            https_port,
        })
}

async fn http01(State(s): State<RedirectState>, Path(token): Path<String>) -> Response {
    match s.store.http01_response(&token) {
        Some(key_authorization) => (
            [(header::CONTENT_TYPE, "application/octet-stream")],
            key_authorization,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn redirect(State(s): State<RedirectState>, req: Request) -> Response {
    // Only redirect to names this server answers to: never an open redirect.
    let Some(authority) = request_host(&req).filter(|h| s.guard.allows_authority(h)) else {
        return error(
            StatusCode::MISDIRECTED_REQUEST,
            "unknown_host",
            "this server does not answer to that host name",
        )
        .into_response();
    };
    let host = strip_port(&authority.to_ascii_lowercase()).to_owned();
    let port = if s.https_port == 443 {
        String::new()
    } else {
        format!(":{}", s.https_port)
    };
    let path = req
        .uri()
        .path_and_query()
        .map_or("/", axum::http::uri::PathAndQuery::as_str);
    let location = format!("https://{host}{port}{path}");
    match HeaderValue::from_str(&location) {
        Ok(v) => (StatusCode::PERMANENT_REDIRECT, [(header::LOCATION, v)]).into_response(),
        Err(_) => StatusCode::BAD_REQUEST.into_response(),
    }
}

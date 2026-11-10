//! Serving the API to an app in the same process, without a socket.
//!
//! The desktop app runs the core in-process (Local mode) and its UI talks to this server
//! through [`Embedded`]: single requests go straight through the router, and the sync
//! WebSocket runs over an in-memory HTTP connection from [`Embedded::connect`]. Nothing
//! listens on a port, so no other program or user on the machine can reach it.
//!
//! Requests that come through here carry an [`InProcess`] marker, a Rust type that no
//! network request can carry, and authenticate as the machine's owner (see
//! `authn::layer`). A LAN listener started next to it with [`crate::serve`] keeps the
//! normal authentication rules.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderValue, Method, header};
use tokio::io::DuplexStream;
use tokio::sync::{mpsc, oneshot};
use tower::ServiceExt as _;

use apitorrent_core::Core;

use crate::{AuthMode, ServerConfig, TrustedProxies, WebUi, router};

/// Set on requests that came through [`Embedded`]. Only this module creates it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct InProcess;

/// The `Host` in-process requests use. It is a loopback name, so the guard accepts it.
pub const EMBEDDED_HOST: &str = "localhost";

/// Bytes buffered in each direction of an in-memory connection.
const PIPE_BUFFER: usize = 256 * 1024;

/// A response from the embedded server.
#[derive(Debug, Clone)]
pub struct EmbeddedResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum EmbeddedError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("the embedded server has stopped")]
    Stopped,
}

/// The API, served in-process. Cheap to clone; the server stops when the last clone drops.
#[derive(Clone, Debug)]
pub struct Embedded {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    app: Router,
    connections: mpsc::UnboundedSender<DuplexStream>,
    _stop: oneshot::Sender<()>,
}

/// Hands in-memory connections to `axum::serve`.
struct ChannelListener(mpsc::UnboundedReceiver<DuplexStream>);

impl axum::serve::Listener for ChannelListener {
    type Io = DuplexStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.0.recv().await {
            Some(io) => (io, loopback()),
            // No more connections can arrive; graceful shutdown ends the server.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(loopback())
    }
}

fn loopback() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

/// Mark a request as in-process. The peer address is loopback, for the audit log.
async fn mark(mut req: Request) -> Request {
    req.extensions_mut().insert(InProcess);
    req.extensions_mut().insert(ConnectInfo(loopback()));
    req
}

impl Embedded {
    /// Serve the API for `core` in-process. `auth` is the same authentication a LAN
    /// listener uses (if any); in-process callers are the owner either way.
    pub fn new(core: Core, auth: AuthMode) -> Self {
        let config = ServerConfig {
            listen: loopback(),
            public_hosts: Vec::new(),
            web_ui: WebUi::None,
            auth,
            trusted_proxies: TrustedProxies::default(),
            allow_insecure_lan: false,
            plugins: None,
            updates: None,
            space_hosts: crate::SpaceHosts::default(),
        };
        let app = router(core, &config).layer(axum::middleware::map_request(mark));
        let (connections, rx) = mpsc::unbounded_channel();
        let (stop, stopped) = oneshot::channel::<()>();
        let served = app.clone();
        tokio::spawn(async move {
            let shutdown = async move {
                let _ = stopped.await;
            };
            if let Err(e) = axum::serve(ChannelListener(rx), served)
                .with_graceful_shutdown(shutdown)
                .await
            {
                tracing::warn!(error = %e, "embedded server stopped");
            }
        });
        Self {
            inner: Arc::new(Inner {
                app,
                connections,
                _stop: stop,
            }),
        }
    }

    /// Run one request (`path` like `/api/v1/torrents?x=1`) and return its response.
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
        content_type: Option<&str>,
    ) -> Result<EmbeddedResponse, EmbeddedError> {
        let method = Method::from_bytes(method.as_bytes())
            .map_err(|_| EmbeddedError::BadRequest(format!("bad method {method}")))?;
        if !path.starts_with('/') {
            return Err(EmbeddedError::BadRequest(format!("not a path: {path}")));
        }
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, EMBEDDED_HOST);
        if let Some(ct) = content_type {
            let ct = HeaderValue::from_str(ct)
                .map_err(|_| EmbeddedError::BadRequest("bad content type".into()))?;
            req = req.header(header::CONTENT_TYPE, ct);
        }
        let req = req
            .body(body.map_or_else(Body::empty, Body::from))
            .map_err(|e| EmbeddedError::BadRequest(e.to_string()))?;
        let resp = self
            .inner
            .app
            .clone()
            .oneshot(req)
            .await
            .map_err(|_| EmbeddedError::Stopped)?;
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .map_err(|e| EmbeddedError::BadRequest(e.to_string()))?
            .to_vec();
        Ok(EmbeddedResponse {
            status,
            content_type,
            body,
        })
    }

    /// An in-memory HTTP/1.1 connection to the server, for the sync WebSocket. Speak HTTP
    /// to `http://localhost` over it (with a WebSocket client, say).
    pub fn connect(&self) -> Result<DuplexStream, EmbeddedError> {
        let (client, server) = tokio::io::duplex(PIPE_BUFFER);
        self.inner
            .connections
            .send(server)
            .map_err(|_| EmbeddedError::Stopped)?;
        Ok(client)
    }
}

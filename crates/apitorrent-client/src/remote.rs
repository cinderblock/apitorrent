//! Talking to a daemon: the `/api/v1` HTTP calls and the sync WebSocket, over TLS verified
//! by a [`Trust`] setting, authenticated with an API token.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use url::Url;

use crate::trust::{self, PinMismatch, Recorder, SpkiHash, Trust, TrustError};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Generous: a Deluge import or a big `.torrent` upload can take a while.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

const USER_AGENT: &str = concat!("apitorrent-client/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("{0}")]
    Url(String),
    #[error(transparent)]
    Trust(#[from] TrustError),
    #[error("{0}")]
    PinMismatch(PinMismatch),
    #[error("could not reach the server: {0}")]
    Connect(String),
    #[error("the server answered {status}: {body}")]
    Status { status: u16, body: String },
    #[error("WebSocket error: {0}")]
    WebSocket(String),
}

impl ClientError {
    /// Classify a transport error, surfacing a pin mismatch (with the key the server
    /// presented) when that is what it was.
    fn transport(e: &(dyn std::error::Error + 'static)) -> Self {
        if let Some(m) = trust::find_pin_mismatch(e) {
            return Self::PinMismatch(m);
        }
        Self::Connect(describe(e))
    }
}

/// An error and its sources, joined: reqwest's top-level message alone ("error sending
/// request") hides the cause.
fn describe(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(s) = cur {
        let msg = s.to_string();
        if !out.contains(&msg) {
            out.push_str(": ");
            out.push_str(&msg);
        }
        cur = s.source();
    }
    out
}

/// An API token. Never printed.
#[derive(Clone)]
pub struct Token(Arc<str>);

impl Token {
    pub fn new(token: impl Into<Arc<str>>) -> Self {
        Self(token.into())
    }

    fn header(&self) -> Result<HeaderValue, ClientError> {
        let mut v = HeaderValue::from_str(&format!("Bearer {}", self.0))
            .map_err(|_| ClientError::Url("the API token contains invalid characters".into()))?;
        v.set_sensitive(true);
        Ok(v)
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(…)")
    }
}

/// A host name that only reaches this machine.
fn is_loopback_host(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d == "localhost" || d.ends_with(".localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Check and normalise a daemon's base URL: `https://`, or `http://` to a loopback address
/// only (an SSH tunnel, say), since a token sent in plain HTTP across a network can be
/// read by anyone on the path. The path, if any, is a prefix the API lives under.
pub fn parse_base_url(input: &str) -> Result<Url, ClientError> {
    let mut url =
        Url::parse(input.trim()).map_err(|e| ClientError::Url(format!("not a valid URL: {e}")))?;
    match url.scheme() {
        "https" => {}
        "http" if is_loopback_host(&url) => {}
        "http" => {
            return Err(ClientError::Url(
                "use https:// for a server on another machine (plain http:// is allowed only \
                 for localhost, such as an SSH tunnel)"
                    .into(),
            ));
        }
        other => {
            return Err(ClientError::Url(format!(
                "unsupported scheme `{other}`; use https://"
            )));
        }
    }
    if url.host().is_none() {
        return Err(ClientError::Url("the URL has no host".into()));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ClientError::Url(
            "don't put credentials in the URL; use an API token".into(),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(ClientError::Url(
            "the URL can't have a query or fragment".into(),
        ));
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

/// Anything a WebSocket can run over: TLS, plain TCP, or an in-memory pipe.
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

impl fmt::Debug for dyn Io {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Io")
    }
}

/// The sync WebSocket.
pub type SyncSocket = WebSocketStream<Box<dyn Io>>;

/// An HTTP response, passed on as it came.
#[derive(Debug, Clone)]
pub struct RawResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

/// A connection profile's view of one daemon.
#[derive(Clone)]
pub struct Remote {
    base: Url,
    token: Option<Token>,
    tls: Arc<ClientConfig>,
    http: reqwest::Client,
}

impl fmt::Debug for Remote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Remote")
            .field("base", &self.base.as_str())
            .field("token", &self.token)
            .finish_non_exhaustive()
    }
}

impl Remote {
    pub fn new(base_url: &str, trust: &Trust, token: Option<Token>) -> Result<Self, ClientError> {
        let base = parse_base_url(base_url)?;
        let tls = trust::client_config(trust)?;
        Self::with_config(base, tls, token)
    }

    fn with_config(
        base: Url,
        tls: Arc<ClientConfig>,
        token: Option<Token>,
    ) -> Result<Self, ClientError> {
        let http = reqwest::Client::builder()
            .use_preconfigured_tls((*tls).clone())
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .referer(false)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .map_err(|e| ClientError::Connect(describe(&e)))?;
        Ok(Self {
            base,
            token,
            tls,
            http,
        })
    }

    pub fn base_url(&self) -> &Url {
        &self.base
    }

    /// The full URL for an API path such as `/api/v1/torrents?x=1`. Only `/api/` paths are
    /// allowed, so the token is never sent anywhere else on the server.
    pub fn api_url(&self, path: &str) -> Result<Url, ClientError> {
        let rel = path
            .strip_prefix('/')
            .filter(|p| p.starts_with("api/"))
            .ok_or_else(|| ClientError::Url(format!("not an API path: {path}")))?;
        let (route, _) = rel.split_once(['?', '#']).unwrap_or((rel, ""));
        if route.split('/').any(|s| s == ".." || s == ".") || rel.contains('\\') {
            return Err(ClientError::Url(format!("not an API path: {path}")));
        }
        let url = self
            .base
            .join(rel)
            .map_err(|e| ClientError::Url(e.to_string()))?;
        if url.origin() != self.base.origin() || !url.path().starts_with(self.base.path()) {
            return Err(ClientError::Url(format!("not an API path: {path}")));
        }
        Ok(url)
    }

    /// Send one API request and return the response, whatever its status.
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
        content_type: Option<&str>,
    ) -> Result<RawResponse, ClientError> {
        let url = self.api_url(path)?;
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| ClientError::Url(format!("bad method {method}")))?;
        let mut req = self.http.request(method, url);
        if let Some(t) = &self.token {
            req = req.header(reqwest::header::AUTHORIZATION, t.header()?);
        }
        if let Some(body) = body {
            req = req.body(body);
        }
        if let Some(ct) = content_type {
            req = req.header(reqwest::header::CONTENT_TYPE, ct);
        }
        let resp = req.send().await.map_err(|e| ClientError::transport(&e))?;
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = resp
            .bytes()
            .await
            .map_err(|e| ClientError::transport(&e))?
            .to_vec();
        Ok(RawResponse {
            status,
            content_type,
            body,
        })
    }

    /// Open the sync WebSocket (`/api/v1/sync`).
    pub async fn sync(&self) -> Result<SyncSocket, ClientError> {
        let mut url = self.api_url("/api/v1/sync")?;
        let ws_scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(ws_scheme)
            .map_err(|()| ClientError::Url("could not build the WebSocket URL".into()))?;
        let io = self.connect_io().await?;
        websocket_over(io, url.as_str(), self.token.as_ref()).await
    }

    /// A TCP connection to the server, wrapped in TLS for `https`.
    async fn connect_io(&self) -> Result<Box<dyn Io>, ClientError> {
        let host = self
            .base
            .host_str()
            .ok_or_else(|| ClientError::Url("no host".into()))?;
        let port = self
            .base
            .port_or_known_default()
            .ok_or_else(|| ClientError::Url("no port".into()))?;
        // `host_str` keeps an IPv6 address's brackets; the resolver doesn't want them.
        let bare = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        let tcp = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio::net::TcpStream::connect((bare.as_str(), port)),
        )
        .await
        .map_err(|_| ClientError::Connect("timed out connecting".into()))?
        .map_err(|e| ClientError::Connect(describe(&e)))?;
        let _ = tcp.set_nodelay(true);
        if self.base.scheme() != "https" {
            return Ok(Box::new(tcp));
        }
        let name = ServerName::try_from(bare)
            .map_err(|e| ClientError::Url(format!("bad server name: {e}")))?;
        let tls = tokio_rustls::TlsConnector::from(self.tls.clone())
            .connect(name, tcp)
            .await
            .map_err(|e| ClientError::transport(&e))?;
        Ok(Box::new(tls))
    }
}

/// Run the WebSocket handshake for `url` over an existing connection. Exposed so an
/// embedded server can be reached over an in-memory pipe the same way.
pub async fn websocket_over(
    io: Box<dyn Io>,
    url: &str,
    token: Option<&Token>,
) -> Result<SyncSocket, ClientError> {
    use tokio_tungstenite::tungstenite::Error as WsError;
    let mut request = url
        .into_client_request()
        .map_err(|e| ClientError::Url(e.to_string()))?;
    if let Some(t) = token {
        request.headers_mut().insert("authorization", t.header()?);
    }
    request
        .headers_mut()
        .insert("user-agent", HeaderValue::from_static(USER_AGENT));
    match tokio_tungstenite::client_async(request, io).await {
        Ok((socket, _)) => Ok(socket),
        Err(WsError::Http(resp)) => Err(ClientError::Status {
            status: resp.status().as_u16(),
            body: resp
                .body()
                .as_deref()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default(),
        }),
        Err(e) => Err(ClientError::WebSocket(describe(&e))),
    }
}

/// What a server presented during a probe.
#[derive(Debug, Clone)]
pub struct Probe {
    /// The key in the server's certificate (absent for plain HTTP, or if the connection
    /// failed before a certificate arrived).
    pub presented: Option<SpkiHash>,
    /// `Ok` when the trust setting accepted the server.
    pub verdict: Result<(), String>,
}

/// Connect to a server's TLS port and report the key it presents and whether `trust`
/// accepts it. Used while pairing: the user compares `presented` with the pairing code the
/// daemon shows. It sends no request and no credential.
pub async fn probe(base_url: &str, trust: &Trust) -> Result<Probe, ClientError> {
    let base = parse_base_url(base_url)?;
    if base.scheme() != "https" {
        return Ok(Probe {
            presented: None,
            verdict: Ok(()),
        });
    }
    let recorder = Arc::new(Recorder {
        inner: trust::verifier(trust)?,
        seen: Mutex::new(None),
    });
    let tls = trust::config_with(recorder.clone())?;
    let remote = Remote::with_config(base, tls, None)?;
    let verdict = match remote.connect_io().await {
        Ok(_) => Ok(()),
        Err(ClientError::Connect(e)) if recorder.seen.lock().expect("not poisoned").is_none() => {
            return Err(ClientError::Connect(e));
        }
        Err(e) => Err(e.to_string()),
    };
    let presented = *recorder.seen.lock().expect("not poisoned");
    Ok(Probe { presented, verdict })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_urls() {
        let ok = |s: &str| parse_base_url(s).unwrap().to_string();
        assert_eq!(ok("https://seedbox.example"), "https://seedbox.example/");
        assert_eq!(
            ok("https://seedbox.example/torrents"),
            "https://seedbox.example/torrents/"
        );
        assert_eq!(ok("http://localhost:8742"), "http://localhost:8742/");
        assert_eq!(ok("http://127.0.0.1:8742/"), "http://127.0.0.1:8742/");
        assert_eq!(ok("http://[::1]:8742"), "http://[::1]:8742/");
        for bad in [
            "http://seedbox.example",
            "http://192.168.1.5:8742",
            "ftp://x",
            "https://user:pw@seedbox.example",
            "https://seedbox.example/?a=1",
            "not a url",
        ] {
            assert!(parse_base_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn api_paths_stay_under_the_api() {
        let r = Remote::new("https://seedbox.example/apt", &Trust::Os, None).unwrap();
        assert_eq!(
            r.api_url("/api/v1/torrents?deleteFiles=true")
                .unwrap()
                .as_str(),
            "https://seedbox.example/apt/api/v1/torrents?deleteFiles=true"
        );
        for bad in [
            "api/v1/x",
            "/other",
            "/api/../secret",
            "/api/v1/./x",
            "//evil.example/api/v1",
            "/api\\v1",
        ] {
            assert!(r.api_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn tokens_are_never_printed() {
        let t = Token::new("apt_secret");
        assert!(!format!("{t:?}").contains("secret"));
        let r = Remote::new("https://x.example", &Trust::Os, Some(t)).unwrap();
        assert!(!format!("{r:?}").contains("secret"));
    }
}

//! The connection to one `deluged`, and typed calls on it.

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

use super::auth::{self, Credentials};
use super::frame::{self, FrameError, FrameLimits, HEADER_LEN};
use super::rencode::Value;
use super::tls::{self, CertError, PinnedCert};

/// Deluge's default daemon port.
pub const DEFAULT_PORT: u16 = 58846;

const RPC_RESPONSE: i64 = 1;
const RPC_ERROR: i64 = 2;
const RPC_EVENT: i64 = 3;

/// Where the daemon is, and how long to wait for it.
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    /// Default `127.0.0.1`. Deluge listens on all interfaces but, unless `allow_remote` is
    /// set, accepts logins only from loopback.
    pub host: String,
    /// Default [`DEFAULT_PORT`]; [`ConnectOptions::from_config_dir`] reads `daemon_port`.
    pub port: u16,
    /// For the TCP connect and the TLS handshake together. Default 10 s.
    pub connect_timeout: Duration,
    /// For each call, from sending the request to the whole reply. Default 60 s.
    pub call_timeout: Duration,
    pub limits: FrameLimits,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: DEFAULT_PORT,
            connect_timeout: Duration::from_secs(10),
            call_timeout: Duration::from_secs(60),
            limits: FrameLimits::default(),
        }
    }
}

impl ConnectOptions {
    /// The defaults, with the port from `<config_dir>/core.conf` (`daemon_port`) when it's
    /// there.
    pub fn from_config_dir(config_dir: &Path) -> Self {
        let mut o = Self::default();
        if let Some(port) = crate::read_config(&config_dir.join("core.conf"))
            .ok()
            .and_then(|c| c.get("daemon_port")?.as_u64())
            .and_then(|p| u16::try_from(p).ok())
        {
            o.port = port;
        }
        o
    }
}

/// An exception the daemon raised for a call.
#[derive(Debug, Clone)]
pub struct RemoteError {
    pub method: String,
    /// The Python exception class, such as `InvalidTorrentError` or `BadLoginError`
    /// (unwrapped from Deluge's `WrappedException`).
    pub exception: String,
    /// The exception's message, when it had one.
    pub message: String,
    /// The Python traceback, from the daemon. Not shown by `Display`.
    pub traceback: String,
}

impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Deluge's {} failed: {}", self.method, self.exception)?;
        if !self.message.is_empty() {
            write!(f, ": {}", self.message)?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("connecting to Deluge at {addr}: {source}")]
    Connect {
        addr: String,
        source: std::io::Error,
    },
    #[error("Deluge at {0} didn't answer within the connect timeout")]
    ConnectTimeout(String),
    #[error("TLS with Deluge at {addr}: {source}")]
    Tls {
        addr: String,
        source: std::io::Error,
    },
    #[error(transparent)]
    Cert(#[from] CertError),
    #[error(transparent)]
    AuthFile(#[from] auth::AuthFileError),
    #[error("talking to Deluge: {0}")]
    Io(std::io::Error),
    #[error("Deluge didn't answer {method} within {after:?}")]
    Timeout { method: String, after: Duration },
    #[error("reading Deluge's reply: {0}")]
    Frame(#[from] FrameError),
    #[error("Deluge sent something unexpected: {0}")]
    Protocol(String),
    #[error("{0}")]
    Remote(RemoteError),
    #[error("Deluge accepted the login but gave no access (auth level 0)")]
    NoAccess,
    #[error("this connection failed earlier; connect again")]
    Broken,
}

impl RpcError {
    /// The daemon's exception, when that's what this is.
    pub fn remote(&self) -> Option<&RemoteError> {
        match self {
            RpcError::Remote(r) => Some(r),
            _ => None,
        }
    }
}

/// Deluge's and libtorrent's versions, as the daemon reports them (`2.1.1`, `2.0.11.0`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Versions {
    pub deluge: String,
    pub libtorrent: String,
}

/// Deluge's torrent states (`deluge.common.TORRENT_STATE`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TorrentState {
    Allocating,
    Checking,
    Downloading,
    Seeding,
    Paused,
    Error,
    Queued,
    Moving,
    Other(String),
}

impl TorrentState {
    fn parse(s: &str) -> Self {
        match s {
            "Allocating" => Self::Allocating,
            "Checking" => Self::Checking,
            "Downloading" => Self::Downloading,
            "Seeding" => Self::Seeding,
            "Paused" => Self::Paused,
            "Error" => Self::Error,
            "Queued" => Self::Queued,
            "Moving" => Self::Moving,
            other => Self::Other(other.to_owned()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tracker {
    pub url: String,
    pub tier: i64,
}

/// One torrent as the daemon sees it now.
#[derive(Debug, Clone, PartialEq)]
pub struct TorrentStatus {
    /// The v1 info-hash in lowercase hex.
    pub id: String,
    pub name: String,
    pub state: TorrentState,
    /// libtorrent's paused flag. A `Queued` torrent is paused by libtorrent's queue too, and
    /// may be resumed by it; only [`TorrentState::Paused`] means Deluge won't start it.
    pub paused: bool,
    /// Percent done, 0 to 100.
    pub progress: f64,
    pub save_path: String,
    /// The Label plugin's label. `None` when unlabelled or the plugin is off.
    pub label: Option<String>,
    pub trackers: Vec<Tracker>,
    /// The current tracker's host, as Deluge shows it (`tracker.example.org`).
    pub tracker_host: String,
    pub total_size: u64,
    pub is_finished: bool,
}

impl TorrentStatus {
    /// Deluge isn't transferring this torrent and won't start it on its own: `Paused`, or
    /// stopped by an error.
    pub fn is_stopped(&self) -> bool {
        matches!(self.state, TorrentState::Paused | TorrentState::Error)
    }
}

/// The status keys [`DelugeClient::torrents`] asks for.
const STATUS_KEYS: &[&str] = &[
    "name",
    "state",
    "paused",
    "progress",
    "save_path",
    "label",
    "trackers",
    "tracker_host",
    "total_size",
    "is_finished",
];

/// A logged-in connection to one Deluge daemon.
///
/// Calls go one at a time (`&mut self`). A call that times out, or any I/O or protocol
/// error, leaves the connection unusable: later calls return [`RpcError::Broken`], and the
/// caller connects again. The daemon's own exceptions ([`RpcError::Remote`]) don't break it.
pub struct DelugeClient {
    stream: TlsStream<TcpStream>,
    next_id: i64,
    call_timeout: Duration,
    limits: FrameLimits,
    broken: bool,
    auth_level: Option<i64>,
}

impl fmt::Debug for DelugeClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DelugeClient")
            .field("next_id", &self.next_id)
            .field("broken", &self.broken)
            .field("auth_level", &self.auth_level)
            .finish_non_exhaustive()
    }
}

impl DelugeClient {
    /// Connect to the daemon that owns `config_dir` and log in as `localclient`: the port
    /// from `core.conf`, the certificate pinned from `ssl/daemon.cert`, the password from
    /// `auth`. Each is read now and not kept.
    pub async fn connect_config_dir(config_dir: &Path) -> Result<Self, RpcError> {
        let opts = ConnectOptions::from_config_dir(config_dir);
        let pin = PinnedCert::from_config_dir(config_dir)?;
        let creds = Credentials::from_config_dir(config_dir, auth::LOCALCLIENT)?;
        let mut c = Self::connect(&opts, &pin).await?;
        c.login(&creds).await?;
        Ok(c)
    }

    /// Open a TLS connection, trusting only `pin`. Log in before anything else: the daemon
    /// answers only `daemon.info` and `daemon.login` until then.
    pub async fn connect(opts: &ConnectOptions, pin: &PinnedCert) -> Result<Self, RpcError> {
        let addr = format!("{}:{}", opts.host, opts.port);
        let server_name = match opts.host.parse::<std::net::IpAddr>() {
            Ok(ip) => ServerName::IpAddress(ip.into()),
            Err(_) => ServerName::try_from(opts.host.clone()).map_err(|e| RpcError::Connect {
                addr: addr.clone(),
                source: std::io::Error::new(std::io::ErrorKind::InvalidInput, e),
            })?,
        };
        let connector = tokio_rustls::TlsConnector::from(Arc::new(tls::client_config(pin)));
        let connecting = async {
            let tcp = TcpStream::connect((opts.host.as_str(), opts.port))
                .await
                .map_err(|source| RpcError::Connect {
                    addr: addr.clone(),
                    source,
                })?;
            let _ = tcp.set_nodelay(true);
            connector
                .connect(server_name, tcp)
                .await
                .map_err(|source| RpcError::Tls {
                    addr: addr.clone(),
                    source,
                })
        };
        let stream = tokio::time::timeout(opts.connect_timeout, connecting)
            .await
            .map_err(|_| RpcError::ConnectTimeout(addr.clone()))??;
        tracing::debug!(%addr, "connected to Deluge");
        Ok(Self {
            stream,
            next_id: 0,
            call_timeout: opts.call_timeout,
            limits: opts.limits,
            broken: false,
            auth_level: None,
        })
    }

    /// `daemon.login`. Returns Deluge's auth level for the account (`10` admin, `5` normal,
    /// `1` read-only); pausing and resuming need normal. A wrong password is
    /// [`RpcError::Remote`] with exception `BadLoginError`, and the daemon closes the
    /// connection.
    pub async fn login(&mut self, creds: &Credentials) -> Result<i64, RpcError> {
        // Deluge 2 refuses a login without `client_version` (IncompatibleClient) but doesn't
        // check its value.
        let reply = self
            .call(
                "daemon.login",
                vec![creds.username.as_str().into(), creds.password().into()],
                vec![(
                    "client_version".into(),
                    format!("apitorrent/{}", env!("CARGO_PKG_VERSION")).into(),
                )],
            )
            .await?;
        let level = reply
            .as_i64()
            .ok_or_else(|| RpcError::Protocol(format!("login returned {reply:?}")))?;
        if level <= 0 {
            self.broken = true;
            return Err(RpcError::NoAccess);
        }
        self.auth_level = Some(level);
        Ok(level)
    }

    /// The auth level from [`DelugeClient::login`], once logged in.
    pub fn auth_level(&self) -> Option<i64> {
        self.auth_level
    }

    /// Call any exported method, such as `core.get_config_value` with `["listen_ports"]`.
    /// `kwargs` are Python keyword arguments.
    pub async fn call(
        &mut self,
        method: &str,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<Value, RpcError> {
        if self.broken {
            return Err(RpcError::Broken);
        }
        let id = self.next_id;
        self.next_id += 1;
        // Deluge reads a message as a list of requests, each [id, method, args, kwargs].
        let request = Value::List(vec![Value::List(vec![
            Value::Int(id),
            method.into(),
            Value::List(args),
            Value::Dict(
                kwargs
                    .into_iter()
                    .map(|(k, v)| (Value::Str(k), v))
                    .collect(),
            ),
        ])]);
        let message = frame::encode(&request)?;
        let timeout = self.call_timeout;
        let result = match tokio::time::timeout(timeout, self.exchange(&message, id, method)).await
        {
            Ok(r) => r,
            Err(_) => Err(RpcError::Timeout {
                method: method.to_owned(),
                after: timeout,
            }),
        };
        if let Err(e) = &result
            && !matches!(e, RpcError::Remote(_))
        {
            self.broken = true;
        }
        result
    }

    async fn exchange(&mut self, message: &[u8], id: i64, method: &str) -> Result<Value, RpcError> {
        self.stream.write_all(message).await.map_err(RpcError::Io)?;
        self.stream.flush().await.map_err(RpcError::Io)?;
        loop {
            let reply = self.read_message().await?;
            let parts = reply
                .as_list()
                .filter(|l| l.len() >= 3)
                .ok_or_else(|| RpcError::Protocol("a reply that isn't a list of 3+".into()))?;
            let kind = parts[0].as_i64();
            if kind == Some(RPC_EVENT) {
                // Sent only to clients that asked with daemon.set_event_interest; we don't.
                tracing::trace!(event = ?parts[1], "ignoring Deluge event");
                continue;
            }
            if parts[1].as_i64() != Some(id) {
                return Err(RpcError::Protocol(format!(
                    "a reply to request {:?} while waiting for {id}",
                    parts[1]
                )));
            }
            return match kind {
                Some(RPC_RESPONSE) => Ok(parts[2].clone()),
                Some(RPC_ERROR) => Err(RpcError::Remote(remote_error(method, &parts[2..]))),
                _ => Err(RpcError::Protocol(format!(
                    "unknown message type {:?}",
                    parts[0]
                ))),
            };
        }
    }

    async fn read_message(&mut self) -> Result<Value, RpcError> {
        let mut header = [0u8; HEADER_LEN];
        self.stream
            .read_exact(&mut header)
            .await
            .map_err(RpcError::Io)?;
        let len = frame::parse_header(header, &self.limits)?;
        let mut body = vec![0u8; len];
        self.stream
            .read_exact(&mut body)
            .await
            .map_err(RpcError::Io)?;
        Ok(frame::decode_body(&body, &self.limits)?)
    }

    /// `daemon.info`: Deluge's version, such as `2.1.1`. Works before logging in.
    pub async fn deluge_version(&mut self) -> Result<String, RpcError> {
        string_reply(
            "daemon.info",
            self.call("daemon.info", vec![], vec![]).await?,
        )
    }

    /// `core.get_libtorrent_version`, such as `2.0.11.0`.
    pub async fn libtorrent_version(&mut self) -> Result<String, RpcError> {
        let m = "core.get_libtorrent_version";
        string_reply(m, self.call(m, vec![], vec![]).await?)
    }

    pub async fn versions(&mut self) -> Result<Versions, RpcError> {
        Ok(Versions {
            deluge: self.deluge_version().await?,
            libtorrent: self.libtorrent_version().await?,
        })
    }

    /// `core.get_config_value(key)`, such as `listen_ports` or `download_location`.
    pub async fn config_value(&mut self, key: &str) -> Result<Value, RpcError> {
        self.call("core.get_config_value", vec![key.into()], vec![])
            .await
    }

    /// The daemon's torrents, all of them or those in `ids`. Ids Deluge doesn't have are
    /// left out, not an error. In no particular order.
    pub async fn torrents<S: AsRef<str>>(
        &mut self,
        ids: Option<&[S]>,
    ) -> Result<Vec<TorrentStatus>, RpcError> {
        let filter = match ids {
            Some(ids) => Value::dict([("id", Value::str_list(ids))]),
            None => Value::Dict(vec![]),
        };
        let m = "core.get_torrents_status";
        let reply = self
            .call(m, vec![filter, Value::str_list(STATUS_KEYS)], vec![])
            .await?;
        let pairs = reply
            .as_dict()
            .ok_or_else(|| RpcError::Protocol(format!("{m} returned {reply:?}")))?;
        pairs
            .iter()
            .map(|(id, status)| {
                let id = id
                    .as_str()
                    .ok_or_else(|| RpcError::Protocol(format!("{m}: torrent id {id:?}")))?;
                Ok(parse_status(id, status))
            })
            .collect()
    }

    /// Of `ids`, those the daemon has.
    pub async fn known_ids<S: AsRef<str>>(&mut self, ids: &[S]) -> Result<Vec<String>, RpcError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let m = "core.get_torrents_status";
        let filter = Value::dict([("id", Value::str_list(ids))]);
        // Not an empty key list: Deluge reads that as "every key".
        let reply = self
            .call(m, vec![filter, Value::str_list(["hash"])], vec![])
            .await?;
        let pairs = reply
            .as_dict()
            .ok_or_else(|| RpcError::Protocol(format!("{m} returned {reply:?}")))?;
        pairs
            .iter()
            .map(|(id, _)| {
                id.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| RpcError::Protocol(format!("{m}: torrent id {id:?}")))
            })
            .collect()
    }

    /// Pause these torrents, and turn off their auto-management so Deluge's queue won't
    /// resume them. Ids Deluge doesn't have are skipped. Returns the ids it paused.
    ///
    /// Returns as soon as Deluge has asked libtorrent; [`DelugeClient::wait_until_stopped`]
    /// tells when they have stopped.
    ///
    /// Uses `core.pause_torrent` with a list, which Deluge 2.0 and 2.1 both take. Deluge
    /// pauses **every** torrent when given an empty list, and stops at the first unknown id
    /// with a `KeyError`; this looks the ids up first and sends nothing when none are known.
    pub async fn pause<S: AsRef<str>>(&mut self, ids: &[S]) -> Result<Vec<String>, RpcError> {
        self.pause_or_resume("core.pause_torrent", ids).await
    }

    /// Resume these torrents (`core.resume_torrent`), skipping ids Deluge doesn't have, as
    /// [`DelugeClient::pause`] does. Returns the ids it resumed.
    pub async fn resume<S: AsRef<str>>(&mut self, ids: &[S]) -> Result<Vec<String>, RpcError> {
        self.pause_or_resume("core.resume_torrent", ids).await
    }

    async fn pause_or_resume<S: AsRef<str>>(
        &mut self,
        method: &str,
        ids: &[S],
    ) -> Result<Vec<String>, RpcError> {
        let known = self.known_ids(ids).await?;
        if !known.is_empty() {
            self.call(method, vec![Value::str_list(&known)], vec![])
                .await?;
        }
        Ok(known)
    }

    /// Poll until every torrent in `ids` that Deluge has is [stopped](TorrentStatus::is_stopped),
    /// or `timeout` passes. Returns the ones still running then (empty when all stopped).
    pub async fn wait_until_stopped<S: AsRef<str>>(
        &mut self,
        ids: &[S],
        timeout: Duration,
    ) -> Result<Vec<TorrentStatus>, RpcError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let running: Vec<TorrentStatus> = self
                .torrents(Some(ids))
                .await?
                .into_iter()
                .filter(|t| !t.is_stopped())
                .collect();
            if running.is_empty() || tokio::time::Instant::now() >= deadline {
                return Ok(running);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Remove a torrent from Deluge, leaving its downloaded files where they are
    /// (`core.remove_torrent(id, False)`). An unknown id is an `InvalidTorrentError`.
    pub async fn remove_keep_data(&mut self, id: &str) -> Result<(), RpcError> {
        let m = "core.remove_torrent";
        let reply = self
            .call(m, vec![id.into(), Value::Bool(false)], vec![])
            .await?;
        match reply {
            Value::Bool(true) => Ok(()),
            other => Err(RpcError::Protocol(format!("{m} returned {other:?}"))),
        }
    }
}

fn string_reply(method: &str, v: Value) -> Result<String, RpcError> {
    match v {
        Value::Str(s) => Ok(s),
        other => Err(RpcError::Protocol(format!("{method} returned {other:?}"))),
    }
}

/// `[exception class, args, kwargs, traceback]` from an `RPC_ERROR`.
fn remote_error(method: &str, parts: &[Value]) -> RemoteError {
    let text = |v: Option<&Value>| v.and_then(Value::as_str).unwrap_or_default().to_owned();
    let mut exception = text(parts.first());
    let args = parts.get(1).and_then(Value::as_list).unwrap_or_default();
    let mut message = text(args.first());
    let traceback = text(parts.get(3));
    // Exceptions that aren't Deluge's own arrive wrapped: (message, type, traceback).
    if exception == "WrappedException" && args.len() >= 2 {
        exception = text(args.get(1));
    }
    if message.is_empty() {
        message = text(parts.get(2).and_then(|kw| kw.get("message")));
    }
    RemoteError {
        method: method.to_owned(),
        exception,
        message,
        traceback,
    }
}

#[allow(clippy::cast_sign_loss)]
fn parse_status(id: &str, s: &Value) -> TorrentStatus {
    let text = |k: &str| {
        s.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let trackers = s
        .get("trackers")
        .and_then(Value::as_list)
        .unwrap_or_default()
        .iter()
        .filter_map(|t| {
            Some(Tracker {
                url: t.get("url")?.as_str()?.to_owned(),
                tier: t.get("tier").and_then(Value::as_i64).unwrap_or(0),
            })
        })
        .collect();
    let label = text("label");
    TorrentStatus {
        id: id.to_owned(),
        name: text("name"),
        state: TorrentState::parse(&text("state")),
        paused: s.get("paused").and_then(Value::as_bool).unwrap_or(false),
        progress: s.get("progress").and_then(Value::as_f64).unwrap_or(0.0),
        save_path: text("save_path"),
        label: (!label.is_empty()).then_some(label),
        trackers,
        tracker_host: text("tracker_host"),
        total_size: s
            .get("total_size")
            .and_then(Value::as_i64)
            .map_or(0, |n| n.max(0) as u64),
        is_finished: s
            .get("is_finished")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

#[cfg(test)]
mod tests;

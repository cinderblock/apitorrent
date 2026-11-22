use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use apitorrent_metainfo::Sha1Hash;
use apitorrent_proxy::{Proxy, Target, UdpAssociation};
use reqwest::StatusCode;
use reqwest::redirect;
use tokio::net::UdpSocket;
use tokio::time::Instant;
use url::{Host, Url};

use crate::error::TrackerError;
use crate::types::{AnnounceRequest, AnnounceResponse, ScrapeResponse};
use crate::udp::{self, ConnectionCache, ConnectionScope, PeerFamily, RetryPolicy};
use crate::{http, tls};

/// Settings for a [`TrackerClient`].
///
/// There are deliberately no TLS settings: HTTPS trackers are always verified against the
/// OS trust store (see the `tls` module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerConfig {
    /// The whole HTTP request, from connecting to the last byte of the body.
    pub http_timeout: Duration,
    /// TCP and TLS connection set-up for HTTP, and DNS resolution for UDP.
    pub connect_timeout: Duration,
    /// The largest HTTP response body accepted. A response with a real peer list is a few
    /// kilobytes.
    pub max_response_size: usize,
    /// Redirects followed per HTTP request. A redirect from HTTPS to plain HTTP is always
    /// refused.
    pub max_redirects: usize,
    pub user_agent: String,
    /// UDP retransmission, applied to each step (connect, then announce or scrape).
    pub udp_retry: RetryPolicy,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            http_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            max_response_size: 2 * 1024 * 1024,
            max_redirects: 5,
            user_agent: concat!("apitorrent/", env!("CARGO_PKG_VERSION")).into(),
            udp_retry: RetryPolicy::default(),
        }
    }
}

/// A client for HTTP, HTTPS and UDP trackers.
///
/// Cloning is cheap and clones share the HTTP connection pool and the UDP connection-id
/// cache, so one client should serve the whole session.
#[derive(Debug, Clone)]
pub struct TrackerClient {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    /// Rebuilt when the proxy changes; requests in flight keep the client they started
    /// with.
    http: RwLock<reqwest::Client>,
    config: TrackerConfig,
    connections: Mutex<ConnectionCache>,
    /// Where tracker connections go: straight to the tracker, or through this proxy.
    proxy: RwLock<Option<Arc<Proxy>>>,
}

enum Scheme {
    Http,
    Udp,
}

fn parse_url(url: &str) -> Result<(Url, Scheme), TrackerError> {
    let parsed = Url::parse(url).map_err(|e| TrackerError::InvalidUrl(e.to_string()))?;
    let scheme = match parsed.scheme() {
        "http" | "https" => Scheme::Http,
        "udp" => Scheme::Udp,
        other => return Err(TrackerError::UnsupportedScheme(other.into())),
    };
    Ok((parsed, scheme))
}

/// The redirect policy: at most `max` hops, only to `http`/`https`, and never back to
/// plain HTTP once the chain has used HTTPS.
fn redirect_policy(max: usize) -> redirect::Policy {
    redirect::Policy::custom(move |attempt| {
        // `previous` starts with the original URL, so it holds one entry per hop so far.
        if attempt.previous().len() > max {
            return attempt.error("too many redirects");
        }
        let next = attempt.url().scheme();
        if next != "http" && next != "https" {
            return attempt.error("redirect to a non-HTTP URL");
        }
        if next == "http" && attempt.previous().iter().any(|u| u.scheme() == "https") {
            return attempt.error("refusing redirect from HTTPS to plain HTTP");
        }
        attempt.follow()
    })
}

/// A general HTTP client with the same safety rules as tracker requests: certificates
/// verified against the OS trust store (never skippable), no proxies from the environment,
/// no `Referer`, and redirects only between HTTP(S) URLs and never down from HTTPS. Used
/// for web seeds and anything else that fetches over HTTP.
pub fn strict_http_client(
    user_agent: &str,
    timeout: std::time::Duration,
    connect_timeout: std::time::Duration,
) -> Result<reqwest::Client, TrackerError> {
    strict_http_client_builder(user_agent, timeout, connect_timeout)?
        .build()
        .map_err(TrackerError::network)
}

/// The builder behind [`strict_http_client`], for callers that must tighten it further:
/// the plugin host replaces the redirect policy (it follows redirects itself, checking
/// each hop against the plugin's grants) and the DNS resolver (it refuses private
/// addresses).
///
/// Only tighten. The TLS configuration is preconfigured, so reqwest's own TLS switches
/// (`danger_accept_invalid_certs` and friends) have no effect on it; don't call them, and
/// don't add a proxy.
pub fn strict_http_client_builder(
    user_agent: &str,
    timeout: std::time::Duration,
    connect_timeout: std::time::Duration,
) -> Result<reqwest::ClientBuilder, TrackerError> {
    Ok(reqwest::Client::builder()
        .tls_backend_preconfigured(tls::client_config()?)
        .redirect(redirect_policy(5))
        .referer(false)
        .no_proxy()
        .user_agent(user_agent)
        .timeout(timeout)
        .connect_timeout(connect_timeout))
}

/// [`strict_http_client`], with every request going through `proxy`. The proxy is the
/// only one used: there is no fallback to a direct connection.
pub fn strict_http_client_via(
    user_agent: &str,
    timeout: std::time::Duration,
    connect_timeout: std::time::Duration,
    proxy: Option<&Proxy>,
) -> Result<reqwest::Client, TrackerError> {
    with_proxy(
        strict_http_client_builder(user_agent, timeout, connect_timeout)?,
        proxy,
    )?
    .build()
    .map_err(TrackerError::network)
}

/// Route a client's requests through `proxy`. The builder must already have had
/// `no_proxy()`, so nothing from the environment is mixed in.
fn with_proxy(
    builder: reqwest::ClientBuilder,
    proxy: Option<&Proxy>,
) -> Result<reqwest::ClientBuilder, TrackerError> {
    let Some(proxy) = proxy else {
        return Ok(builder);
    };
    // The URL holds the password: errors about it must not quote it.
    let mut url = proxy
        .url()
        .map_err(|e| TrackerError::network(format!("bad proxy address: {e}")))?;
    // An HTTP proxy's credentials go in as Basic authentication; left in the URL, they
    // would reach the proxy still percent-encoded.
    let http_auth = match (&proxy.credentials, proxy.kind) {
        (Some(c), apitorrent_proxy::ProxyKind::Http) => {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            Some(c)
        }
        _ => None,
    };
    let mut reqwest_proxy = reqwest::Proxy::all(url.as_str())
        .map_err(|_| TrackerError::network("bad proxy address"))?;
    if let Some(c) = http_auth {
        reqwest_proxy = reqwest_proxy.basic_auth(&c.username, &c.password);
    }
    Ok(builder.proxy(reqwest_proxy))
}

/// The tracker HTTP client, direct or through `proxy`.
fn build_http(
    config: &TrackerConfig,
    proxy: Option<&Proxy>,
) -> Result<reqwest::Client, TrackerError> {
    // No TLS session resumption and no kept-alive connections: either would let a
    // tracker tie a client identity's announces to the previous identity's after a
    // switch. libtorrent doesn't reuse them either (it sends `Connection: close`), and
    // announces are minutes apart, so this costs a handshake nobody would have saved.
    let mut tls = tls::client_config()?;
    tls.resumption = rustls::client::Resumption::disabled();
    let builder = reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .pool_max_idle_per_host(0)
        .redirect(redirect_policy(config.max_redirects))
        // The announce URL can hold a private tracker's passkey; don't hand it to
        // wherever a redirect points.
        .referer(false)
        // The proxy is an explicit setting, never picked up from the environment.
        .no_proxy()
        .user_agent(config.user_agent.clone())
        .timeout(config.http_timeout)
        .connect_timeout(config.connect_timeout);
    with_proxy(builder, proxy)?
        .build()
        .map_err(TrackerError::network)
}

impl TrackerClient {
    /// A client with the default [`TrackerConfig`].
    pub fn new() -> Result<Self, TrackerError> {
        Self::with_config(TrackerConfig::default())
    }

    pub fn with_config(config: TrackerConfig) -> Result<Self, TrackerError> {
        let http = build_http(&config, None)?;
        Ok(Self {
            inner: Arc::new(Inner {
                http: RwLock::new(http),
                config,
                connections: Mutex::new(ConnectionCache::default()),
                proxy: RwLock::new(None),
            }),
        })
    }

    pub fn config(&self) -> &TrackerConfig {
        &self.inner.config
    }

    /// Send every tracker request through `proxy` from now on, or straight to the tracker
    /// with `None`. HTTP(S) trackers go through any kind of proxy; UDP trackers only
    /// through SOCKS5 (a UDP association). Through an HTTP or SOCKS4 proxy, UDP trackers
    /// fail with [`TrackerError::UdpNotProxied`] rather than going around the proxy.
    pub fn set_proxy(&self, proxy: Option<Proxy>) -> Result<(), TrackerError> {
        let http = build_http(&self.inner.config, proxy.as_ref())?;
        *self
            .inner
            .http
            .write()
            .unwrap_or_else(PoisonError::into_inner) = http;
        *self
            .inner
            .proxy
            .write()
            .unwrap_or_else(PoisonError::into_inner) = proxy.map(Arc::new);
        Ok(())
    }

    fn proxy(&self) -> Option<Arc<Proxy>> {
        self.inner
            .proxy
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Announce to the tracker at `url` (`http://`, `https://` or `udp://`).
    pub async fn announce(
        &self,
        url: &str,
        req: &AnnounceRequest,
    ) -> Result<AnnounceResponse, TrackerError> {
        let (parsed, scheme) = parse_url(url)?;
        let result = match scheme {
            Scheme::Http => {
                let (status, body) = self
                    .http_get(&http::announce_url(url, req), req.user_agent.as_deref())
                    .await?;
                interpret(status, &body, http::parse_announce)
            }
            Scheme::Udp => {
                self.udp_request(
                    &parsed,
                    Some(req.key),
                    |conn, tx| udp::encode_announce(conn, tx, req).to_vec(),
                    udp::decode_announce,
                )
                .await
            }
        };
        tracing::debug!(
            tracker = parsed.host_str().unwrap_or(""),
            ok = result.is_ok(),
            "announce"
        );
        result
    }

    /// Scrape swarm statistics for up to [`crate::MAX_SCRAPE_HASHES`] torrents.
    pub async fn scrape(
        &self,
        url: &str,
        hashes: &[Sha1Hash],
    ) -> Result<ScrapeResponse, TrackerError> {
        self.scrape_as(url, hashes, None).await
    }

    /// [`TrackerClient::scrape`], with this `User-Agent` on an HTTP scrape in place of the
    /// configured one.
    pub async fn scrape_as(
        &self,
        url: &str,
        hashes: &[Sha1Hash],
        user_agent: Option<&str>,
    ) -> Result<ScrapeResponse, TrackerError> {
        http::check_scrape_hashes(hashes)?;
        let (parsed, scheme) = parse_url(url)?;
        match scheme {
            Scheme::Http => {
                let (status, body) = self
                    .http_get(&http::scrape_url(url, hashes)?, user_agent)
                    .await?;
                interpret(status, &body, |b| http::parse_scrape(b, hashes))
            }
            Scheme::Udp => {
                self.udp_request(
                    &parsed,
                    None,
                    |conn, tx| udp::encode_scrape(conn, tx, hashes).expect("hashes checked"),
                    |buf, tx, _| udp::decode_scrape(buf, tx, hashes),
                )
                .await
            }
        }
    }

    /// GET `url` and read the body, up to the size limit. `user_agent` replaces the
    /// configured one for this request.
    async fn http_get(
        &self,
        url: &str,
        user_agent: Option<&str>,
    ) -> Result<(StatusCode, Vec<u8>), TrackerError> {
        let limit = self.inner.config.max_response_size;
        let http = self
            .inner
            .http
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut request = http.get(url);
        if let Some(agent) = user_agent {
            let value = reqwest::header::HeaderValue::from_str(agent).map_err(|_| {
                TrackerError::InvalidRequest("the user agent is not a valid header")
            })?;
            request = request.header(reqwest::header::USER_AGENT, value);
        }
        let mut resp = request.send().await?;
        let status = resp.status();
        if resp
            .content_length()
            .is_some_and(|len| len > u64::try_from(limit).unwrap_or(u64::MAX))
        {
            return Err(TrackerError::ResponseTooLarge { limit });
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            if body.len() + chunk.len() > limit {
                return Err(TrackerError::ResponseTooLarge { limit });
            }
            body.extend_from_slice(&chunk);
        }
        Ok((status, body))
    }

    /// One UDP tracker request: straight to the tracker, or through the proxy when there
    /// is one. `key` is the announce key, part of the connection id's scope.
    async fn udp_request<T>(
        &self,
        url: &Url,
        key: Option<u32>,
        encode: impl Fn(u64, u32) -> Vec<u8>,
        decode: impl Fn(&[u8], u32, PeerFamily) -> Result<T, TrackerError>,
    ) -> Result<T, TrackerError> {
        if let Some(proxy) = self.proxy() {
            return self.udp_proxied(&proxy, url, encode, decode).await;
        }
        let addr = self.resolve_udp(url).await?;
        self.udp_exchange((addr, key), encode, decode).await
    }

    /// A UDP tracker request through a SOCKS5 UDP association, with a fresh connection id
    /// each time: the tracker's address may be a name only the proxy resolves, so there
    /// is no address to cache the id under.
    async fn udp_proxied<T>(
        &self,
        proxy: &Proxy,
        url: &Url,
        encode: impl Fn(u64, u32) -> Vec<u8>,
        decode: impl Fn(&[u8], u32, PeerFamily) -> Result<T, TrackerError>,
    ) -> Result<T, TrackerError> {
        if !proxy.kind.carries_udp() {
            return Err(TrackerError::UdpNotProxied);
        }
        let port = url
            .port()
            .ok_or_else(|| TrackerError::InvalidUrl("a UDP tracker URL needs a port".into()))?;
        let target = match url.host() {
            Some(Host::Ipv4(ip)) => Target::Addr(SocketAddr::new(ip.into(), port)),
            Some(Host::Ipv6(ip)) => Target::Addr(SocketAddr::new(ip.into(), port)),
            Some(Host::Domain(name)) if !name.is_empty() => Target::Name(name.to_owned(), port),
            _ => return Err(TrackerError::InvalidUrl("missing host".into())),
        };
        let association =
            tokio::time::timeout(self.inner.config.connect_timeout, proxy.udp_associate())
                .await
                .map_err(|_| TrackerError::Timeout)?
                .map_err(TrackerError::network)?;
        let path = UdpPath::Proxied(&association, &target);
        let mut buf = vec![0u8; udp::MAX_DATAGRAM];
        let tx = rand::random::<u32>();
        let connection_id = self
            .transact(&path, &udp::encode_connect(tx), tx, &mut buf, |b, _| {
                udp::decode_connect(b, tx)
            })
            .await?;
        let tx = rand::random::<u32>();
        let packet = encode(connection_id, tx);
        self.transact(&path, &packet, tx, &mut buf, |b, family| {
            decode(b, tx, family)
        })
        .await
    }

    async fn resolve_udp(&self, url: &Url) -> Result<SocketAddr, TrackerError> {
        let port = url
            .port()
            .ok_or_else(|| TrackerError::InvalidUrl("a UDP tracker URL needs a port".into()))?;
        match url.host() {
            Some(Host::Ipv4(ip)) => Ok(SocketAddr::new(ip.into(), port)),
            Some(Host::Ipv6(ip)) => Ok(SocketAddr::new(ip.into(), port)),
            // `udp` is not a "special" URL scheme, so IPv4 literals arrive as domains;
            // `lookup_host` parses them without a DNS query.
            Some(Host::Domain(name)) if !name.is_empty() => {
                let lookup = tokio::net::lookup_host((name, port));
                let mut addrs = tokio::time::timeout(self.inner.config.connect_timeout, lookup)
                    .await
                    .map_err(|_| TrackerError::Timeout)?
                    .map_err(TrackerError::network)?;
                addrs
                    .next()
                    .ok_or_else(|| TrackerError::network(format!("{name} has no address")))
            }
            _ => Err(TrackerError::InvalidUrl("missing host".into())),
        }
    }

    fn cached_connection(&self, scope: &ConnectionScope) -> Option<u64> {
        self.connections().get(scope, Instant::now().into_std())
    }

    fn connections(&self) -> std::sync::MutexGuard<'_, ConnectionCache> {
        self.inner
            .connections
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Connect (or reuse a cached connection id for `scope`), then send one request.
    async fn udp_exchange<T>(
        &self,
        scope: ConnectionScope,
        encode: impl Fn(u64, u32) -> Vec<u8>,
        decode: impl Fn(&[u8], u32, PeerFamily) -> Result<T, TrackerError>,
    ) -> Result<T, TrackerError> {
        let addr = scope.0;
        let bind: SocketAddr = if addr.is_ipv4() {
            (Ipv4Addr::UNSPECIFIED, 0).into()
        } else {
            (Ipv6Addr::UNSPECIFIED, 0).into()
        };
        let socket = UdpSocket::bind(bind).await.map_err(TrackerError::network)?;
        // A connected socket only receives datagrams from the tracker's address.
        socket.connect(addr).await.map_err(TrackerError::network)?;
        let path = UdpPath::Direct(&socket, PeerFamily::of(&addr));
        let mut buf = vec![0u8; udp::MAX_DATAGRAM];

        let mut used_cached = self.cached_connection(&scope);
        loop {
            let connection_id = if let Some(id) = used_cached {
                id
            } else {
                let tx = rand::random::<u32>();
                let id = self
                    .transact(&path, &udp::encode_connect(tx), tx, &mut buf, |b, _| {
                        udp::decode_connect(b, tx)
                    })
                    .await?;
                self.connections()
                    .insert(scope, id, Instant::now().into_std());
                id
            };

            let tx = rand::random::<u32>();
            let packet = encode(connection_id, tx);
            match self
                .transact(&path, &packet, tx, &mut buf, |b, family| {
                    decode(b, tx, family)
                })
                .await
            {
                Ok(v) => return Ok(v),
                Err(e @ (TrackerError::Failure(_) | TrackerError::Timeout)) => {
                    // The tracker may have restarted and forgotten our connection id; many
                    // report that as an error packet. Retry once with a fresh one.
                    self.connections().remove(&scope);
                    if used_cached.take().is_some() && matches!(e, TrackerError::Failure(_)) {
                        continue;
                    }
                    return Err(e);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Send `packet` and wait for the response with transaction id `tx`, retransmitting on
    /// the configured schedule. The same transaction id is kept across retransmissions, so
    /// a late answer to an earlier copy still counts. `decode` gets the family of the
    /// address the answer came from, which says how its peers are encoded.
    async fn transact<T>(
        &self,
        path: &UdpPath<'_>,
        packet: &[u8],
        tx: u32,
        buf: &mut [u8],
        decode: impl Fn(&[u8], PeerFamily) -> Result<T, TrackerError>,
    ) -> Result<T, TrackerError> {
        let policy = self.inner.config.udp_retry;
        for attempt in 0..policy.attempts.max(1) {
            path.send(packet).await.map_err(TrackerError::network)?;
            let deadline = Instant::now() + policy.timeout(attempt);
            loop {
                let Ok(received) = tokio::time::timeout_at(deadline, path.recv(buf)).await else {
                    break;
                };
                let (n, family) = received.map_err(TrackerError::network)?;
                let datagram = &buf[..n];
                match udp::response_header(datagram) {
                    Some((_, got)) if got == tx => return decode(datagram, family),
                    // Not an answer to this request: stale, or forged.
                    _ => {}
                }
            }
        }
        Err(TrackerError::Timeout)
    }
}

/// Where UDP tracker datagrams go: a socket connected to the tracker, or a SOCKS5 UDP
/// association with the tracker's address.
enum UdpPath<'a> {
    Direct(&'a UdpSocket, PeerFamily),
    Proxied(&'a UdpAssociation, &'a Target),
}

impl UdpPath<'_> {
    async fn send(&self, packet: &[u8]) -> std::io::Result<()> {
        match self {
            UdpPath::Direct(socket, _) => socket.send(packet).await.map(drop),
            UdpPath::Proxied(association, target) => association.send_to(packet, target).await,
        }
    }

    /// The next datagram's length, and the family of the address it came from.
    async fn recv(&self, buf: &mut [u8]) -> std::io::Result<(usize, PeerFamily)> {
        match self {
            UdpPath::Direct(socket, family) => Ok((socket.recv(buf).await?, *family)),
            UdpPath::Proxied(association, _) => loop {
                // The relay names the sender; a name (which no relay sends back) can't
                // say how peers are encoded, so such a datagram is skipped.
                if let (n, Target::Addr(from)) = association.recv_from(buf).await? {
                    return Ok((n, PeerFamily::of(&from)));
                }
            },
        }
    }
}

/// Turn an HTTP status and body into a result. Trackers often send a bencoded
/// `failure reason` with an error status; that is more useful than the status alone.
fn interpret<T>(
    status: StatusCode,
    body: &[u8],
    parse: impl Fn(&[u8]) -> Result<T, TrackerError>,
) -> Result<T, TrackerError> {
    match parse(body) {
        Ok(v) if status.is_success() => Ok(v),
        Err(e) if status.is_success() || e.is_tracker_failure() => Err(e),
        _ => Err(TrackerError::HttpStatus(status.as_u16())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_schemes() {
        assert!(matches!(
            parse_url("wss://t.example/announce"),
            Err(TrackerError::UnsupportedScheme(s)) if s == "wss"
        ));
        assert!(matches!(
            parse_url("not a url"),
            Err(TrackerError::InvalidUrl(_))
        ));
        assert!(parse_url("udp://t.example:80").is_ok());
        assert!(parse_url("HTTPS://t.example/announce").is_ok());
    }

    #[test]
    fn interprets_status() {
        let ok = |b: &[u8]| http::parse_announce(b);
        let fail = b"d14:failure reason4:nopee";
        assert!(matches!(
            interpret(StatusCode::FORBIDDEN, fail, ok),
            Err(TrackerError::Failure(_))
        ));
        assert!(matches!(
            interpret(StatusCode::BAD_GATEWAY, b"<html>", ok),
            Err(TrackerError::HttpStatus(502))
        ));
        assert!(matches!(
            interpret(StatusCode::OK, b"<html>", ok),
            Err(TrackerError::Malformed(_))
        ));
        assert!(matches!(
            interpret(StatusCode::NOT_FOUND, b"d8:intervali1ee", ok),
            Err(TrackerError::HttpStatus(404))
        ));
    }

    #[test]
    fn client_builds_and_clones() {
        let client = TrackerClient::new().expect("client builds");
        let clone = client.clone();
        assert!(Arc::ptr_eq(&client.inner, &clone.inner));
        assert_eq!(client.config().max_response_size, 2 * 1024 * 1024);
    }
}

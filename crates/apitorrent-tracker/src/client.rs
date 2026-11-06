use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use apitorrent_metainfo::Sha1Hash;
use reqwest::StatusCode;
use reqwest::redirect;
use tokio::net::UdpSocket;
use tokio::time::Instant;
use url::{Host, Url};

use crate::error::TrackerError;
use crate::types::{AnnounceRequest, AnnounceResponse, ScrapeResponse};
use crate::udp::{self, ConnectionCache, PeerFamily, RetryPolicy};
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
    http: reqwest::Client,
    config: TrackerConfig,
    connections: Mutex<ConnectionCache>,
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

impl TrackerClient {
    /// A client with the default [`TrackerConfig`].
    pub fn new() -> Result<Self, TrackerError> {
        Self::with_config(TrackerConfig::default())
    }

    pub fn with_config(config: TrackerConfig) -> Result<Self, TrackerError> {
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured(tls::client_config()?)
            .redirect(redirect_policy(config.max_redirects))
            // The announce URL can hold a private tracker's passkey; don't hand it to
            // wherever a redirect points.
            .referer(false)
            // Proxies will be explicit settings, never picked up from the environment.
            .no_proxy()
            .user_agent(config.user_agent.clone())
            .timeout(config.http_timeout)
            .connect_timeout(config.connect_timeout)
            .build()
            .map_err(TrackerError::network)?;
        Ok(Self {
            inner: Arc::new(Inner {
                http,
                config,
                connections: Mutex::new(ConnectionCache::default()),
            }),
        })
    }

    pub fn config(&self) -> &TrackerConfig {
        &self.inner.config
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
                let (status, body) = self.http_get(&http::announce_url(url, req)).await?;
                interpret(status, &body, http::parse_announce)
            }
            Scheme::Udp => {
                let addr = self.resolve_udp(&parsed).await?;
                let family = PeerFamily::of(&addr);
                self.udp_exchange(
                    addr,
                    |conn, tx| udp::encode_announce(conn, tx, req).to_vec(),
                    |buf, tx| udp::decode_announce(buf, tx, family),
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
        http::check_scrape_hashes(hashes)?;
        let (parsed, scheme) = parse_url(url)?;
        match scheme {
            Scheme::Http => {
                let (status, body) = self.http_get(&http::scrape_url(url, hashes)?).await?;
                interpret(status, &body, |b| http::parse_scrape(b, hashes))
            }
            Scheme::Udp => {
                let addr = self.resolve_udp(&parsed).await?;
                self.udp_exchange(
                    addr,
                    |conn, tx| udp::encode_scrape(conn, tx, hashes).expect("hashes checked"),
                    |buf, tx| udp::decode_scrape(buf, tx, hashes),
                )
                .await
            }
        }
    }

    /// GET `url` and read the body, up to the size limit.
    async fn http_get(&self, url: &str) -> Result<(StatusCode, Vec<u8>), TrackerError> {
        let limit = self.inner.config.max_response_size;
        let mut resp = self.inner.http.get(url).send().await?;
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

    fn cached_connection(&self, addr: &SocketAddr) -> Option<u64> {
        self.connections().get(addr, Instant::now().into_std())
    }

    fn connections(&self) -> std::sync::MutexGuard<'_, ConnectionCache> {
        self.inner
            .connections
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Connect (or reuse a cached connection id), then send one request.
    async fn udp_exchange<T>(
        &self,
        addr: SocketAddr,
        encode: impl Fn(u64, u32) -> Vec<u8>,
        decode: impl Fn(&[u8], u32) -> Result<T, TrackerError>,
    ) -> Result<T, TrackerError> {
        let bind: SocketAddr = if addr.is_ipv4() {
            (Ipv4Addr::UNSPECIFIED, 0).into()
        } else {
            (Ipv6Addr::UNSPECIFIED, 0).into()
        };
        let socket = UdpSocket::bind(bind).await.map_err(TrackerError::network)?;
        // A connected socket only receives datagrams from the tracker's address.
        socket.connect(addr).await.map_err(TrackerError::network)?;
        let mut buf = vec![0u8; udp::MAX_DATAGRAM];

        let mut used_cached = self.cached_connection(&addr);
        loop {
            let connection_id = if let Some(id) = used_cached {
                id
            } else {
                let tx = rand::random::<u32>();
                let id = self
                    .transact(&socket, &udp::encode_connect(tx), tx, &mut buf, |b| {
                        udp::decode_connect(b, tx)
                    })
                    .await?;
                self.connections()
                    .insert(addr, id, Instant::now().into_std());
                id
            };

            let tx = rand::random::<u32>();
            let packet = encode(connection_id, tx);
            match self
                .transact(&socket, &packet, tx, &mut buf, |b| decode(b, tx))
                .await
            {
                Ok(v) => return Ok(v),
                Err(e @ (TrackerError::Failure(_) | TrackerError::Timeout)) => {
                    // The tracker may have restarted and forgotten our connection id; many
                    // report that as an error packet. Retry once with a fresh one.
                    self.connections().remove(&addr);
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
    /// a late answer to an earlier copy still counts.
    async fn transact<T>(
        &self,
        socket: &UdpSocket,
        packet: &[u8],
        tx: u32,
        buf: &mut [u8],
        decode: impl Fn(&[u8]) -> Result<T, TrackerError>,
    ) -> Result<T, TrackerError> {
        let policy = self.inner.config.udp_retry;
        for attempt in 0..policy.attempts.max(1) {
            socket.send(packet).await.map_err(TrackerError::network)?;
            let deadline = Instant::now() + policy.timeout(attempt);
            loop {
                let Ok(received) = tokio::time::timeout_at(deadline, socket.recv(buf)).await else {
                    break;
                };
                let n = received.map_err(TrackerError::network)?;
                let datagram = &buf[..n];
                match udp::response_header(datagram) {
                    Some((_, got)) if got == tx => return decode(datagram),
                    // Not an answer to this request: stale, or forged.
                    _ => {}
                }
            }
        }
        Err(TrackerError::Timeout)
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

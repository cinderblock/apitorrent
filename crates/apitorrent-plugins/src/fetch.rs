//! `fetch` for plugins, checked by the host.
//!
//! - **Grants.** Every URL, including every redirect hop, must match one of the plugin's
//!   network grants (see `apitorrent_plugin_api::network`). The host follows redirects
//!   itself so it can check each hop.
//! - **Addresses.** A granted host *name* must resolve to public addresses; loopback,
//!   private and link-local answers are dropped (DNS rebinding). A grant that names an IP
//!   address or `localhost` is how a plugin reaches local services.
//! - **TLS** is the tracker crate's strict configuration: OS trust store, no exceptions.
//! - No environment proxies, no `Referer`, no downgrade from HTTPS to HTTP on redirect, and
//!   `Authorization` is dropped when a redirect leaves the origin.
//! - Request and response bodies are capped.

use std::net::SocketAddr;
use std::time::Duration;

use apitorrent_tracker::reqwest;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use serde::{Deserialize, Serialize};
use url::Url;

use apitorrent_plugin_api::network::{NetworkGrants, is_public};

/// Limits for plugin HTTP requests.
#[derive(Debug, Clone)]
pub struct FetchConfig {
    pub user_agent: String,
    /// The whole request, redirects included.
    pub timeout: Duration,
    pub connect_timeout: Duration,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub max_redirects: usize,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            user_agent: format!("apitorrent/{} (plugin)", env!("CARGO_PKG_VERSION")),
            timeout: Duration::from_secs(120),
            connect_timeout: Duration::from_secs(15),
            max_request_bytes: 8 * 1024 * 1024,
            max_response_bytes: 32 * 1024 * 1024,
            max_redirects: 10,
        }
    }
}

/// Request headers a plugin may not set: the host manages them, or they would let a
/// plugin smuggle a second request or pose as a proxy.
const FORBIDDEN_HEADERS: &[&str] = &[
    "connection",
    "content-length",
    "host",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum RedirectMode {
    #[default]
    Follow,
    Error,
    Manual,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FetchArgs {
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub redirect: RedirectMode,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FetchMeta {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub url: String,
    pub redirected: bool,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum FetchError {
    /// The plugin wasn't granted this URL.
    #[error("{0}")]
    NotCapable(String),
    /// What `fetch` reports as a `TypeError`: bad input, network failures, limits.
    #[error("{0}")]
    Failed(String),
}

#[derive(Debug)]
pub(crate) struct Fetcher {
    client: reqwest::Client,
    config: FetchConfig,
}

/// Resolves names with the system resolver, keeping only public addresses (except for
/// `localhost`, which only a grant naming it can reach).
#[derive(Debug)]
struct PublicOnly;

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let found: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let kept = keep_reachable(&host, found);
            if kept.is_empty() {
                return Err(format!(
                    "{host} resolves only to local or private addresses; a plugin reaches \
                     those only through a grant that names the IP address"
                )
                .into());
            }
            Ok(Box::new(kept.into_iter()) as Addrs)
        })
    }
}

fn keep_reachable(host: &str, addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") {
        return addrs.into_iter().filter(|a| a.ip().is_loopback()).collect();
    }
    addrs.into_iter().filter(|a| is_public(a.ip())).collect()
}

fn origin(u: &Url) -> String {
    u.origin().ascii_serialization()
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

impl Fetcher {
    pub fn new(config: FetchConfig) -> Result<Self, String> {
        let client = apitorrent_tracker::strict_http_client_builder(
            &config.user_agent,
            config.timeout,
            config.connect_timeout,
        )
        .map_err(|e| e.to_string())?
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(std::sync::Arc::new(PublicOnly))
        .build()
        .map_err(|e| e.to_string())?;
        Ok(Self { client, config })
    }

    /// Run one `fetch`. The body arrives whole; responses over the size limit fail.
    pub async fn fetch(
        &self,
        grants: &NetworkGrants,
        args: FetchArgs,
        body: Option<Vec<u8>>,
    ) -> Result<(FetchMeta, Vec<u8>), FetchError> {
        let deadline = tokio::time::Instant::now() + self.config.timeout;
        tokio::time::timeout_at(deadline, self.fetch_inner(grants, args, body))
            .await
            .map_err(|_| {
                FetchError::Failed(format!(
                    "the request took longer than {} s",
                    self.config.timeout.as_secs()
                ))
            })?
    }

    async fn fetch_inner(
        &self,
        grants: &NetworkGrants,
        args: FetchArgs,
        mut body: Option<Vec<u8>>,
    ) -> Result<(FetchMeta, Vec<u8>), FetchError> {
        let mut url = Url::parse(&args.url)
            .map_err(|e| FetchError::Failed(format!("invalid URL {}: {e}", args.url)))?;
        let mut method =
            reqwest::Method::from_bytes(args.method.to_ascii_uppercase().as_bytes())
                .map_err(|_| FetchError::Failed(format!("invalid method {}", args.method)))?;
        if matches!(method.as_str(), "CONNECT" | "TRACE" | "TRACK") {
            return Err(FetchError::Failed(format!(
                "method {method} is not allowed"
            )));
        }
        if body
            .as_ref()
            .is_some_and(|b| b.len() > self.config.max_request_bytes)
        {
            return Err(FetchError::Failed(format!(
                "the request body is over {} bytes",
                self.config.max_request_bytes
            )));
        }
        let mut headers = reqwest::header::HeaderMap::new();
        for (k, v) in &args.headers {
            if FORBIDDEN_HEADERS.contains(&k.to_ascii_lowercase().as_str()) {
                continue;
            }
            let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|_| FetchError::Failed(format!("invalid header name {k:?}")))?;
            let value = reqwest::header::HeaderValue::from_str(v)
                .map_err(|_| FetchError::Failed(format!("invalid value for header {k}")))?;
            headers.append(name, value);
        }
        let mut redirected = false;
        let mut hops = 0;
        loop {
            if !matches!(url.scheme(), "http" | "https") {
                return Err(FetchError::Failed(format!(
                    "only http and https URLs can be fetched, not {}",
                    url.scheme()
                )));
            }
            if !grants.allows(&url) {
                return Err(FetchError::NotCapable(format!(
                    "fetch to {} is not allowed: the plugin has no network grant for it",
                    origin(&url)
                )));
            }
            let mut req = self
                .client
                .request(method.clone(), url.clone())
                .headers(headers.clone());
            if let Some(b) = &body {
                req = req.body(b.clone());
            }
            let resp = req
                .send()
                .await
                .map_err(|e| FetchError::Failed(describe(&e)))?;
            let status = resp.status().as_u16();
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            if is_redirect(status)
                && let Some(location) = location
                && args.redirect != RedirectMode::Manual
            {
                if args.redirect == RedirectMode::Error {
                    return Err(FetchError::Failed(format!(
                        "{url} redirected, and the request said redirect: \"error\""
                    )));
                }
                hops += 1;
                if hops > self.config.max_redirects {
                    return Err(FetchError::Failed("too many redirects".to_owned()));
                }
                let next = url.join(&location).map_err(|e| {
                    FetchError::Failed(format!("bad redirect location {location:?}: {e}"))
                })?;
                if url.scheme() == "https" && next.scheme() == "http" {
                    return Err(FetchError::Failed(format!(
                        "refusing a redirect from HTTPS to plain HTTP ({next})"
                    )));
                }
                // The method change browsers make (fetch spec, "HTTP-redirect fetch").
                let to_get = status == 303 && method != reqwest::Method::HEAD
                    || matches!(status, 301 | 302) && method == reqwest::Method::POST;
                if to_get {
                    method = reqwest::Method::GET;
                    body = None;
                    for h in [
                        "content-type",
                        "content-encoding",
                        "content-language",
                        "content-location",
                    ] {
                        headers.remove(h);
                    }
                }
                if origin(&next) != origin(&url) {
                    headers.remove(reqwest::header::AUTHORIZATION);
                    headers.remove(reqwest::header::COOKIE);
                }
                url = next;
                redirected = true;
                continue;
            }
            let meta = FetchMeta {
                status,
                status_text: resp
                    .status()
                    .canonical_reason()
                    .unwrap_or_default()
                    .to_owned(),
                headers: resp
                    .headers()
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.as_str().to_owned(),
                            String::from_utf8_lossy(v.as_bytes()).into_owned(),
                        )
                    })
                    .collect(),
                url: url.to_string(),
                redirected,
            };
            let body = read_capped(resp, self.config.max_response_bytes).await?;
            return Ok((meta, body));
        }
    }
}

async fn read_capped(mut resp: reqwest::Response, max: usize) -> Result<Vec<u8>, FetchError> {
    if resp
        .content_length()
        .is_some_and(|n| usize::try_from(n).unwrap_or(usize::MAX) > max)
    {
        return Err(FetchError::Failed(format!(
            "the response is over the {max}-byte limit"
        )));
    }
    let mut out = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| FetchError::Failed(describe(&e)))?
    {
        if out.len() + chunk.len() > max {
            return Err(FetchError::Failed(format!(
                "the response is over the {max}-byte limit"
            )));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// reqwest's top-level message is vague ("error sending request"); the cause chain says
/// what happened (a certificate error, a refused connection, a resolver refusal).
fn describe(e: &reqwest::Error) -> String {
    use std::error::Error as _;
    let mut msg = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        let s_text = s.to_string();
        if !msg.contains(&s_text) {
            msg.push_str(": ");
            msg.push_str(&s_text);
        }
        source = s.source();
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolver_keeps_only_reachable_addresses() {
        let addrs: Vec<SocketAddr> = ["127.0.0.1:0", "10.0.0.1:0", "93.184.215.14:0", "[::1]:0"]
            .iter()
            .map(|a| a.parse().unwrap())
            .collect();
        assert_eq!(
            keep_reachable("feeds.example.com", addrs.clone()),
            ["93.184.215.14:0".parse::<SocketAddr>().unwrap()]
        );
        assert_eq!(keep_reachable("localhost", addrs.clone()).len(), 2);
        assert_eq!(keep_reachable("app.localhost.", addrs).len(), 2);
        // A name that only resolves to the LAN yields nothing.
        assert!(keep_reachable("nas.example", vec!["192.168.1.2:0".parse().unwrap()]).is_empty());
    }
}

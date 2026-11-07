//! Host and Origin checks (see the crate docs).

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::authn::is_websocket;
use crate::routes::error;

#[derive(Debug, Clone)]
pub(crate) struct Guard {
    hosts: Arc<Vec<String>>,
}

impl Guard {
    pub(crate) fn new(public_hosts: &[String]) -> Self {
        let mut hosts = vec![
            "localhost".to_owned(),
            "127.0.0.1".to_owned(),
            "[::1]".to_owned(),
        ];
        hosts.extend(public_hosts.iter().map(|h| h.to_ascii_lowercase()));
        Self {
            hosts: Arc::new(hosts),
        }
    }

    /// Whether `authority` (`host` or `host:port`) names this server.
    fn allows_authority(&self, authority: &str) -> bool {
        let authority = authority.to_ascii_lowercase();
        let host = strip_port(&authority);
        self.hosts.iter().any(|h| h == host)
    }

    fn allows_origin(&self, origin: &str) -> bool {
        origin_authority(origin).is_some_and(|a| self.allows_authority(&a))
    }

    /// The page's origin is this very server: an allowed host, and the same host and port
    /// the request was sent to. Other ports on the same host are other sites' pages (and
    /// count as `same-site` to browsers, so `SameSite` cookies alone don't stop them).
    fn same_origin(&self, origin: &str, host: &str) -> bool {
        self.allows_origin(origin)
            && origin_authority(origin).is_some_and(|a| a == normalize_host(host))
    }
}

/// `scheme://authority` → the authority, lowercased, without the scheme's default port.
fn origin_authority(origin: &str) -> Option<String> {
    let (scheme, rest) = origin.split_once("://")?;
    let default = match scheme.to_ascii_lowercase().as_str() {
        "http" => ":80",
        "https" => ":443",
        _ => return None,
    };
    let authority = rest.trim_end_matches('/').to_ascii_lowercase();
    if authority.is_empty() || authority.contains('/') {
        return None;
    }
    Some(
        authority
            .strip_suffix(default)
            .map_or(authority.clone(), str::to_owned),
    )
}

/// A `Host` header, lowercased, without a default port.
fn normalize_host(host: &str) -> String {
    let h = host.trim().to_ascii_lowercase();
    for default in [":80", ":443"] {
        if let Some(stripped) = h.strip_suffix(default) {
            return stripped.to_owned();
        }
    }
    h
}

/// Remove a trailing `:port`, leaving bracketed IPv6 literals intact.
fn strip_port(authority: &str) -> &str {
    if authority.starts_with('[') {
        return authority
            .find(']')
            .map_or(authority, |end| &authority[..=end]);
    }
    authority.split(':').next().unwrap_or(authority)
}

pub(crate) async fn check(State(guard): State<Guard>, req: Request, next: Next) -> Response {
    let headers = req.headers();
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            req.uri()
                .authority()
                .map(axum::http::uri::Authority::as_str)
        });
    if !host.is_some_and(|h| guard.allows_authority(h)) {
        return error(
            StatusCode::MISDIRECTED_REQUEST,
            "unknown_host",
            "this server does not answer to that host name",
        )
        .into_response();
    }

    let host = host.unwrap_or_default();
    // Browsers never attach an `Authorization` header to a cross-site request by themselves
    // (it would need a CORS preflight, which this server never grants), so bearer requests
    // can't be forged by another site and skip the CSRF checks.
    let bearer = headers.contains_key(header::AUTHORIZATION);
    let changes_state = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if !bearer && (changes_state || is_websocket(headers)) {
        if let Some(origin) = headers.get(header::ORIGIN) {
            let ok = origin.to_str().is_ok_and(|o| guard.same_origin(o, host));
            if !ok {
                return error(
                    StatusCode::FORBIDDEN,
                    "cross_origin",
                    "requests from other sites are not allowed",
                )
                .into_response();
            }
        }
        let cross_site = headers
            .get("sec-fetch-site")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == "cross-site" || v == "same-site");
        if cross_site {
            return error(
                StatusCode::FORBIDDEN,
                "cross_origin",
                "requests from other sites are not allowed",
            )
            .into_response();
        }
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_and_origins() {
        let g = Guard::new(&["Torrents.Example.com".to_owned()]);
        assert!(g.allows_authority("localhost:8742"));
        assert!(g.allows_authority("127.0.0.1"));
        assert!(g.allows_authority("[::1]:8742"));
        assert!(g.allows_authority("torrents.example.com"));
        assert!(!g.allows_authority("evil.example.com"));
        assert!(!g.allows_authority("localhost.evil.com"));
        assert!(!g.allows_authority("127.0.0.2"));

        assert!(g.allows_origin("http://localhost:5173"));
        assert!(g.allows_origin("https://torrents.example.com"));
        assert!(!g.allows_origin("https://evil.example.com"));
        assert!(!g.allows_origin("null"));
        assert!(!g.allows_origin("file:///x"));

        assert!(g.same_origin("http://localhost:8742", "localhost:8742"));
        assert!(g.same_origin("https://torrents.example.com", "Torrents.Example.com"));
        assert!(g.same_origin("https://torrents.example.com:443", "torrents.example.com"));
        // Another port on the same host is another site's page.
        assert!(!g.same_origin("http://localhost:5173", "localhost:8742"));
        assert!(!g.same_origin("http://localhost", "localhost:8742"));
        assert!(!g.same_origin("https://evil.example.com", "evil.example.com"));
    }
}

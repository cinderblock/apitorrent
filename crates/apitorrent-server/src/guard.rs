//! Host and Origin checks (see the crate docs).

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

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
        let Some((scheme, rest)) = origin.split_once("://") else {
            return false;
        };
        if scheme != "http" && scheme != "https" {
            return false;
        }
        self.allows_authority(rest.trim_end_matches('/'))
    }
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

fn is_websocket(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
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

    let changes_state = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if changes_state || is_websocket(headers) {
        if let Some(origin) = headers.get(header::ORIGIN) {
            let ok = origin.to_str().is_ok_and(|o| guard.allows_origin(o));
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
            .is_some_and(|v| v == "cross-site");
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
    }
}

//! Authentication for `/api/v1`: who is calling, from where, and whether they may.
//!
//! Every API request gets a [`RequestCtx`]. The caller is identified, in order, by:
//!
//! 1. `Authorization: Bearer <token>`: an API token. A bad token is a 401; there is no
//!    fallback to the cookie. (The guard skips CSRF checks for these requests: browsers
//!    never attach an `Authorization` header to a cross-site request on their own.)
//! 2. With `--insecure-no-auth-loopback`: a direct loopback request is the local admin.
//! 3. The session cookie.
//!
//! Everything except [`PUBLIC`] needs a caller. Writes (anything but `GET`/`HEAD`) need one
//! that may write: not a `read` token and not a viewer. Under `/auth/`, the auth crate makes
//! its own, finer decisions.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use apitorrent_auth::{ClientMeta, Principal};

use crate::routes::error;
use crate::{AppState, AuthMode, TrustedProxies};

/// The session cookie over HTTPS. `__Host-` makes browsers insist on `Secure`, `Path=/` and
/// no `Domain`, so no other host or plain-HTTP page can set or shadow it.
pub(crate) const SECURE_COOKIE: &str = "__Host-apitorrent-session";
/// The session cookie over plain HTTP (loopback, or a LAN with `--allow-insecure-lan`).
/// Browsers only accept `__Host-` cookies with `Secure`, and Safari refuses `Secure` cookies
/// on `http://localhost`, so plain HTTP uses an unprefixed, non-`Secure` name instead. Each
/// transport reads only its own name, so neither can stand in for the other.
pub(crate) const PLAIN_COOKIE: &str = "apitorrent-session";

/// Endpoints that work without signing in.
const PUBLIC: &[&str] = &[
    "/info",
    "/auth/status",
    "/auth/setup/start",
    "/auth/setup/finish",
    "/auth/setup/password",
    "/auth/login/passkey/start",
    "/auth/login/passkey/finish",
    "/auth/login/password",
    "/auth/logout",
];

/// Endpoints a session waiting for its second factor may use.
const PENDING_OK: &[&str] = &["/auth/status", "/auth/login/password", "/auth/logout"];

#[derive(Debug, Clone)]
pub(crate) struct RequestCtx {
    pub meta: ClientMeta,
    /// The `Host` header (already checked by the guard).
    pub host: String,
    /// Reached us over HTTPS: built-in TLS, or a trusted proxy that says so.
    pub secure: bool,
    pub principal: Option<Principal>,
}

impl RequestCtx {
    pub(crate) fn cookie_name(&self) -> &'static str {
        if self.secure {
            SECURE_COOKIE
        } else {
            PLAIN_COOKIE
        }
    }

    /// A `Set-Cookie` value for a new session.
    pub(crate) fn session_cookie(&self, secret: &str, max_age_secs: u64) -> HeaderValue {
        let secure = if self.secure { "; Secure" } else { "" };
        HeaderValue::from_str(&format!(
            "{}={secret}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age_secs}{secure}",
            self.cookie_name()
        ))
        .expect("cookie values are ASCII")
    }

    pub(crate) fn clear_cookie(&self) -> HeaderValue {
        let secure = if self.secure { "; Secure" } else { "" };
        HeaderValue::from_str(&format!(
            "{}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{secure}",
            self.cookie_name()
        ))
        .expect("cookie values are ASCII")
    }
}

/// `localhost`, `127.0.0.1`, `[::1]` and the like, with or without a port.
pub(crate) fn is_loopback_host(host: &str) -> bool {
    let h = apitorrent_auth::host_without_port(host);
    let h = h.trim_start_matches('[').trim_end_matches(']');
    h == "localhost"
        || h.ends_with(".localhost")
        || h.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn peer(req: &Request) -> Option<IpAddr> {
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip())
}

/// The client's address: the peer, or for a trusted proxy the right-most untrusted address
/// in `X-Forwarded-For` (the left-most entries are whatever the client claimed).
fn client_ip(
    peer: Option<IpAddr>,
    headers: &HeaderMap,
    proxies: &TrustedProxies,
) -> Option<IpAddr> {
    let peer = peer?;
    if !proxies.contains(peer) {
        return Some(peer);
    }
    let mut chain: Vec<IpAddr> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    while let Some(ip) = chain.pop() {
        if !proxies.contains(ip) {
            return Some(ip);
        }
    }
    Some(peer)
}

fn forwarded_https(peer: Option<IpAddr>, headers: &HeaderMap, proxies: &TrustedProxies) -> bool {
    peer.is_some_and(|p| proxies.contains(p))
        && headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("https"))
}

fn looks_proxied(headers: &HeaderMap) -> bool {
    ["x-forwarded-for", "forwarded", "x-real-ip"]
        .iter()
        .any(|h| headers.contains_key(*h))
}

/// The value of cookie `name`, if present. Only the first occurrence counts.
pub(crate) fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.trim())
}

fn bearer(headers: &HeaderMap) -> Option<Result<&str, ()>> {
    let v = headers.get(header::AUTHORIZATION)?;
    let Ok(v) = v.to_str() else {
        return Some(Err(()));
    };
    let (scheme, token) = v.split_once(' ').unwrap_or((v, ""));
    if scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty() {
        Some(Ok(token.trim()))
    } else {
        Some(Err(()))
    }
}

fn unauthorized(code: &str, message: &str) -> Response {
    let mut r = error(StatusCode::UNAUTHORIZED, code, message).into_response();
    r.headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    r
}

/// A WebSocket handshake: HTTP/1.1 `Upgrade: websocket`, or an HTTP/2 extended CONNECT
/// (RFC 8441), which carries no `Upgrade` header. The API serves CONNECT only for the sync
/// socket, so any CONNECT is treated as one.
pub(crate) fn is_websocket(method: &Method, headers: &HeaderMap) -> bool {
    *method == Method::CONNECT
        || headers
            .get(header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

pub(crate) async fn layer(State(s): State<AppState>, mut req: Request, next: Next) -> Response {
    let headers = req.headers();
    let peer = peer(&req);
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            req.uri()
                .authority()
                .map(axum::http::uri::Authority::as_str)
        })
        .unwrap_or_default()
        .to_owned();
    let mut ctx = RequestCtx {
        meta: ClientMeta {
            ip: client_ip(peer, headers, &s.proxies),
            user_agent: headers
                .get(header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
        },
        // Built-in TLS, or a trusted proxy that terminated it.
        secure: s.tls.is_some() || forwarded_https(peer, headers, &s.proxies),
        host,
        principal: None,
    };
    let mut stale_cookie = false;
    let mut via_cookie = false;

    match (&s.auth, bearer(headers)) {
        (_, Some(Err(()))) => {
            return unauthorized("invalid_token", "use `Authorization: Bearer <token>`");
        }
        (AuthMode::Enabled(auth), Some(Ok(token))) => match auth.authenticate_token(token) {
            Ok(Some(p)) => ctx.principal = Some(p),
            Ok(None) => return unauthorized("invalid_token", "unknown, expired or revoked token"),
            Err(e) => return crate::auth_routes::Failure(e).into_response(),
        },
        (AuthMode::DisabledOnLoopback, _) => {
            // A local reverse proxy would make remote clients look like loopback peers, so
            // proxied requests never get the free pass.
            let direct = peer.is_some_and(|p| p.is_loopback())
                && is_loopback_host(&ctx.host)
                && !looks_proxied(headers);
            if direct {
                ctx.principal = Some(Principal::local());
            }
        }
        (AuthMode::Enabled(auth), None) => {
            if let Some(secret) = cookie(headers, ctx.cookie_name()) {
                match auth.authenticate_session(secret) {
                    Ok(Some(p)) => {
                        ctx.principal = Some(p);
                        via_cookie = true;
                    }
                    Ok(None) => stale_cookie = true,
                    Err(e) => return crate::auth_routes::Failure(e).into_response(),
                }
            }
        }
    }

    let full = req.uri().path();
    let path = full.strip_prefix("/api/v1").unwrap_or(full);
    let public = PUBLIC.contains(&path);
    let decision = match &ctx.principal {
        None if public => Ok(()),
        None => Err(unauthorized("unauthenticated", "sign in first")),
        Some(p) if p.pending_second_factor() && !PENDING_OK.contains(&path) && !public => {
            Err(error(
                StatusCode::FORBIDDEN,
                "second_factor_required",
                "enter your password to finish signing in",
            )
            .into_response())
        }
        Some(p) => {
            // Opening the sync socket is a read, whether by GET (HTTP/1.1) or CONNECT (h2).
            let reads = matches!(*req.method(), Method::GET | Method::HEAD | Method::CONNECT);
            if path.starts_with("/auth/") || (reads && p.may_read()) || (!reads && p.may_write()) {
                Ok(())
            } else {
                Err(error(
                    StatusCode::FORBIDDEN,
                    "read_only",
                    "this credential may only read",
                )
                .into_response())
            }
        }
    };
    // A browser always sends `Origin` on a WebSocket handshake; one riding on a cookie
    // without it isn't a page of ours.
    if via_cookie && is_websocket(req.method(), headers) && !headers.contains_key(header::ORIGIN) {
        return error(
            StatusCode::FORBIDDEN,
            "cross_origin",
            "WebSocket requests need an Origin header",
        )
        .into_response();
    }
    let clear = stale_cookie.then(|| ctx.clear_cookie());
    let mut resp = match decision {
        Ok(()) => {
            req.extensions_mut().insert(ctx);
            next.run(req).await
        }
        Err(r) => r,
    };
    if let Some(clear) = clear
        && !resp.headers().contains_key(header::SET_COOKIE)
    {
        resp.headers_mut().insert(header::SET_COOKIE, clear);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_ip_through_proxies() {
        let proxies = TrustedProxies::parse(&["10.0.0.0/8".into()]).unwrap();
        let mut h = HeaderMap::new();
        h.insert(
            "x-forwarded-for",
            HeaderValue::from_static("1.1.1.1, 203.0.113.9, 10.0.0.2"),
        );
        let proxy: IpAddr = "10.0.0.1".parse().unwrap();
        assert_eq!(
            client_ip(Some(proxy), &h, &proxies),
            Some("203.0.113.9".parse().unwrap())
        );
        // An untrusted peer's header is ignored.
        let other: IpAddr = "192.0.2.5".parse().unwrap();
        assert_eq!(client_ip(Some(other), &h, &proxies), Some(other));
        h.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert!(forwarded_https(Some(proxy), &h, &proxies));
        assert!(!forwarded_https(Some(other), &h, &proxies));
    }

    #[test]
    fn cookies_and_bearer() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; apitorrent-session=xyz; b=2"),
        );
        assert_eq!(cookie(&h, PLAIN_COOKIE), Some("xyz"));
        assert_eq!(cookie(&h, SECURE_COOKIE), None);
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer apt_x"),
        );
        assert_eq!(bearer(&h), Some(Ok("apt_x")));
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Basic Zm9v"),
        );
        assert_eq!(bearer(&h), Some(Err(())));
    }

    #[test]
    fn loopback_hosts() {
        for h in [
            "localhost",
            "localhost:8742",
            "127.0.0.1:1",
            "[::1]:8742",
            "app.localhost",
        ] {
            assert!(is_loopback_host(h), "{h}");
        }
        for h in ["example.com", "192.168.1.2", "localhost.evil.com"] {
            assert!(!is_loopback_host(h), "{h}");
        }
    }
}

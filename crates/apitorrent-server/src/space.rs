//! Which space a request is in, and what the caller may do there.
//!
//! The space comes from, in order:
//!
//! 1. **The API token's bound space.** A URL naming another space is refused (403).
//! 2. **The path prefix `/s/<slug>/`**, for the web UI, the API and plugin routes (the
//!    qBittorrent API's clients take it as their "URL base"). [`strip_prefix`] removes it
//!    before routing and keeps the slug as [`SpacePrefix`].
//! 3. **A host name mapped to a space** (`--space-host`), optionally.
//! 4. **The caller's default:** the space they used last, else their first, else (for a
//!    server admin) `default`.
//!
//! A caller who isn't a member of the space gets 404, as for a space that doesn't exist, so
//! slugs don't leak. Server admins (and the local owner) are admins of every space.

// Refusals are whole responses, built once per refused request; boxing them buys nothing.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;

use axum::Json;
use axum::extract::{FromRequestParts, Request};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};

use apitorrent_auth::Principal;
use apitorrent_types::ApiError;
use apitorrent_types::spaces::{DEFAULT_SPACE, SpaceId, SpaceRole};

use crate::authn::RequestCtx;
use crate::routes::error;
use crate::{AppState, AuthMode};

/// The space a request is in.
#[derive(Debug, Clone)]
pub(crate) struct SpaceCtx {
    pub id: SpaceId,
    #[allow(dead_code, reason = "for logs and handlers that name the space")]
    pub slug: String,
    pub role: SpaceRole,
}

/// The `/s/<slug>` the request came in under; the path was rewritten without it.
#[derive(Debug, Clone)]
pub(crate) struct SpacePrefix(pub String);

/// Host names mapped to space slugs (`--space-host`), without ports, lowercase.
#[derive(Debug, Clone, Default)]
pub struct SpaceHosts(HashMap<String, String>);

impl SpaceHosts {
    /// Parse `host=slug` entries.
    pub fn parse(entries: &[String]) -> Result<Self, String> {
        let mut map = HashMap::new();
        for e in entries {
            let (host, slug) = e
                .split_once('=')
                .ok_or_else(|| format!("expected host=slug: {e}"))?;
            let (host, slug) = (host.trim().to_ascii_lowercase(), slug.trim());
            if host.is_empty() || !apitorrent_types::spaces::valid_slug(slug) {
                return Err(format!("expected host=slug: {e}"));
            }
            map.insert(host, slug.to_owned());
        }
        Ok(Self(map))
    }

    pub(crate) fn slug_for_host(&self, host: &str) -> Option<&str> {
        let h = apitorrent_auth::host_without_port(host).to_ascii_lowercase();
        self.0.get(&h).map(String::as_str)
    }
}

/// Move a leading `/s/<slug>` out of the path into a [`SpacePrefix`]. A bare `/s/<slug>`
/// becomes `/`.
pub(crate) fn strip_prefix(mut req: Request) -> Request {
    let uri = req.uri().clone();
    let Some(rest) = uri.path().strip_prefix("/s/") else {
        return req;
    };
    let (slug, tail) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    if !apitorrent_types::spaces::valid_slug(slug) {
        return req;
    }
    let slug = slug.to_owned();
    let tail = if tail.is_empty() { "/" } else { tail }.to_owned();
    let path_and_query = match uri.query() {
        Some(q) => format!("{tail}?{q}"),
        None => tail.clone(),
    };
    let mut parts = uri.into_parts();
    let Ok(pq) = path_and_query.parse() else {
        return req;
    };
    parts.path_and_query = Some(pq);
    if let Ok(new) = axum::http::Uri::from_parts(parts) {
        *req.uri_mut() = new;
        req.extensions_mut().insert(SpacePrefix(slug));
    }
    req
}

fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "no_such_space", "no such space").into_response()
}

/// The space for a signed-in caller (see the module docs), or the response refusing it.
pub(crate) fn resolve(
    s: &AppState,
    principal: &Principal,
    prefix: Option<&str>,
    host: &str,
) -> Result<SpaceCtx, Response> {
    let core = &s.core;
    let by_slug = |slug: &str| core.space_by_slug(slug);
    let named = prefix.or_else(|| s.space_hosts.slug_for_host(host));
    let space = if let Some(bound) = principal.token_space() {
        if let Some(slug) = prefix
            && by_slug(slug).is_none_or(|sp| sp.id != bound)
        {
            return Err(error(
                StatusCode::FORBIDDEN,
                "wrong_space",
                "this API token works in another space",
            )
            .into_response());
        }
        core.space(bound).map_err(|_| not_found())?
    } else if let Some(slug) = named {
        by_slug(slug).ok_or_else(not_found)?
    } else {
        default_space(s, principal).ok_or_else(not_found)?
    };
    let role = match &s.auth {
        AuthMode::Enabled(auth) => auth
            .space_role(principal, space.id)
            .map_err(|e| crate::auth_routes::Failure(e).into_response())?,
        AuthMode::DisabledOnLoopback => Some(SpaceRole::Admin),
    };
    let role = if principal.server_admin() {
        Some(role.unwrap_or(SpaceRole::Admin))
    } else {
        role
    };
    let role = role.ok_or_else(not_found)?;
    Ok(SpaceCtx {
        id: space.id,
        slug: space.slug,
        role,
    })
}

/// The caller's last space if they may still use it, else their first; a server admin
/// without either lands in `default`.
fn default_space(s: &AppState, p: &Principal) -> Option<apitorrent_types::spaces::Space> {
    let AuthMode::Enabled(auth) = &s.auth else {
        return s.core.space(DEFAULT_SPACE).ok();
    };
    let mine: Vec<SpaceId> = auth
        .memberships(&p.user.id)
        .ok()?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let last = auth.last_space(&p.user.id).ok().flatten();
    let pick = last
        .filter(|l| p.server_admin() || mine.contains(l))
        .or_else(|| mine.first().copied())
        .or_else(|| p.server_admin().then_some(DEFAULT_SPACE))?;
    s.core.space(pick).ok()
}

/// The request's space, for handlers. A request without one (a caller in no space) is
/// refused with 404.
#[derive(Debug, Clone)]
pub(crate) struct Here(pub SpaceCtx);

impl Here {
    pub(crate) fn id(&self) -> SpaceId {
        self.0.id
    }

    /// A torrent of this space.
    pub(crate) fn key(&self, id: impl Into<String>) -> apitorrent_core::Key {
        apitorrent_core::Key::new(self.0.id, id)
    }

    /// Refuse callers below `role` here.
    pub(crate) fn require(&self, role: SpaceRole) -> Result<(), (StatusCode, Json<ApiError>)> {
        if self.0.role >= role {
            Ok(())
        } else {
            Err(error(
                StatusCode::FORBIDDEN,
                "space_role",
                match role {
                    SpaceRole::Admin => "only an admin of this space can do that",
                    _ => "your role in this space may only read",
                },
            ))
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Here {
    type Rejection = Response;

    fn from_request_parts(
        parts: &mut Parts,
        _: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        std::future::ready(
            parts
                .extensions
                .get::<RequestCtx>()
                .and_then(|c| c.space.clone())
                .map(Here)
                .ok_or_else(not_found),
        )
    }
}

/// Refuse callers that aren't server admins.
pub(crate) fn server_admin(ctx: &RequestCtx) -> Result<&Principal, (StatusCode, Json<ApiError>)> {
    match &ctx.principal {
        Some(p) if p.server_admin() => Ok(p),
        Some(_) => Err(error(
            StatusCode::FORBIDDEN,
            "admin_only",
            "only a server admin can do that",
        )),
        None => Err(error(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "sign in first",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path_of(uri: &str) -> (String, Option<String>) {
        let req = Request::builder()
            .uri(uri)
            .body(axum::body::Body::empty())
            .unwrap();
        let req = strip_prefix(req);
        (
            req.uri().to_string(),
            req.extensions().get::<SpacePrefix>().map(|p| p.0.clone()),
        )
    }

    #[test]
    fn prefixes_are_stripped() {
        assert_eq!(
            path_of("/s/tv/api/v1/torrents?x=1"),
            ("/api/v1/torrents?x=1".into(), Some("tv".into()))
        );
        assert_eq!(path_of("/s/tv"), ("/".into(), Some("tv".into())));
        assert_eq!(path_of("/s/tv/"), ("/".into(), Some("tv".into())));
        assert_eq!(path_of("/api/v1/info"), ("/api/v1/info".into(), None));
        // Not a slug: left alone (and later a 404 from the router).
        assert_eq!(path_of("/s/TV/x"), ("/s/TV/x".into(), None));
    }

    #[test]
    fn space_hosts_parse() {
        let h = SpaceHosts::parse(&["TV.example.com=tv".into()]).unwrap();
        assert_eq!(h.slug_for_host("tv.example.com:443"), Some("tv"));
        assert_eq!(h.slug_for_host("other.example.com"), None);
        assert!(SpaceHosts::parse(&["nohost".into()]).is_err());
        assert!(SpaceHosts::parse(&["a=Bad Slug".into()]).is_err());
    }
}

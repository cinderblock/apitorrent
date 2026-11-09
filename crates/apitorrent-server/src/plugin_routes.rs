//! `/api/v1/plugins`: plugin management, and the plugins' own routes.
//!
//! Management sits under `/plugins` and `/plugins/{id}`, with actions under
//! `/plugins/{id}/-/…` (the `-` segment can't start a plugin route). Everything else under
//! `/plugins/{id}/` belongs to the plugin.
//!
//! Who may do what, on top of the general rules in `authn` (a credential for everything,
//! one that may write for anything but `GET`):
//!
//! - **Management is admin-only.** Plugins run code inside the daemon.
//! - **Installing, approving and uninstalling need a browser session with recent
//!   authentication**, as creating an API token does: approving grants privileges, and
//!   uninstalling deletes the plugin's data. Each is recorded in the security audit log.
//! - **Enabling, disabling and settings** take any admin credential that may write, so a
//!   script with a full-scope token can switch a misbehaving plugin off.
//! - **Plugin routes** are open to any signed-in user (viewers only for `GET`), and the
//!   plugin learns who called. It never sees the caller's cookies or `Authorization`
//!   header, and its responses can't set cookies or loosen the page's security policy:
//!   they are served sandboxed, so a plugin can't run script as this origin.

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Extension, OriginalUri, Path, Query, Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;

use apitorrent_auth::Principal;
use apitorrent_plugin_api::{
    ApprovePluginRequest, InstallPluginResponse, PluginBundle, PluginCaller, PluginError,
    PluginHttpRequest, PluginInfo, PluginLogEntry, PluginService, PluginSettings,
    PutPluginSettingsRequest,
};
use apitorrent_types::auth::Role;

use crate::authn::RequestCtx;
use crate::routes::error;
use crate::{AppState, AuthMode};

/// Plugin responses get this policy whatever they ask for: an opaque origin with nothing
/// allowed to load, so HTML from a plugin can't act as this site.
const PLUGIN_CSP: &str = "sandbox; default-src 'none'; frame-ancestors 'none'";

/// The largest request body handed to a plugin route.
const MAX_PLUGIN_BODY: usize = 8 * 1024 * 1024;

pub(crate) fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/plugins", get(list).post(install))
        .route("/plugins/{id}", get(detail).delete(uninstall))
        .route("/plugins/{id}/-/approve", post(approve))
        .route("/plugins/{id}/-/enable", post(enable))
        .route("/plugins/{id}/-/disable", post(disable))
        .route("/plugins/{id}/-/logs", get(logs))
        .route("/plugins/{id}/-/settings", get(settings).put(put_settings))
        .route("/plugins/{id}/-/{*rest}", axum::routing::any(no_action))
        .route("/plugins/{id}/{*rest}", axum::routing::any(plugin_route))
}

/// A plugin error, or a refusal, as an HTTP response.
/// Boxed: a `Response` is large, and these travel through `Result`s.
pub(crate) struct Failure(Box<Response>);

impl From<PluginError> for Failure {
    fn from(e: PluginError) -> Self {
        let status = match &e {
            PluginError::NotFound | PluginError::NoRoute => StatusCode::NOT_FOUND,
            PluginError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            PluginError::Conflict(_) => StatusCode::CONFLICT,
            PluginError::NotRunning(_) => StatusCode::SERVICE_UNAVAILABLE,
            PluginError::Timeout => StatusCode::GATEWAY_TIMEOUT,
            PluginError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let message = if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %e, "plugin request failed");
            "internal error".to_owned()
        } else {
            e.to_string()
        };
        Self(Box::new(error(status, e.code(), &message).into_response()))
    }
}

impl From<apitorrent_auth::AuthError> for Failure {
    fn from(e: apitorrent_auth::AuthError) -> Self {
        Self(Box::new(crate::auth_routes::Failure(e).into_response()))
    }
}

impl Failure {
    fn from_response(r: Response) -> Self {
        Self(Box::new(r))
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        *self.0
    }
}

type PluginResult<T> = Result<T, Failure>;

fn service(s: &AppState) -> PluginResult<&dyn PluginService> {
    s.plugins.as_deref().ok_or_else(|| {
        Failure::from_response(
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "plugins_disabled",
                "plugins are turned off on this server (--no-plugins)",
            )
            .into_response(),
        )
    })
}

fn principal(ctx: &RequestCtx) -> PluginResult<&Principal> {
    ctx.principal.as_ref().ok_or_else(|| {
        Failure::from_response(
            error(StatusCode::UNAUTHORIZED, "unauthenticated", "sign in first").into_response(),
        )
    })
}

fn admin(ctx: &RequestCtx) -> PluginResult<&Principal> {
    let p = principal(ctx)?;
    if p.user.role == Role::Admin {
        Ok(p)
    } else {
        Err(Failure::from_response(
            error(
                StatusCode::FORBIDDEN,
                "admin_only",
                "only an admin can manage plugins",
            )
            .into_response(),
        ))
    }
}

/// An admin's browser session that signed in recently (or the no-auth local admin).
fn recent_admin<'c>(s: &AppState, ctx: &'c RequestCtx) -> PluginResult<&'c Principal> {
    let p = admin(ctx)?;
    if let AuthMode::Enabled(auth) = &s.auth {
        auth.require_recent(p)?;
    }
    Ok(p)
}

fn audit(s: &AppState, ctx: &RequestCtx, p: &Principal, kind: &str, detail: &str) {
    if let AuthMode::Enabled(auth) = &s.auth {
        auth.audit(kind, p, &ctx.meta, detail);
    }
}

async fn list(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
) -> PluginResult<Json<Vec<PluginInfo>>> {
    admin(&ctx)?;
    Ok(Json(service(&s)?.list()))
}

async fn detail(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
) -> PluginResult<Json<PluginInfo>> {
    admin(&ctx)?;
    Ok(Json(service(&s)?.get(&id)?))
}

async fn install(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Json(bundle): Json<PluginBundle>,
) -> PluginResult<(StatusCode, Json<InstallPluginResponse>)> {
    let p = recent_admin(&s, &ctx)?;
    let what = format!("{} {}", bundle.manifest.id, bundle.manifest.version);
    let r = service(&s)?.install(bundle).await?;
    audit(&s, &ctx, p, "plugin_installed", &what);
    Ok((StatusCode::CREATED, Json(r)))
}

async fn approve(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
    Json(req): Json<ApprovePluginRequest>,
) -> PluginResult<Json<PluginInfo>> {
    let p = recent_admin(&s, &ctx)?;
    let info = service(&s)?
        .approve(id.clone(), req.version.clone())
        .await?;
    let granted = serde_json::to_string(&info.granted).unwrap_or_default();
    audit(
        &s,
        &ctx,
        p,
        "plugin_approved",
        &format!("{id} {}: {granted}", req.version),
    );
    Ok(Json(info))
}

async fn set_enabled(
    s: &AppState,
    ctx: &RequestCtx,
    id: String,
    enabled: bool,
) -> PluginResult<Json<PluginInfo>> {
    let p = admin(ctx)?;
    let info = service(s)?.set_enabled(id.clone(), enabled).await?;
    let kind = if enabled {
        "plugin_enabled"
    } else {
        "plugin_disabled"
    };
    audit(s, ctx, p, kind, &id);
    Ok(Json(info))
}

async fn enable(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
) -> PluginResult<Json<PluginInfo>> {
    set_enabled(&s, &ctx, id, true).await
}

async fn disable(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
) -> PluginResult<Json<PluginInfo>> {
    set_enabled(&s, &ctx, id, false).await
}

async fn uninstall(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
) -> PluginResult<StatusCode> {
    let p = recent_admin(&s, &ctx)?;
    service(&s)?.uninstall(id.clone()).await?;
    audit(&s, &ctx, p, "plugin_uninstalled", &id);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub(crate) struct LogsQuery {
    after: Option<u64>,
}

async fn logs(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
    Query(q): Query<LogsQuery>,
) -> PluginResult<Json<Vec<PluginLogEntry>>> {
    admin(&ctx)?;
    Ok(Json(service(&s)?.logs(&id, q.after)?))
}

async fn settings(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
) -> PluginResult<Json<PluginSettings>> {
    admin(&ctx)?;
    Ok(Json(service(&s)?.settings(&id)?))
}

async fn put_settings(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
    Json(req): Json<PutPluginSettingsRequest>,
) -> PluginResult<Json<PluginSettings>> {
    let p = admin(&ctx)?;
    let r = service(&s)?.set_settings(id.clone(), req.values).await?;
    audit(&s, &ctx, p, "plugin_settings_changed", &id);
    Ok(Json(r))
}

async fn no_action() -> Response {
    error(StatusCode::NOT_FOUND, "not_found", "no such plugin action").into_response()
}

/// Request headers a plugin never sees.
const HIDDEN_REQUEST_HEADERS: &[&str] = &[
    "cookie",
    "authorization",
    "proxy-authorization",
    "x-forwarded-for",
    "x-forwarded-proto",
    "x-real-ip",
    "forwarded",
];

/// Response headers a plugin can't set: cookies, hop-by-hop headers, and the security
/// headers the server sets itself.
fn allowed_response_header(name: &str) -> bool {
    !matches!(
        name,
        "set-cookie"
            | "connection"
            | "keep-alive"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
            | "content-security-policy"
            | "content-security-policy-report-only"
            | "x-frame-options"
            | "strict-transport-security"
            | "cross-origin-opener-policy"
            | "access-control-allow-origin"
            | "access-control-allow-credentials"
    )
}

async fn plugin_route(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path((id, rest)): Path<(String, String)>,
    OriginalUri(original): OriginalUri,
    req: Request,
) -> PluginResult<Response> {
    let who = principal(&ctx)?;
    let caller = PluginCaller {
        user_id: who.user.id.clone(),
        user_name: who.user.name.clone(),
        role: who.user.role,
    };
    let method = req.method().as_str().to_owned();
    let query = req.uri().query().map(str::to_owned);
    let url = format!(
        "{}://{}{}",
        if ctx.secure { "https" } else { "http" },
        ctx.host,
        original
            .path_and_query()
            .map_or("/", axum::http::uri::PathAndQuery::as_str)
    );
    let headers = req
        .headers()
        .iter()
        .filter(|(k, _)| !HIDDEN_REQUEST_HEADERS.contains(&k.as_str()))
        .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
        .collect();
    let body: Bytes = axum::body::to_bytes(req.into_body(), MAX_PLUGIN_BODY)
        .await
        .map_err(|_| {
            Failure::from_response(
                error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "too_large",
                    "plugin request bodies are limited to 8 MiB",
                )
                .into_response(),
            )
        })?;
    let r = service(&s)?
        .http(
            id,
            PluginHttpRequest {
                method,
                path: format!("/{rest}"),
                query,
                url,
                headers,
                body: body.to_vec(),
                caller,
            },
        )
        .await?;
    let mut resp = Response::new(Body::from(r.body));
    *resp.status_mut() = StatusCode::from_u16(r.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let out = resp.headers_mut();
    for (k, v) in r.headers {
        let k = k.to_ascii_lowercase();
        if !allowed_response_header(&k) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(k), HeaderValue::try_from(v)) {
            out.append(name, value);
        }
    }
    out.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(PLUGIN_CSP),
    );
    out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(resp)
}

//! `/api/v1/system/update`: the daemon's own updates. Admins only.
//!
//! - `GET /system/update`: the last check's result, the settings and whether this build
//!   can install.
//! - `POST /system/update/check`: check now.
//! - `POST /system/update/install`: download, verify and swap in the latest release, then
//!   restart. Answers `202 Accepted` once the new binary is in place; the daemon restarts
//!   a moment later, so clients poll `/info` for the new version.
//! - `PUT /system/update/settings`: the daily check and automatic installs.
//!
//! Installing is recorded in the security audit log. The service (the daemon's updater)
//! does the verifying: a signature by the compiled-in key, a newer version, a matching
//! SHA-256. A server without one (the desktop app's in-process server, whose updates come
//! with the app) answers `updates_unavailable`.

use std::future::Future;
use std::pin::Pin;

use axum::Json;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};

use apitorrent_auth::Principal;
use apitorrent_types::auth::Role;
use apitorrent_types::update::{UpdateSettings, UpdateStatus};

use crate::authn::RequestCtx;
use crate::routes::error;
use crate::{AppState, AuthMode};

pub type UpdateFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Why an update call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateFailure {
    /// A machine-readable code, such as `update_bad_signature`.
    pub code: String,
    pub message: String,
    /// Busy (`409`), disabled (`403`), or something else (`502`, a failure upstream or
    /// on disk).
    pub kind: UpdateFailureKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateFailureKind {
    Busy,
    Disabled,
    Failed,
}

/// The daemon's updater, as the server sees it.
pub trait UpdateService: Send + Sync + 'static {
    fn status(&self) -> UpdateStatus;
    fn check(&self) -> UpdateFuture<Result<UpdateStatus, UpdateFailure>>;
    /// Install the latest release. Resolves once the new binary is in place; the service
    /// then restarts the daemon by itself.
    fn install(&self) -> UpdateFuture<Result<UpdateStatus, UpdateFailure>>;
    fn set_settings(&self, settings: UpdateSettings) -> Result<UpdateStatus, UpdateFailure>;
}

pub(crate) fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/system/update", get(status))
        .route("/system/update/check", post(check))
        .route("/system/update/install", post(install))
        .route("/system/update/settings", put(settings))
}

/// A refusal or failure. Boxed: a `Response` is large, and these travel through `Result`s.
struct Refusal(Box<Response>);

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        *self.0
    }
}

impl From<UpdateFailure> for Refusal {
    fn from(f: UpdateFailure) -> Self {
        Self(Box::new(f.into_response()))
    }
}

fn refuse(status: StatusCode, code: &str, message: &str) -> Refusal {
    Refusal(Box::new(error(status, code, message).into_response()))
}

impl IntoResponse for UpdateFailure {
    fn into_response(self) -> Response {
        let status = match self.kind {
            UpdateFailureKind::Busy => StatusCode::CONFLICT,
            UpdateFailureKind::Disabled => StatusCode::FORBIDDEN,
            UpdateFailureKind::Failed => StatusCode::BAD_GATEWAY,
        };
        error(status, &self.code, &self.message).into_response()
    }
}

fn admin(ctx: &RequestCtx) -> Result<&Principal, Refusal> {
    let p = ctx
        .principal
        .as_ref()
        .ok_or_else(|| refuse(StatusCode::UNAUTHORIZED, "unauthenticated", "sign in first"))?;
    if p.user.role == Role::Admin {
        Ok(p)
    } else {
        Err(refuse(
            StatusCode::FORBIDDEN,
            "admin_only",
            "only an admin can manage updates",
        ))
    }
}

fn service(s: &AppState) -> Result<&dyn UpdateService, Refusal> {
    s.updates.as_deref().ok_or_else(|| {
        refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "updates_unavailable",
            "this server doesn't update itself",
        )
    })
}

fn audit(s: &AppState, ctx: &RequestCtx, p: &Principal, kind: &str, detail: &str) {
    if let AuthMode::Enabled(auth) = &s.auth {
        auth.audit(kind, p, &ctx.meta, detail);
    }
}

async fn status(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
) -> Result<Json<UpdateStatus>, Refusal> {
    admin(&ctx)?;
    Ok(Json(service(&s)?.status()))
}

async fn check(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
) -> Result<Json<UpdateStatus>, Refusal> {
    admin(&ctx)?;
    let svc = service(&s)?;
    svc.check().await.map(Json).map_err(Refusal::from)
}

async fn install(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
) -> Result<(StatusCode, Json<UpdateStatus>), Refusal> {
    let p = admin(&ctx)?;
    let svc = service(&s)?;
    let before = svc.status();
    let target = before
        .latest
        .as_ref()
        .map_or_else(|| "the latest release".to_owned(), |r| r.version.clone());
    match svc.install().await {
        Ok(status) => {
            audit(
                &s,
                &ctx,
                p,
                "update_installed",
                &format!("{} -> {target}", before.current_version),
            );
            Ok((StatusCode::ACCEPTED, Json(status)))
        }
        Err(e) => {
            audit(
                &s,
                &ctx,
                p,
                "update_failed",
                &format!("{} -> {target}: {}", before.current_version, e.message),
            );
            Err(e.into())
        }
    }
}

async fn settings(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Json(req): Json<UpdateSettings>,
) -> Result<Json<UpdateStatus>, Refusal> {
    let p = admin(&ctx)?;
    let status = service(&s)?.set_settings(req).map_err(Refusal::from)?;
    audit(
        &s,
        &ctx,
        p,
        "update_settings",
        &format!(
            "checkDaily={} autoInstall={}",
            req.check_daily, req.auto_install
        ),
    );
    Ok(Json(status))
}

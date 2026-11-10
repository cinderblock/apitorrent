//! HTTP handlers.

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use apitorrent_core::CoreError;
use apitorrent_types::{
    API_VERSION, AddTorrentRequest, AddTorrentResponse, ApiError, Ban, BanRequest,
    ImportDelugeRequest, ImportReport, QueueMoveRequest, ReleaseHeldRequest, ServerInfo,
    SessionStats, SetFilePrioritiesRequest, SetLabelRequest, SetUnwantedPolicyRequest, Settings,
    TorrentDetail, TorrentLimits,
};

use crate::AppState;
use crate::authn::RequestCtx;
use crate::space::{Here, server_admin};
use apitorrent_types::spaces::SpaceRole;

pub(crate) fn error(status: StatusCode, code: &str, message: &str) -> (StatusCode, Json<ApiError>) {
    (
        status,
        Json(ApiError {
            code: code.to_owned(),
            message: message.to_owned(),
        }),
    )
}

/// A core error as an HTTP response.
pub(crate) struct Failure(CoreError);

impl From<CoreError> for Failure {
    fn from(e: CoreError) -> Self {
        Self(e)
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            CoreError::InvalidTorrent(_) | CoreError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            CoreError::Duplicate => StatusCode::CONFLICT,
            CoreError::NotFound => StatusCode::NOT_FOUND,
            CoreError::Unsupported(_) => StatusCode::NOT_IMPLEMENTED,
            CoreError::Database(_) | CoreError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let message = if status == StatusCode::INTERNAL_SERVER_ERROR {
            // Internal details go to the log, not to clients.
            tracing::error!(error = %self.0, "request failed");
            "internal error".to_owned()
        } else {
            self.0.to_string()
        };
        error(status, self.0.code(), &message).into_response()
    }
}

type ApiResult<T> = Result<T, Failure>;

pub(crate) async fn info(State(s): State<AppState>) -> Json<ServerInfo> {
    let mut capabilities: Vec<String> = [
        "sync",
        "file-priorities",
        "auth",
        "storage-roots",
        "automation",
        "spaces",
    ]
    .map(str::to_owned)
    .into();
    if s.updates.is_some() {
        // `/system/update` (admins).
        capabilities.push("updates".to_owned());
    }
    Json(ServerInfo {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        api_version: API_VERSION,
        capabilities,
    })
}

/// The storage roots, so a desktop client can map them to local paths.
pub(crate) async fn roots(State(s): State<AppState>) -> Json<Vec<apitorrent_types::StorageRoot>> {
    Json(s.core.storage_roots())
}

#[derive(Debug, Deserialize)]
pub(crate) struct ListQuery {
    /// `all`: every space's torrents (server admins only).
    space: Option<String>,
}

pub(crate) async fn list(
    State(s): State<AppState>,
    here: Here,
    Extension(ctx): Extension<RequestCtx>,
    Query(q): Query<ListQuery>,
) -> Response {
    if q.space.as_deref() == Some("all") {
        if let Err(e) = server_admin(&ctx) {
            return e.into_response();
        }
        return Json(s.core.list(None)).into_response();
    }
    Json(s.core.list(Some(here.id()))).into_response()
}

pub(crate) async fn add(
    State(s): State<AppState>,
    here: Here,
    Json(req): Json<AddTorrentRequest>,
) -> ApiResult<(StatusCode, Json<AddTorrentResponse>)> {
    let id = s.core.add(here.id(), req).await?;
    Ok((StatusCode::CREATED, Json(AddTorrentResponse { id })))
}

pub(crate) async fn detail(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
) -> ApiResult<Json<TorrentDetail>> {
    Ok(Json(s.core.detail(&here.key(id)).await?))
}

pub(crate) async fn pause(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    s.core.pause(&here.key(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn resume(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    s.core.resume(&here.key(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn set_priorities(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
    Json(req): Json<SetFilePrioritiesRequest>,
) -> ApiResult<StatusCode> {
    s.core
        .set_file_priorities(&here.key(id), &req.priorities)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn set_unwanted_policy(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
    Json(req): Json<SetUnwantedPolicyRequest>,
) -> ApiResult<StatusCode> {
    s.core
        .set_unwanted_policy(&here.key(id), req.policy)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn release_held(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
    Json(req): Json<ReleaseHeldRequest>,
) -> ApiResult<StatusCode> {
    s.core.release_held(&here.key(id), req.files).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn stats(State(s): State<AppState>) -> Json<SessionStats> {
    Json(s.core.session_stats())
}

pub(crate) async fn get_settings(State(s): State<AppState>) -> Json<Settings> {
    Json(s.core.settings())
}

/// The instance's settings apply to every space: server admins only.
pub(crate) async fn put_settings(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Json(req): Json<Settings>,
) -> Response {
    if let Err(e) = server_admin(&ctx) {
        return e.into_response();
    }
    match s.core.set_settings(req).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => Failure(e).into_response(),
    }
}

pub(crate) async fn set_limits(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
    Json(req): Json<TorrentLimits>,
) -> ApiResult<StatusCode> {
    s.core.set_torrent_limits(&here.key(id), req).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn set_seed_limits(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
    Json(req): Json<apitorrent_types::SetSeedLimitsRequest>,
) -> ApiResult<StatusCode> {
    s.core.set_seed_limits(&here.key(id), req.limits).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn set_label(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
    Json(req): Json<SetLabelRequest>,
) -> ApiResult<StatusCode> {
    s.core.set_label(&here.key(id), req.label).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn queue_move(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
    Json(req): Json<QueueMoveRequest>,
) -> ApiResult<StatusCode> {
    s.core.queue_move(&here.key(id), req.to).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Import into the request's space: its admins only.
pub(crate) async fn import_deluge(
    State(s): State<AppState>,
    here: Here,
    Json(req): Json<ImportDelugeRequest>,
) -> Response {
    if let Err(e) = here.require(SpaceRole::Admin) {
        return e.into_response();
    }
    match s.core.import_deluge(here.id(), req).await {
        Ok(r) => Json::<ImportReport>(r).into_response(),
        Err(e) => Failure(e).into_response(),
    }
}

pub(crate) async fn bans(State(s): State<AppState>) -> Json<Vec<Ban>> {
    Json(s.core.bans())
}

/// Bans apply to every space: server admins only.
pub(crate) async fn ban(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Json(req): Json<BanRequest>,
) -> Response {
    if let Err(e) = server_admin(&ctx) {
        return e.into_response();
    }
    match s.core.ban(&req.ip) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => Failure(e).into_response(),
    }
}

pub(crate) async fn unban(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(ip): Path<String>,
) -> Response {
    if let Err(e) = server_admin(&ctx) {
        return e.into_response();
    }
    match s.core.unban(&ip) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => Failure(e).into_response(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RemoveQuery {
    #[serde(default)]
    delete_files: bool,
}

pub(crate) async fn remove(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
    Query(q): Query<RemoveQuery>,
) -> ApiResult<StatusCode> {
    s.core.remove(&here.key(id), q.delete_files).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn not_found() -> impl IntoResponse {
    error(StatusCode::NOT_FOUND, "not_found", "no such endpoint")
}

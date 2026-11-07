//! HTTP handlers.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use apitorrent_core::CoreError;
use apitorrent_types::{
    API_VERSION, AddTorrentRequest, AddTorrentResponse, ApiError, Ban, BanRequest,
    ReleaseHeldRequest, ServerInfo, SetFilePrioritiesRequest, SetUnwantedPolicyRequest,
    TorrentDetail, TorrentSummary,
};

use crate::AppState;

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

pub(crate) async fn info() -> Json<ServerInfo> {
    Json(ServerInfo {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        api_version: API_VERSION,
        capabilities: vec!["sync".to_owned(), "file-priorities".to_owned()],
    })
}

pub(crate) async fn list(State(s): State<AppState>) -> Json<Vec<TorrentSummary>> {
    Json(s.core.list())
}

pub(crate) async fn add(
    State(s): State<AppState>,
    Json(req): Json<AddTorrentRequest>,
) -> ApiResult<(StatusCode, Json<AddTorrentResponse>)> {
    let id = s.core.add(req).await?;
    Ok((StatusCode::CREATED, Json(AddTorrentResponse { id })))
}

pub(crate) async fn detail(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<TorrentDetail>> {
    Ok(Json(s.core.detail(&id).await?))
}

pub(crate) async fn pause(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    s.core.pause(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn resume(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    s.core.resume(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn set_priorities(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SetFilePrioritiesRequest>,
) -> ApiResult<StatusCode> {
    s.core.set_file_priorities(&id, &req.priorities).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn set_unwanted_policy(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SetUnwantedPolicyRequest>,
) -> ApiResult<StatusCode> {
    s.core.set_unwanted_policy(&id, req.policy).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn release_held(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<ReleaseHeldRequest>,
) -> ApiResult<StatusCode> {
    s.core.release_held(&id, req.files).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn bans(State(s): State<AppState>) -> Json<Vec<Ban>> {
    Json(s.core.bans())
}

pub(crate) async fn ban(
    State(s): State<AppState>,
    Json(req): Json<BanRequest>,
) -> ApiResult<StatusCode> {
    s.core.ban(&req.ip)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn unban(
    State(s): State<AppState>,
    Path(ip): Path<String>,
) -> ApiResult<StatusCode> {
    s.core.unban(&ip)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RemoveQuery {
    #[serde(default)]
    delete_files: bool,
}

pub(crate) async fn remove(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<RemoveQuery>,
) -> ApiResult<StatusCode> {
    s.core.remove(&id, q.delete_files).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn not_found() -> impl IntoResponse {
    error(StatusCode::NOT_FOUND, "not_found", "no such endpoint")
}

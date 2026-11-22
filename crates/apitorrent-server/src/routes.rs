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
    SessionStats, SetFilePrioritiesRequest, SetLabelRequest, SetTrackersRequest,
    SetUnwantedPolicyRequest, Settings, TorrentDetail, TorrentLimits,
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
            CoreError::Duplicate | CoreError::OtherOwner(_) | CoreError::Busy(_) => {
                StatusCode::CONFLICT
            }
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
        "file-order",
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

/// Set where other machines reach a storage root (UNC, `smb://`, `nfs://`, `https://`).
/// Roots belong to the instance: server admins only.
pub(crate) async fn set_root_locators(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
    Json(req): Json<apitorrent_types::SetLocatorsRequest>,
) -> Response {
    if let Err(e) = server_admin(&ctx) {
        return e.into_response();
    }
    match s.core.set_root_locators(&id, &req.locators).await {
        Ok(root) => Json(root).into_response(),
        Err(e) => Failure(e).into_response(),
    }
}

/// Check from the daemon that a web locator serves a storage root (it fetches the root's
/// marker file there). Server admins only: it makes the daemon fetch a URL.
pub(crate) async fn check_root_locator(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
    Json(req): Json<apitorrent_types::CheckLocatorRequest>,
) -> Response {
    if let Err(e) = server_admin(&ctx) {
        return e.into_response();
    }
    match s.core.check_root_locator(&id, &req.locator).await {
        Ok(check) => Json(check).into_response(),
        Err(e) => Failure(e).into_response(),
    }
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

pub(crate) async fn set_file_order(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
    Json(req): Json<apitorrent_types::files::SetFileOrderRequest>,
) -> ApiResult<StatusCode> {
    s.core.set_file_order(&here.key(id), req.order).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn set_sequential(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
    Json(req): Json<apitorrent_types::files::SetSequentialRequest>,
) -> ApiResult<StatusCode> {
    s.core.set_sequential(&here.key(id), &req.files).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn set_move_completed(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
    Json(req): Json<apitorrent_types::files::SetMoveCompletedRequest>,
) -> ApiResult<StatusCode> {
    s.core
        .set_move_completed(&here.key(id), req.settings)
        .await?;
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

/// An entry's own tracker list (the space's users; viewers are refused before this).
pub(crate) async fn set_trackers(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
    Json(req): Json<SetTrackersRequest>,
) -> ApiResult<StatusCode> {
    s.core.set_trackers(&here.key(id), req.tiers).await?;
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

/// Start importing into the request's space in the background (its admins only). Answers
/// at once with the job; `GET /jobs/{id}` follows it.
pub(crate) async fn start_import_deluge(
    State(s): State<AppState>,
    here: Here,
    Json(req): Json<ImportDelugeRequest>,
) -> Response {
    if let Err(e) = here.require(SpaceRole::Admin) {
        return e.into_response();
    }
    match s.core.start_import_deluge(here.id(), req) {
        Ok(job) => (StatusCode::ACCEPTED, Json(job)).into_response(),
        Err(e) => Failure(e).into_response(),
    }
}

/// Start a take-over in the background (the space's admins).
pub(crate) async fn start_take_over(
    State(s): State<AppState>,
    here: Here,
    Json(req): Json<apitorrent_types::TakeOverRequest>,
) -> Response {
    if let Err(e) = here.require(SpaceRole::Admin) {
        return e.into_response();
    }
    match s.core.start_take_over(here.id(), req.ids) {
        Ok(job) => (StatusCode::ACCEPTED, Json(job)).into_response(),
        Err(e) => Failure(e).into_response(),
    }
}

/// The space's background jobs, running and recently finished (its admins: only they
/// start them).
pub(crate) async fn jobs(State(s): State<AppState>, here: Here) -> Response {
    if let Err(e) = here.require(SpaceRole::Admin) {
        return e.into_response();
    }
    Json(s.core.jobs(here.id())).into_response()
}

/// One of the space's background jobs: its progress, then its outcome.
pub(crate) async fn job(State(s): State<AppState>, here: Here, Path(id): Path<u64>) -> Response {
    if let Err(e) = here.require(SpaceRole::Admin) {
        return e.into_response();
    }
    match s.core.job(here.id(), id) {
        Ok(job) => Json(job).into_response(),
        Err(e) => Failure(e).into_response(),
    }
}

/// What a Deluge installation holds, for choosing what to import (this space's admins).
pub(crate) async fn preview_deluge(
    State(s): State<AppState>,
    here: Here,
    Json(req): Json<apitorrent_types::DelugePreviewRequest>,
) -> Response {
    if let Err(e) = here.require(SpaceRole::Admin) {
        return e.into_response();
    }
    match s.core.preview_deluge(here.id(), req).await {
        Ok(p) => Json(p).into_response(),
        Err(e) => Failure(e).into_response(),
    }
}

/// The Deluge installations on the server, for the import wizard (this space's admins,
/// who can import from any of them).
pub(crate) async fn find_deluge(State(s): State<AppState>, here: Here) -> Response {
    if let Err(e) = here.require(SpaceRole::Admin) {
        return e.into_response();
    }
    match s.core.find_deluge(here.id()).await {
        Ok(f) => Json(f).into_response(),
        Err(e) => Failure(e).into_response(),
    }
}

/// Imported torrents the client they came from may still run (the space's admins).
pub(crate) async fn deluge_waiting(State(s): State<AppState>, here: Here) -> Response {
    if let Err(e) = here.require(SpaceRole::Admin) {
        return e.into_response();
    }
    match s.core.imports_not_taken_over(here.id()).await {
        Ok(w) => Json(w).into_response(),
        Err(e) => Failure(e).into_response(),
    }
}

/// Pause imported torrents in Deluge and run them here (the space's admins).
pub(crate) async fn take_over(
    State(s): State<AppState>,
    here: Here,
    Json(req): Json<apitorrent_types::TakeOverRequest>,
) -> Response {
    if let Err(e) = here.require(SpaceRole::Admin) {
        return e.into_response();
    }
    match s.core.take_over(here.id(), &req.ids).await {
        Ok(r) => Json(r).into_response(),
        Err(e) => Failure(e).into_response(),
    }
}

/// The space's tracker passkeys, masked (the space's admins: they are its credentials).
pub(crate) async fn tracker_passkeys(State(s): State<AppState>, here: Here) -> Response {
    if let Err(e) = here.require(SpaceRole::Admin) {
        return e.into_response();
    }
    Json(s.core.tracker_passkeys(here.id())).into_response()
}

/// Change one passkey on every torrent in the space that uses it.
pub(crate) async fn replace_tracker_passkey(
    State(s): State<AppState>,
    here: Here,
    Json(req): Json<apitorrent_types::ReplacePasskeyRequest>,
) -> Response {
    if let Err(e) = here.require(SpaceRole::Admin) {
        return e.into_response();
    }
    match s.core.replace_tracker_passkey(here.id(), req).await {
        Ok(r) => Json(r).into_response(),
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

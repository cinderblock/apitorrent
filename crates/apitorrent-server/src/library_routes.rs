//! The content library's server-wide side: dedupe status and "deduplicate now", and library
//! folders. Server admins only: folder paths are the instance's, and dedupe works across
//! spaces (its numbers are aggregates, never other spaces' file names).

use axum::Json;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use apitorrent_types::library::AddLibraryFolderRequest;

use crate::AppState;
use crate::authn::RequestCtx;
use crate::routes::Failure;
use crate::space::server_admin;

/// `GET /library`: dedupe status and library folders with their scan progress.
pub(crate) async fn status(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
) -> Response {
    if let Err(e) = server_admin(&ctx) {
        return e.into_response();
    }
    match s.core.library_status().await {
        Ok(st) => Json(st).into_response(),
        Err(e) => Failure::from(e).into_response(),
    }
}

/// `POST /library/dedupe`: start a pass that merges what it finds (202), even in
/// report-only mode. Refused (400) while dedupe is off.
pub(crate) async fn dedupe(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
) -> Response {
    if let Err(e) = server_admin(&ctx) {
        return e.into_response();
    }
    match s.core.start_dedupe() {
        Ok(()) => (StatusCode::ACCEPTED, Json(s.core.dedupe_status())).into_response(),
        Err(e) => Failure::from(e).into_response(),
    }
}

/// `POST /library/folders`: add a folder (absolute, existing). It is walked at once.
pub(crate) async fn add_folder(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Json(req): Json<AddLibraryFolderRequest>,
) -> Response {
    if let Err(e) = server_admin(&ctx) {
        return e.into_response();
    }
    match s.core.add_library_folder(&req.path).await {
        Ok(f) => (StatusCode::CREATED, Json(f)).into_response(),
        Err(e) => Failure::from(e).into_response(),
    }
}

/// `DELETE /library/folders/{id}`: forget a folder. Its files are never touched.
pub(crate) async fn remove_folder(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<i64>,
) -> Response {
    if let Err(e) = server_admin(&ctx) {
        return e.into_response();
    }
    match s.core.remove_library_folder(id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => Failure::from(e).into_response(),
    }
}

/// `POST /library/folders/{id}/scan`: walk a folder again soon (204; progress shows in
/// `GET /library`).
pub(crate) async fn rescan_folder(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<i64>,
) -> Response {
    if let Err(e) = server_admin(&ctx) {
        return e.into_response();
    }
    match s.core.library_status().await {
        Ok(st) if st.folders.iter().any(|f| f.id == id) => {
            s.core.rescan_library_folder(id);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(_) => Failure::from(apitorrent_core::CoreError::NotFound).into_response(),
        Err(e) => Failure::from(e).into_response(),
    }
}

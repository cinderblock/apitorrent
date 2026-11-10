//! Spaces endpoints.
//!
//! - `GET /space`: the request's space, the caller's role there, and every space the caller
//!   may switch to. Signed-in browser sessions also remember it as their last space.
//! - `GET /spaces`: the spaces the caller may use (all of them for a server admin).
//! - Server admins only: `POST /spaces` and `GET/PUT/DELETE /spaces/{slug}`.
//! - Server admins and the space's admins: its members under `/spaces/{slug}/members`
//!   (a space's admins change and remove existing members; only server admins add users,
//!   whom they pick from `GET /users`). Invitations are in `user_routes`.
//!
//! A space the caller can't use is a 404, as for one that doesn't exist.

// Refusals are whole responses, built once per refused request; boxing them buys nothing.
#![allow(clippy::result_large_err)]

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};

use apitorrent_auth::Principal;
use apitorrent_types::spaces::{
    CreateSpaceRequest, SetMemberRequest, Space, SpaceContext, SpaceMember, SpaceRole, SpaceView,
    UpdateSpaceRequest,
};

use crate::authn::RequestCtx;
use crate::routes::{Failure, error};
use crate::space::{Here, server_admin};
use crate::{AppState, AuthMode};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/space", get(current))
        .route("/spaces", get(list).post(create))
        .route("/spaces/{slug}", get(one).put(update).delete(delete_space))
        .route("/spaces/{slug}/members", get(members))
        .route(
            "/spaces/{slug}/members/{user}",
            put(set_member).delete(remove_member),
        )
}

fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "no_such_space", "no such space").into_response()
}

fn auth_failure(e: apitorrent_auth::AuthError) -> Response {
    crate::auth_routes::Failure(e).into_response()
}

/// The spaces a caller may use, with their role in each.
fn visible(s: &AppState, p: &Principal) -> Result<Vec<SpaceView>, Response> {
    let mut out = Vec::new();
    for space in s.core.spaces() {
        // A token sees only its own space.
        if p.token_space().is_some_and(|t| t != space.id) {
            continue;
        }
        let role = match &s.auth {
            AuthMode::Enabled(auth) => auth.space_role(p, space.id).map_err(auth_failure)?,
            AuthMode::DisabledOnLoopback => Some(SpaceRole::Admin),
        };
        let role = if p.server_admin() {
            Some(role.unwrap_or(SpaceRole::Admin))
        } else {
            role
        };
        if let Some(role) = role {
            out.push(view(&space, role));
        }
    }
    Ok(out)
}

fn view(space: &Space, role: SpaceRole) -> SpaceView {
    SpaceView {
        id: space.id,
        slug: space.slug.clone(),
        name: space.name.clone(),
        description: space.description.clone(),
        role,
    }
}

fn principal(ctx: &RequestCtx) -> Result<&Principal, Response> {
    ctx.principal.as_ref().ok_or_else(|| {
        error(StatusCode::UNAUTHORIZED, "unauthenticated", "sign in first").into_response()
    })
}

async fn current(
    State(s): State<AppState>,
    here: Here,
    Extension(ctx): Extension<RequestCtx>,
) -> Response {
    let p = match principal(&ctx) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let spaces = match visible(&s, p) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if let (AuthMode::Enabled(auth), Some(_)) = (&s.auth, p.session())
        && let Err(e) = auth.set_last_space(&p.user.id, here.id())
    {
        tracing::warn!(error = %e, "could not remember the last space");
    }
    let Ok(space) = s.core.space(here.id()) else {
        return not_found();
    };
    Json(SpaceContext {
        current: view(&space, here.0.role),
        spaces,
        server_admin: p.server_admin(),
    })
    .into_response()
}

async fn list(State(s): State<AppState>, Extension(ctx): Extension<RequestCtx>) -> Response {
    match principal(&ctx).and_then(|p| visible(&s, p)) {
        Ok(v) => Json(v).into_response(),
        Err(r) => r,
    }
}

/// A space by slug, for a server admin.
fn admin_space(s: &AppState, ctx: &RequestCtx, slug: &str) -> Result<Space, Response> {
    server_admin(ctx).map_err(IntoResponse::into_response)?;
    s.core.space_by_slug(slug).ok_or_else(not_found)
}

/// A space by slug, for someone who manages its members: a server admin or one of its
/// admins. A space the caller can't use at all is a 404; one they use but don't
/// administer, a 403.
fn member_space(s: &AppState, ctx: &RequestCtx, slug: &str) -> Result<Space, Response> {
    let p = principal(ctx)?;
    let space = s.core.space_by_slug(slug).ok_or_else(not_found)?;
    if p.server_admin() {
        return Ok(space);
    }
    let auth = enabled(s)?;
    match auth.space_role(p, space.id).map_err(auth_failure)? {
        Some(SpaceRole::Admin) => Ok(space),
        Some(_) => Err(error(
            StatusCode::FORBIDDEN,
            "space_role",
            "only an admin of this space can do that",
        )
        .into_response()),
        None => Err(not_found()),
    }
}

async fn create(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Json(req): Json<CreateSpaceRequest>,
) -> Response {
    if let Err(e) = server_admin(&ctx) {
        return e.into_response();
    }
    match s.core.create_space(req).await {
        Ok(space) => {
            // Space-aware plugins get an instance in it.
            if let Some(p) = &s.plugins {
                p.spaces_changed().await;
            }
            (StatusCode::CREATED, Json(space)).into_response()
        }
        Err(e) => Failure::from(e).into_response(),
    }
}

async fn one(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(slug): Path<String>,
) -> Response {
    match admin_space(&s, &ctx, &slug) {
        Ok(space) => Json(space).into_response(),
        Err(r) => r,
    }
}

async fn update(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(slug): Path<String>,
    Json(req): Json<UpdateSpaceRequest>,
) -> Response {
    let space = match admin_space(&s, &ctx, &slug) {
        Ok(space) => space,
        Err(r) => return r,
    };
    match s.core.update_space(space.id, req).await {
        Ok(space) => Json(space).into_response(),
        Err(e) => Failure::from(e).into_response(),
    }
}

async fn delete_space(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(slug): Path<String>,
) -> Response {
    let space = match admin_space(&s, &ctx, &slug) {
        Ok(space) => space,
        Err(r) => return r,
    };
    if let Err(e) = s.core.delete_space(space.id).await {
        return Failure::from(e).into_response();
    }
    if let AuthMode::Enabled(auth) = &s.auth
        && let Err(e) = auth.forget_space(space.id)
    {
        return auth_failure(e);
    }
    if let Some(p) = &s.plugins {
        p.spaces_changed().await;
    }
    StatusCode::NO_CONTENT.into_response()
}

fn enabled(s: &AppState) -> Result<&apitorrent_auth::Auth, Response> {
    match &s.auth {
        AuthMode::Enabled(auth) => Ok(auth),
        AuthMode::DisabledOnLoopback => Err(error(
            StatusCode::CONFLICT,
            "auth_disabled",
            "authentication is disabled, so there are no members",
        )
        .into_response()),
    }
}

async fn members(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(slug): Path<String>,
) -> Response {
    let space = match member_space(&s, &ctx, &slug) {
        Ok(space) => space,
        Err(r) => return r,
    };
    match enabled(&s).and_then(|a| a.members(space.id).map_err(auth_failure)) {
        Ok(m) => Json::<Vec<SpaceMember>>(m).into_response(),
        Err(r) => r,
    }
}

async fn set_member(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path((slug, user)): Path<(String, String)>,
    Json(req): Json<SetMemberRequest>,
) -> Response {
    let space = match member_space(&s, &ctx, &slug) {
        Ok(space) => space,
        Err(r) => return r,
    };
    let result = enabled(&s).and_then(|a| {
        let p = principal(&ctx)?;
        a.set_member(p, space.id, &user, req.role, &ctx.meta)
            .map_err(auth_failure)
    });
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(r) => r,
    }
}

async fn remove_member(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path((slug, user)): Path<(String, String)>,
) -> Response {
    let space = match member_space(&s, &ctx, &slug) {
        Ok(space) => space,
        Err(r) => return r,
    };
    let result = enabled(&s).and_then(|a| {
        let p = principal(&ctx)?;
        a.remove_member(p, space.id, &user, &ctx.meta)
            .map_err(auth_failure)
    });
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(r) => r,
    }
}

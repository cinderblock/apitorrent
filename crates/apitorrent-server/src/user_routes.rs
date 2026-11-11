//! Invitations, enrollment links and user management.
//!
//! - `POST /auth/enroll/info`, `/auth/enroll/start`, `/auth/enroll/finish` and
//!   `/auth/enroll/password` (public): open an invitation or recovery link. The link's
//!   secret travels in the body, never the URL.
//! - `POST /auth/enroll/accept` (a signed-in browser session): accept an invitation into
//!   the caller's own account instead of creating a user.
//! - `POST /invitations` (server admins, or a space's admins for that space),
//!   `GET /invitations` (server admins), `GET /spaces/{slug}/invitations` (the space's
//!   admins) and `DELETE /invitations/{id}`.
//! - Server admins: `GET /users`, `PATCH /users/{id}`, `DELETE /users/{id}` and
//!   `POST /users/{id}/recovery`.
//!
//! The auth crate decides who may do what; this module names spaces and builds links.

use axum::extract::{Extension, Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};

use apitorrent_auth::{Auth, AuthError, EnrollmentRecord, IssuedLink, Principal, Role, joinable};
use apitorrent_types::auth::CeremonyStart;
use apitorrent_types::spaces::{SpaceId, SpaceRole};
use apitorrent_types::users::{
    CreateInvitationRequest, CreateRecoveryRequest, CreatedInvitation, EnrollFinishRequest,
    EnrollInfo, EnrollPasswordRequest, EnrollSpace, EnrollStartRequest, EnrollTokenRequest,
    GrantedSpace, InvitationInfo, JoinedInvitation, ManagedUser, UpdateUserRequest, UserSpace,
};

use crate::auth_routes::{Failure, blocking, signed_in};
use crate::authn::RequestCtx;
use crate::routes::error;
use crate::{AppState, AuthMode};

type R<T> = Result<T, Failure>;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/enroll/info", post(enroll_info))
        .route("/auth/enroll/start", post(enroll_start))
        .route("/auth/enroll/finish", post(enroll_finish))
        .route("/auth/enroll/password", post(enroll_password))
        .route("/auth/enroll/accept", post(enroll_accept))
        .route("/invitations", get(invitations).post(invite))
        .route("/invitations/{id}", delete(revoke))
        .route("/spaces/{slug}/invitations", get(space_invitations))
        .route("/users", get(users))
        .route("/users/{id}", patch(update_user).delete(delete_user))
        .route("/users/{id}/recovery", post(recovery))
}

fn enabled(s: &AppState) -> R<&Auth> {
    match &s.auth {
        AuthMode::Enabled(a) => Ok(a),
        AuthMode::DisabledOnLoopback => Err(Failure(AuthError::Unavailable(
            "authentication is disabled (--insecure-no-auth-loopback), so there are no users"
                .into(),
        ))),
    }
}

fn principal(ctx: &RequestCtx) -> R<&Principal> {
    ctx.principal
        .as_ref()
        .ok_or(Failure(AuthError::Unauthorized))
}

/// A space's slug and name, if it still exists.
fn named(s: &AppState, id: SpaceId) -> Option<(String, String)> {
    s.core.space(id).ok().map(|sp| (sp.slug, sp.name))
}

fn info(s: &AppState, r: EnrollmentRecord) -> InvitationInfo {
    InvitationInfo {
        id: r.id,
        kind: r.kind,
        state: r.state,
        role: r.role,
        spaces: r
            .spaces
            .into_iter()
            .filter_map(|(space, role)| {
                let (slug, name) = named(s, space)?;
                Some(GrantedSpace {
                    space,
                    slug,
                    name,
                    role,
                })
            })
            .collect(),
        user_name: r.user_name,
        created_by: r.created_by,
        created_at: r.created_at,
        expires_at: r.expires_at,
        used_at: r.used_at,
        note: r.note,
    }
}

/// `<origin>/enroll#token=…`: the first public URL (passkeys work there), else the origin
/// this request came to.
fn link(auth: &Auth, ctx: &RequestCtx, secret: &str) -> String {
    let origin = auth.public_url().map_or_else(
        || {
            let scheme = if ctx.secure { "https" } else { "http" };
            format!("{scheme}://{}", ctx.host)
        },
        |u| u.origin().ascii_serialization(),
    );
    format!("{origin}/enroll#token={secret}")
}

fn created(s: &AppState, auth: &Auth, ctx: &RequestCtx, issued: IssuedLink) -> Response {
    let body = CreatedInvitation {
        link: link(auth, ctx, &issued.secret),
        invitation: info(s, issued.record),
    };
    let mut resp = (StatusCode::CREATED, Json(body)).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

// ----- Public: opening a link -----

async fn enroll_info(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Json(req): Json<EnrollTokenRequest>,
) -> R<Json<EnrollInfo>> {
    let auth = enabled(&s)?;
    let r = auth.enroll_info(&req.token, &ctx.meta)?;
    Ok(Json(EnrollInfo {
        joinable: joinable(&r),
        kind: r.kind,
        username: r.user_name,
        display_name: r.user_display_name,
        role: r.role.unwrap_or(Role::User),
        spaces: r
            .spaces
            .into_iter()
            .filter_map(|(id, role)| {
                Some(EnrollSpace {
                    name: named(&s, id)?.1,
                    role,
                })
            })
            .collect(),
        expires_at: r.expires_at,
        invited_by: r.created_by,
        password_mode: auth.password_mode(),
    }))
}

async fn enroll_start(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Json(req): Json<EnrollStartRequest>,
) -> R<Json<CeremonyStart>> {
    Ok(Json(enabled(&s)?.start_enroll(
        &ctx.host,
        &req.token,
        &req.username,
        req.display_name.as_deref(),
        &ctx.meta,
    )?))
}

async fn enroll_finish(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Json(req): Json<EnrollFinishRequest>,
) -> R<Response> {
    let session = enabled(&s)?.finish_enroll(
        &ctx.host,
        &req.ceremony_id,
        &req.credential,
        req.passkey_name.as_deref(),
        &ctx.meta,
        ctx.principal.as_ref(),
    )?;
    signed_in(&s, &ctx, &session)
}

async fn enroll_password(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Json(req): Json<EnrollPasswordRequest>,
) -> R<Response> {
    let auth = enabled(&s)?.clone();
    let meta = ctx.meta.clone();
    let current = ctx.principal.clone();
    let session = blocking(move || {
        auth.enroll_with_password(
            &req.token,
            &req.username,
            req.display_name.as_deref(),
            &req.password,
            &meta,
            current.as_ref(),
        )
    })
    .await?;
    signed_in(&s, &ctx, &session)
}

async fn enroll_accept(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Json(req): Json<EnrollTokenRequest>,
) -> R<Json<JoinedInvitation>> {
    let auth = enabled(&s)?;
    let granted = auth.accept_invitation(principal(&ctx)?, &req.token, &ctx.meta)?;
    Ok(Json(JoinedInvitation {
        spaces: granted
            .into_iter()
            .filter_map(|(space, role)| {
                let (slug, name) = named(&s, space)?;
                Some(GrantedSpace {
                    space,
                    slug,
                    name,
                    role,
                })
            })
            .collect(),
    }))
}

// ----- Invitations -----

async fn invite(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Json(req): Json<CreateInvitationRequest>,
) -> R<Response> {
    let auth = enabled(&s)?;
    let p = principal(&ctx)?;
    let mut spaces = Vec::with_capacity(req.spaces.len());
    for want in &req.spaces {
        let Some(space) = s.core.space_by_slug(&want.slug) else {
            // Don't tell non-admins which slugs exist.
            return Err(Failure(if p.server_admin() {
                AuthError::NotFound
            } else {
                AuthError::Forbidden("you can only invite into spaces you administer")
            }));
        };
        spaces.push((space.id, want.role));
    }
    let issued = auth.create_invitation(
        p,
        req.role.unwrap_or(Role::User),
        &spaces,
        req.expires_in_days,
        req.note.as_deref(),
        &ctx.meta,
    )?;
    Ok(created(&s, auth, &ctx, issued))
}

async fn invitations(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
) -> R<Json<Vec<InvitationInfo>>> {
    let list = enabled(&s)?.enrollments(principal(&ctx)?, None)?;
    Ok(Json(list.into_iter().map(|r| info(&s, r)).collect()))
}

async fn space_invitations(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(slug): Path<String>,
) -> R<Response> {
    let auth = enabled(&s)?;
    let p = principal(&ctx)?;
    let Some(space) = s.core.space_by_slug(&slug) else {
        return Ok(no_space());
    };
    // A space the caller can't use is a 404, as for one that doesn't exist.
    if !p.server_admin() && auth.space_role(p, space.id)?.is_none() {
        return Ok(no_space());
    }
    let list = auth.enrollments(p, Some(space.id))?;
    Ok(Json(list.into_iter().map(|r| info(&s, r)).collect::<Vec<_>>()).into_response())
}

fn no_space() -> Response {
    error(StatusCode::NOT_FOUND, "no_such_space", "no such space").into_response()
}

async fn revoke(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
) -> R<StatusCode> {
    enabled(&s)?.revoke_enrollment(principal(&ctx)?, &id, &ctx.meta)?;
    Ok(StatusCode::NO_CONTENT)
}

// ----- Users -----

async fn users(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
) -> R<Json<Vec<ManagedUser>>> {
    let p = principal(&ctx)?;
    let list = enabled(&s)?.managed_users(p)?;
    Ok(Json(
        list.into_iter()
            .map(|u| ManagedUser {
                you: u.info.id == p.user.id,
                id: u.info.id,
                name: u.info.name,
                display_name: u.info.display_name,
                role: u.info.role,
                disabled: u.disabled,
                created_at: u.created_at,
                passkeys: u.passkeys,
                has_password: u.info.has_password,
                tokens: u.tokens,
                last_seen_at: u.last_seen_at,
                spaces: u
                    .spaces
                    .into_iter()
                    .filter_map(|(space, role): (SpaceId, SpaceRole)| {
                        let (slug, name) = named(&s, space)?;
                        Some(UserSpace {
                            space,
                            slug,
                            name,
                            role,
                        })
                    })
                    .collect(),
            })
            .collect(),
    ))
}

async fn update_user(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
    Json(req): Json<UpdateUserRequest>,
) -> R<StatusCode> {
    let auth = enabled(&s)?;
    let p = principal(&ctx)?;
    if let Some(role) = req.role {
        auth.set_user_role(p, &id, role, &ctx.meta)?;
    }
    if let Some(disabled) = req.disabled {
        auth.set_user_disabled(p, &id, disabled, &ctx.meta)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_user(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
) -> R<StatusCode> {
    enabled(&s)?.delete_user(principal(&ctx)?, &id, &ctx.meta)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn recovery(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
    body: Option<Json<CreateRecoveryRequest>>,
) -> R<Response> {
    let auth = enabled(&s)?;
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let issued = auth.create_recovery(principal(&ctx)?, &id, req.expires_in_days, &ctx.meta)?;
    Ok(created(&s, auth, &ctx, issued))
}

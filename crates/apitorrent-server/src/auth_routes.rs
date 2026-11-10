//! `/api/v1/auth/*`: setup, sign-in, re-authentication and account management.

use axum::Json;
use axum::extract::{Extension, Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use apitorrent_auth::{Auth, AuthError, NewSession, PasswordMode, Principal};
use apitorrent_types::auth::{
    AddPasskeyFinishRequest, ApiTokenInfo, AuthStatus, CeremonyStart, CreateTokenRequest,
    CreatedToken, PasskeyFinishRequest, PasskeyInfo, PasswordLoginRequest, PasswordRequest,
    RenameRequest, SessionInfo, SetupFinishRequest, SetupPasswordRequest, SetupStartRequest,
};

use crate::authn::{RequestCtx, is_loopback_host};
use crate::routes::error;
use crate::{AppState, AuthMode};

/// An auth error as an HTTP response.
pub(crate) struct Failure(pub(crate) AuthError);

impl From<AuthError> for Failure {
    fn from(e: AuthError) -> Self {
        Self(e)
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            AuthError::InvalidRequest(_) | AuthError::Unavailable(_) => StatusCode::BAD_REQUEST,
            AuthError::Unauthorized | AuthError::InvalidSetupToken | AuthError::InvalidLink => {
                StatusCode::UNAUTHORIZED
            }
            AuthError::AlreadySetUp | AuthError::Conflict(_) => StatusCode::CONFLICT,
            AuthError::RateLimited(_) => StatusCode::TOO_MANY_REQUESTS,
            AuthError::ReauthRequired | AuthError::Forbidden(_) => StatusCode::FORBIDDEN,
            AuthError::NotFound => StatusCode::NOT_FOUND,
            AuthError::Database(_) | AuthError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let message = if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %self.0, "auth request failed");
            "internal error".to_owned()
        } else {
            self.0.to_string()
        };
        let mut resp = error(status, self.0.code(), &message).into_response();
        if let AuthError::RateLimited(after) = &self.0 {
            let secs = after.as_secs().max(1).to_string();
            if let Ok(v) = HeaderValue::from_str(&secs) {
                resp.headers_mut().insert(header::RETRY_AFTER, v);
            }
        }
        resp
    }
}

pub(crate) type AuthResult<T> = Result<T, Failure>;

fn enabled(s: &AppState) -> AuthResult<&Auth> {
    match &s.auth {
        AuthMode::Enabled(a) => Ok(a),
        AuthMode::DisabledOnLoopback => Err(Failure(AuthError::Unavailable(
            "authentication is disabled (--insecure-no-auth-loopback)".into(),
        ))),
    }
}

fn principal(ctx: &RequestCtx) -> AuthResult<&Principal> {
    ctx.principal
        .as_ref()
        .ok_or(Failure(AuthError::Unauthorized))
}

/// Run blocking auth work (password hashing) off the async workers.
pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> apitorrent_auth::Result<T> + Send + 'static,
) -> AuthResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Failure(AuthError::Internal(e.to_string())))?
        .map_err(Failure)
}

fn status_for(
    s: &AppState,
    ctx: &RequestCtx,
    principal: Option<&Principal>,
) -> AuthResult<AuthStatus> {
    let insecure_transport = !ctx.secure && !is_loopback_host(&ctx.host);
    let Ok(auth) = enabled(s) else {
        return Ok(AuthStatus {
            setup_required: false,
            auth_disabled: true,
            user: principal.map(|p| p.user.clone()),
            session: None,
            second_factor_required: false,
            passkeys_available: false,
            passkey_hint: None,
            password_mode: PasswordMode::Off,
            insecure_transport,
        });
    };
    let passkeys_available = auth.passkeys_available(&ctx.host);
    let passkey_hint = if passkeys_available {
        None
    } else if let Some(url) = auth.public_url() {
        Some(url.to_string().trim_end_matches('/').to_owned())
    } else if is_loopback_host(&ctx.host) {
        let port = ctx
            .host
            .rsplit_once(':')
            .map(|(_, p)| p)
            .filter(|p| !p.contains(']'));
        // The same scheme the request came over: an HTTPS listener doesn't speak HTTP.
        let scheme = if ctx.secure { "https" } else { "http" };
        Some(port.map_or_else(
            || format!("{scheme}://localhost"),
            |p| format!("{scheme}://localhost:{p}"),
        ))
    } else {
        None
    };
    let pending = principal.is_some_and(Principal::pending_second_factor);
    Ok(AuthStatus {
        setup_required: auth.setup_required()?,
        auth_disabled: false,
        user: principal.filter(|_| !pending).map(|p| p.user.clone()),
        session: principal
            .and_then(Principal::session)
            .map(|s| s.state.clone()),
        second_factor_required: pending,
        passkeys_available,
        passkey_hint,
        password_mode: auth.password_mode(),
        insecure_transport,
    })
}

/// Set the new session's cookie and answer with the resulting status.
pub(crate) fn signed_in(
    s: &AppState,
    ctx: &RequestCtx,
    session: &NewSession,
) -> AuthResult<Response> {
    let auth = enabled(s)?;
    let principal = auth.authenticate_session(&session.secret)?;
    let status = status_for(s, ctx, principal.as_ref())?;
    let mut resp = Json(status).into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        ctx.session_cookie(&session.secret, session.max_age.as_secs()),
    );
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(resp)
}

pub(crate) async fn status(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
) -> AuthResult<Json<AuthStatus>> {
    Ok(Json(status_for(&s, &ctx, ctx.principal.as_ref())?))
}

pub(crate) async fn setup_start(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Json(req): Json<SetupStartRequest>,
) -> AuthResult<Json<CeremonyStart>> {
    let auth = enabled(&s)?;
    Ok(Json(auth.start_setup(
        &ctx.host,
        &req.token,
        &req.username,
        req.display_name.as_deref(),
        &ctx.meta,
    )?))
}

pub(crate) async fn setup_finish(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Json(req): Json<SetupFinishRequest>,
) -> AuthResult<Response> {
    let auth = enabled(&s)?;
    let session = auth.finish_setup(
        &ctx.host,
        &req.ceremony_id,
        &req.credential,
        req.passkey_name.as_deref(),
        &ctx.meta,
    )?;
    signed_in(&s, &ctx, &session)
}

pub(crate) async fn setup_password(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Json(req): Json<SetupPasswordRequest>,
) -> AuthResult<Response> {
    let auth = enabled(&s)?.clone();
    let meta = ctx.meta.clone();
    let session = blocking(move || {
        auth.setup_with_password(
            &req.token,
            &req.username,
            req.display_name.as_deref(),
            &req.password,
            &meta,
        )
    })
    .await?;
    signed_in(&s, &ctx, &session)
}

pub(crate) async fn login_start(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
) -> AuthResult<Json<CeremonyStart>> {
    Ok(Json(enabled(&s)?.start_login(&ctx.host, &ctx.meta)?))
}

pub(crate) async fn login_finish(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Json(req): Json<PasskeyFinishRequest>,
) -> AuthResult<Response> {
    let session = enabled(&s)?.finish_login(
        &ctx.host,
        &req.ceremony_id,
        &req.credential,
        &ctx.meta,
        ctx.principal.as_ref(),
    )?;
    signed_in(&s, &ctx, &session)
}

/// A password sign-in, or the password step after a passkey when it's a second factor.
pub(crate) async fn login_password(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Json(req): Json<PasswordLoginRequest>,
) -> AuthResult<Response> {
    let auth = enabled(&s)?.clone();
    let meta = ctx.meta.clone();
    let current = ctx.principal.clone();
    let session = blocking(move || match current {
        Some(p) if p.pending_second_factor() => {
            auth.complete_second_factor(&p, &req.password, &meta)
        }
        current => auth.password_login(
            req.username.as_deref().unwrap_or_default(),
            &req.password,
            &meta,
            current.as_ref(),
        ),
    })
    .await?;
    signed_in(&s, &ctx, &session)
}

pub(crate) async fn logout(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
) -> AuthResult<Response> {
    if let (AuthMode::Enabled(auth), Some(p)) = (&s.auth, &ctx.principal) {
        auth.logout(p, &ctx.meta)?;
    }
    let mut resp = StatusCode::NO_CONTENT.into_response();
    resp.headers_mut()
        .insert(header::SET_COOKIE, ctx.clear_cookie());
    Ok(resp)
}

pub(crate) async fn reauth_start(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
) -> AuthResult<Json<CeremonyStart>> {
    Ok(Json(enabled(&s)?.start_reauth(
        &ctx.host,
        principal(&ctx)?,
        &ctx.meta,
    )?))
}

pub(crate) async fn reauth_finish(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Json(req): Json<PasskeyFinishRequest>,
) -> AuthResult<Response> {
    let session = enabled(&s)?.finish_reauth(
        &ctx.host,
        principal(&ctx)?,
        &req.ceremony_id,
        &req.credential,
        &ctx.meta,
    )?;
    signed_in(&s, &ctx, &session)
}

pub(crate) async fn reauth_password(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Json(req): Json<PasswordRequest>,
) -> AuthResult<Response> {
    let auth = enabled(&s)?.clone();
    let p = principal(&ctx)?.clone();
    let meta = ctx.meta.clone();
    let session = blocking(move || auth.reauth_with_password(&p, &req.password, &meta)).await?;
    signed_in(&s, &ctx, &session)
}

pub(crate) async fn passkeys(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
) -> AuthResult<Json<Vec<PasskeyInfo>>> {
    Ok(Json(enabled(&s)?.list_passkeys(principal(&ctx)?)?))
}

pub(crate) async fn passkey_start(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
) -> AuthResult<Json<CeremonyStart>> {
    Ok(Json(
        enabled(&s)?.start_add_passkey(&ctx.host, principal(&ctx)?)?,
    ))
}

pub(crate) async fn passkey_finish(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Json(req): Json<AddPasskeyFinishRequest>,
) -> AuthResult<(StatusCode, Json<PasskeyInfo>)> {
    let info = enabled(&s)?.finish_add_passkey(
        &ctx.host,
        principal(&ctx)?,
        &req.ceremony_id,
        &req.credential,
        req.name.as_deref(),
        &ctx.meta,
    )?;
    Ok((StatusCode::CREATED, Json(info)))
}

pub(crate) async fn passkey_rename(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
    Json(req): Json<RenameRequest>,
) -> AuthResult<StatusCode> {
    enabled(&s)?.rename_passkey(principal(&ctx)?, &id, &req.name)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn passkey_delete(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
) -> AuthResult<StatusCode> {
    enabled(&s)?.remove_passkey(principal(&ctx)?, &id, &ctx.meta)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn password_set(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Json(req): Json<PasswordRequest>,
) -> AuthResult<StatusCode> {
    let auth = enabled(&s)?.clone();
    let p = principal(&ctx)?.clone();
    let meta = ctx.meta.clone();
    blocking(move || auth.set_password(&p, &req.password, &meta)).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn password_delete(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
) -> AuthResult<StatusCode> {
    enabled(&s)?.remove_password(principal(&ctx)?, &ctx.meta)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn tokens(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
) -> AuthResult<Json<Vec<ApiTokenInfo>>> {
    Ok(Json(enabled(&s)?.list_tokens(principal(&ctx)?)?))
}

pub(crate) async fn token_create(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Json(req): Json<CreateTokenRequest>,
) -> AuthResult<Response> {
    // Bound to the space the request is in: a client using it lands there.
    let space = ctx
        .space
        .as_ref()
        .map_or(apitorrent_types::spaces::DEFAULT_SPACE, |h| h.id);
    let created: CreatedToken =
        enabled(&s)?.create_token_in(principal(&ctx)?, space, &req, &ctx.meta)?;
    let mut resp = (StatusCode::CREATED, Json(created)).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(resp)
}

pub(crate) async fn token_delete(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
) -> AuthResult<StatusCode> {
    enabled(&s)?.revoke_token(principal(&ctx)?, &id, &ctx.meta)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn sessions(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
) -> AuthResult<Json<Vec<SessionInfo>>> {
    Ok(Json(enabled(&s)?.list_sessions(principal(&ctx)?)?))
}

pub(crate) async fn session_delete(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
) -> AuthResult<Response> {
    let p = principal(&ctx)?;
    let current = p.session().is_some_and(|c| c.public_id == id);
    enabled(&s)?.revoke_session(p, &id, &ctx.meta)?;
    let mut resp = StatusCode::NO_CONTENT.into_response();
    if current {
        resp.headers_mut()
            .insert(header::SET_COOKIE, ctx.clear_cookie());
    }
    Ok(resp)
}

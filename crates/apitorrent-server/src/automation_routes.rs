//! Automation endpoints: candidates, rules (with the dry run), channels and their signed
//! feeds, and subscriptions.
//!
//! Who may do what, on top of the general rules in `authn` (a credential for everything,
//! one that may write for anything but `GET`):
//!
//! - **Reading** rules, candidates and channels, and fetching a channel's feed, takes any
//!   credential, so a subscriber needs only a `read` token.
//! - **Rules and candidates** may be changed by anyone who may write: a rule only adds
//!   torrents, which they can do anyway.
//! - **Channels and subscriptions are admin-only.** A channel publishes this instance's
//!   torrents to other machines; a subscription stores another server's token and makes
//!   this daemon connect out to an address the caller chooses.

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use apitorrent_types::auth::Role;
use apitorrent_types::automation::{
    ChannelInfo, ChannelSpec, CreateChannelRequest, DryRun, DryRunRequest, RuleInfo, RuleSpec,
    SignedChannel, StoredCandidate, SubscriptionInfo, SubscriptionSpec,
};

use crate::AppState;
use crate::authn::RequestCtx;
use crate::routes::{Failure, error};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/candidates", get(candidates).delete(forget_candidates))
        .route("/rules", get(rules).post(create_rule))
        .route("/rules/dry-run", post(dry_run))
        .route("/rules/run", post(run_rules))
        .route(
            "/rules/{id}",
            get(rule).put(update_rule).delete(delete_rule),
        )
        .route("/channels", get(channels).post(create_channel))
        .route(
            "/channels/{id}",
            get(channel).put(update_channel).delete(delete_channel),
        )
        .route("/channels/{id}/feed", get(feed))
        .route(
            "/subscriptions",
            get(subscriptions).post(create_subscription),
        )
        .route(
            "/subscriptions/{id}",
            get(subscription)
                .put(update_subscription)
                .delete(delete_subscription),
        )
        .route("/subscriptions/{id}/refresh", post(refresh_subscription))
}

/// A refusal or a failure, boxed: a `Response` is large, and these travel in `Result`s.
struct Refusal(Box<Response>);

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        *self.0
    }
}

impl<T: IntoResponse> From<(StatusCode, T)> for Refusal {
    fn from(r: (StatusCode, T)) -> Self {
        Self(Box::new(r.into_response()))
    }
}

type Result<T> = std::result::Result<T, Refusal>;

fn fail(e: apitorrent_core::CoreError) -> Refusal {
    Refusal(Box::new(Failure::from(e).into_response()))
}

fn admin(ctx: &RequestCtx) -> Result<()> {
    match &ctx.principal {
        Some(p) if p.user.role == Role::Admin => Ok(()),
        Some(_) => Err(error(
            StatusCode::FORBIDDEN,
            "admin_only",
            "only an admin can manage channels and subscriptions",
        )
        .into()),
        None => Err(error(StatusCode::UNAUTHORIZED, "unauthenticated", "sign in first").into()),
    }
}

#[derive(Deserialize)]
struct SourceQuery {
    source: Option<String>,
}

async fn candidates(
    State(s): State<AppState>,
    Query(q): Query<SourceQuery>,
) -> Result<Json<Vec<StoredCandidate>>> {
    s.core.candidates(q.source).await.map(Json).map_err(fail)
}

async fn forget_candidates(
    State(s): State<AppState>,
    Query(q): Query<SourceQuery>,
) -> Result<StatusCode> {
    let source = q.source.ok_or_else(|| -> Refusal {
        error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "name the source: ?source=plugin:rss",
        )
        .into()
    })?;
    s.core.forget_candidates(source).await.map_err(fail)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn rules(State(s): State<AppState>) -> Result<Json<Vec<RuleInfo>>> {
    s.core.rules().await.map(Json).map_err(fail)
}

async fn rule(State(s): State<AppState>, Path(id): Path<String>) -> Result<Json<RuleInfo>> {
    s.core.rule(&id).await.map(Json).map_err(fail)
}

async fn create_rule(
    State(s): State<AppState>,
    Json(spec): Json<RuleSpec>,
) -> Result<(StatusCode, Json<RuleInfo>)> {
    let info = s.core.create_rule(spec).await.map_err(fail)?;
    Ok((StatusCode::CREATED, Json(info)))
}

async fn update_rule(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(spec): Json<RuleSpec>,
) -> Result<Json<RuleInfo>> {
    s.core.update_rule(&id, spec).await.map(Json).map_err(fail)
}

async fn delete_rule(State(s): State<AppState>, Path(id): Path<String>) -> Result<StatusCode> {
    s.core.delete_rule(&id).await.map_err(fail)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn dry_run(
    State(s): State<AppState>,
    Json(req): Json<DryRunRequest>,
) -> Result<Json<DryRun>> {
    s.core.dry_run(req).await.map(Json).map_err(fail)
}

async fn run_rules(State(s): State<AppState>) -> Result<Json<Vec<RuleInfo>>> {
    s.core.run_rules().await;
    s.core.rules().await.map(Json).map_err(fail)
}

async fn channels(State(s): State<AppState>) -> Result<Json<Vec<ChannelInfo>>> {
    s.core.channels().await.map(Json).map_err(fail)
}

async fn channel(State(s): State<AppState>, Path(id): Path<String>) -> Result<Json<ChannelInfo>> {
    s.core.channel(&id).await.map(Json).map_err(fail)
}

async fn create_channel(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Json(req): Json<CreateChannelRequest>,
) -> Result<(StatusCode, Json<ChannelInfo>)> {
    admin(&ctx)?;
    let info = s.core.create_channel(req).await.map_err(fail)?;
    Ok((StatusCode::CREATED, Json(info)))
}

async fn update_channel(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
    Json(spec): Json<ChannelSpec>,
) -> Result<Json<ChannelInfo>> {
    admin(&ctx)?;
    s.core
        .update_channel(&id, spec)
        .await
        .map(Json)
        .map_err(fail)
}

async fn delete_channel(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    admin(&ctx)?;
    s.core.delete_channel(&id).await.map_err(fail)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn feed(State(s): State<AppState>, Path(id): Path<String>) -> Result<Json<SignedChannel>> {
    s.core.channel_feed(&id).await.map(Json).map_err(fail)
}

async fn subscriptions(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
) -> Result<Json<Vec<SubscriptionInfo>>> {
    admin(&ctx)?;
    s.core.subscriptions().await.map(Json).map_err(fail)
}

async fn subscription(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
) -> Result<Json<SubscriptionInfo>> {
    admin(&ctx)?;
    s.core.subscription(&id).await.map(Json).map_err(fail)
}

async fn create_subscription(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Json(spec): Json<SubscriptionSpec>,
) -> Result<(StatusCode, Json<SubscriptionInfo>)> {
    admin(&ctx)?;
    let info = s.core.create_subscription(spec).await.map_err(fail)?;
    Ok((StatusCode::CREATED, Json(info)))
}

async fn update_subscription(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
    Json(spec): Json<SubscriptionSpec>,
) -> Result<Json<SubscriptionInfo>> {
    admin(&ctx)?;
    s.core
        .update_subscription(&id, spec)
        .await
        .map(Json)
        .map_err(fail)
}

async fn delete_subscription(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    admin(&ctx)?;
    s.core.delete_subscription(&id).await.map_err(fail)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn refresh_subscription(
    State(s): State<AppState>,
    Extension(ctx): Extension<RequestCtx>,
    Path(id): Path<String>,
) -> Result<Json<SubscriptionInfo>> {
    admin(&ctx)?;
    s.core
        .refresh_subscription(&id)
        .await
        .map(Json)
        .map_err(fail)
}

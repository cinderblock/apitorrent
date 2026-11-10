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
//! - **Channels and subscriptions are for the space's admins.** A channel publishes this instance's
//!   torrents to other machines; a subscription stores another server's token and makes
//!   this daemon connect out to an address the caller chooses.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use apitorrent_types::automation::{
    ChannelInfo, ChannelSpec, CreateChannelRequest, DryRun, DryRunRequest, RuleInfo, RuleSpec,
    SignedChannel, StoredCandidate, SubscriptionInfo, SubscriptionSpec,
};
use apitorrent_types::spaces::SpaceRole;

use crate::AppState;
use crate::routes::{Failure, error};
use crate::space::Here;

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

/// Channels and subscriptions are for the space's admins.
fn admin(here: &Here) -> Result<()> {
    here.require(SpaceRole::Admin).map_err(Refusal::from)
}

#[derive(Deserialize)]
struct SourceQuery {
    source: Option<String>,
}

async fn candidates(
    State(s): State<AppState>,
    here: Here,
    Query(q): Query<SourceQuery>,
) -> Result<Json<Vec<StoredCandidate>>> {
    s.core
        .candidates(here.id(), q.source)
        .await
        .map(Json)
        .map_err(fail)
}

async fn forget_candidates(
    State(s): State<AppState>,
    here: Here,
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
    s.core
        .forget_candidates(here.id(), source)
        .await
        .map_err(fail)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn rules(State(s): State<AppState>, here: Here) -> Result<Json<Vec<RuleInfo>>> {
    s.core.rules(here.id()).await.map(Json).map_err(fail)
}

async fn rule(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
) -> Result<Json<RuleInfo>> {
    s.core.rule(here.id(), &id).await.map(Json).map_err(fail)
}

async fn create_rule(
    State(s): State<AppState>,
    here: Here,
    Json(spec): Json<RuleSpec>,
) -> Result<(StatusCode, Json<RuleInfo>)> {
    let info = s.core.create_rule(here.id(), spec).await.map_err(fail)?;
    Ok((StatusCode::CREATED, Json(info)))
}

async fn update_rule(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
    Json(spec): Json<RuleSpec>,
) -> Result<Json<RuleInfo>> {
    s.core
        .update_rule(here.id(), &id, spec)
        .await
        .map(Json)
        .map_err(fail)
}

async fn delete_rule(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    s.core.delete_rule(here.id(), &id).await.map_err(fail)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn dry_run(
    State(s): State<AppState>,
    here: Here,
    Json(req): Json<DryRunRequest>,
) -> Result<Json<DryRun>> {
    s.core.dry_run(here.id(), req).await.map(Json).map_err(fail)
}

async fn run_rules(State(s): State<AppState>, here: Here) -> Result<Json<Vec<RuleInfo>>> {
    s.core.run_rules().await;
    s.core.rules(here.id()).await.map(Json).map_err(fail)
}

async fn channels(State(s): State<AppState>, here: Here) -> Result<Json<Vec<ChannelInfo>>> {
    s.core.channels(here.id()).await.map(Json).map_err(fail)
}

async fn channel(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
) -> Result<Json<ChannelInfo>> {
    s.core.channel(here.id(), &id).await.map(Json).map_err(fail)
}

async fn create_channel(
    State(s): State<AppState>,
    here: Here,
    Json(req): Json<CreateChannelRequest>,
) -> Result<(StatusCode, Json<ChannelInfo>)> {
    admin(&here)?;
    let info = s.core.create_channel(here.id(), req).await.map_err(fail)?;
    Ok((StatusCode::CREATED, Json(info)))
}

async fn update_channel(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
    Json(spec): Json<ChannelSpec>,
) -> Result<Json<ChannelInfo>> {
    admin(&here)?;
    s.core
        .update_channel(here.id(), &id, spec)
        .await
        .map(Json)
        .map_err(fail)
}

async fn delete_channel(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    admin(&here)?;
    s.core.delete_channel(here.id(), &id).await.map_err(fail)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn feed(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
) -> Result<Json<SignedChannel>> {
    s.core
        .channel_feed(here.id(), &id)
        .await
        .map(Json)
        .map_err(fail)
}

async fn subscriptions(
    State(s): State<AppState>,
    here: Here,
) -> Result<Json<Vec<SubscriptionInfo>>> {
    admin(&here)?;
    s.core
        .subscriptions(here.id())
        .await
        .map(Json)
        .map_err(fail)
}

async fn subscription(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
) -> Result<Json<SubscriptionInfo>> {
    admin(&here)?;
    s.core
        .subscription(here.id(), &id)
        .await
        .map(Json)
        .map_err(fail)
}

async fn create_subscription(
    State(s): State<AppState>,
    here: Here,
    Json(spec): Json<SubscriptionSpec>,
) -> Result<(StatusCode, Json<SubscriptionInfo>)> {
    admin(&here)?;
    let info = s
        .core
        .create_subscription(here.id(), spec)
        .await
        .map_err(fail)?;
    Ok((StatusCode::CREATED, Json(info)))
}

async fn update_subscription(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
    Json(spec): Json<SubscriptionSpec>,
) -> Result<Json<SubscriptionInfo>> {
    admin(&here)?;
    s.core
        .update_subscription(here.id(), &id, spec)
        .await
        .map(Json)
        .map_err(fail)
}

async fn delete_subscription(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    admin(&here)?;
    s.core
        .delete_subscription(here.id(), &id)
        .await
        .map_err(fail)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn refresh_subscription(
    State(s): State<AppState>,
    here: Here,
    Path(id): Path<String>,
) -> Result<Json<SubscriptionInfo>> {
    admin(&here)?;
    s.core
        .refresh_subscription(here.id(), &id)
        .await
        .map(Json)
        .map_err(fail)
}

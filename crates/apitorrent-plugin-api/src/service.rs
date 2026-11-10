//! What the HTTP server asks of the plugin host.

use apitorrent_types::plugins::{
    InstallPluginResponse, PluginBundle, PluginCaller, PluginInfo, PluginLogEntry, PluginRouteAuth,
    PluginSettings,
};

use apitorrent_types::spaces::SpaceId;

use crate::BoxFuture;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PluginError {
    #[error("no such plugin")]
    NotFound,
    #[error("{0}")]
    InvalidRequest(String),
    #[error("{0}")]
    Conflict(String),
    /// The plugin isn't running, so it can't answer.
    #[error("{0}")]
    NotRunning(String),
    /// The plugin has no route for this method and path.
    #[error("the plugin has no such route")]
    NoRoute,
    /// The plugin took too long to answer.
    #[error("the plugin did not answer in time")]
    Timeout,
    /// The route needs apitorrent's sign-in and the request had none.
    #[error("sign in first")]
    Unauthenticated,
    #[error("{0}")]
    Internal(String),
}

impl PluginError {
    /// A stable code for API clients.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::InvalidRequest(_) => "invalid_request",
            Self::Conflict(_) => "conflict",
            Self::NotRunning(_) => "plugin_not_running",
            Self::NoRoute => "no_route",
            Self::Timeout => "plugin_timeout",
            Self::Unauthenticated => "unauthenticated",
            Self::Internal(_) => "internal",
        }
    }
}

/// A request to one of a plugin's routes. The server strips credentials (`Cookie`,
/// `Authorization`) before a plugin sees the headers.
#[derive(Debug, Clone)]
pub struct PluginHttpRequest {
    pub method: String,
    /// Relative to `/api/v1/plugins/<id>`, starting with `/`.
    pub path: String,
    /// The query string, without `?`.
    pub query: Option<String>,
    /// The URL the client used, for building links (`https://host/api/v1/plugins/<id>/…`).
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Who called, if the request signed in to apitorrent. Always set on routes apitorrent
    /// authenticates; on routes the plugin authenticates (see [`PluginRouteAuth`]) it is
    /// `None` for clients that only hold the plugin's own credentials.
    pub caller: Option<PluginCaller>,
    /// The space the request is in. A space-aware plugin's instance there answers it.
    pub space: apitorrent_types::spaces::SpaceId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginHttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Plugin management and routing, as the HTTP server uses it.
pub trait PluginService: Send + Sync + 'static {
    fn list(&self) -> Vec<PluginInfo>;
    fn get(&self, id: &str) -> Result<PluginInfo, PluginError>;
    /// Install a new plugin, or a new version of an installed one. A new plugin, or a
    /// version that asks for more than was granted, waits for [`Self::approve`].
    fn install(
        &self,
        bundle: PluginBundle,
    ) -> BoxFuture<Result<InstallPluginResponse, PluginError>>;
    /// Grant the pending version's capabilities and make it the active version. `version`
    /// must name the pending version, so an admin approves what they looked at.
    fn approve(&self, id: String, version: String) -> BoxFuture<Result<PluginInfo, PluginError>>;
    fn set_enabled(&self, id: String, enabled: bool) -> BoxFuture<Result<PluginInfo, PluginError>>;
    /// Stop the plugin and delete its code, settings and data.
    fn uninstall(&self, id: String) -> BoxFuture<Result<(), PluginError>>;
    /// Log entries after `after` (all retained entries when `None`), oldest first.
    /// Log entries of the plugin's instance in `space` (its only one, for a plugin that
    /// isn't space-aware).
    fn logs(
        &self,
        id: &str,
        space: SpaceId,
        after: Option<u64>,
    ) -> Result<Vec<PluginLogEntry>, PluginError>;
    /// Settings of the plugin's instance in `space` (see [`Self::logs`]).
    fn settings(&self, id: &str, space: SpaceId) -> Result<PluginSettings, PluginError>;
    fn set_settings(
        &self,
        id: String,
        space: SpaceId,
        values: serde_json::Value,
    ) -> BoxFuture<Result<PluginSettings, PluginError>>;
    /// Whether the plugin runs an instance per space (its manifest says `spaceAware`).
    fn space_aware(&self, id: &str) -> bool {
        let _ = id;
        false
    }
    /// Spaces were created or deleted: start and stop space-aware plugins' instances.
    fn spaces_changed(&self) -> BoxFuture<()> {
        Box::pin(async {})
    }
    /// How the route that serves `method` and `path` (relative to the plugin's prefix)
    /// authenticates, or `None` when the running plugin declares no such route. The server
    /// asks before authenticating a request to a plugin route.
    fn route_auth(&self, id: &str, method: &str, path: &str) -> Option<PluginRouteAuth>;
    /// Hand a request to the plugin's route handler.
    fn http(
        &self,
        id: String,
        req: PluginHttpRequest,
    ) -> BoxFuture<Result<PluginHttpResponse, PluginError>>;
    /// Stop every plugin, giving each a moment to run its `stop` hook.
    fn shutdown(&self) -> BoxFuture<()>;
}

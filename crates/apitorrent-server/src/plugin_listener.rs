//! A listener of its own for one plugin (`--plugin-listen <id>=<address>`).
//!
//! Some clients can't be given a URL base: they expect another program's API at the root
//! of a host and port (qBittorrent's `/api/v2/…`, say). This listener serves one plugin's
//! routes at `/`: a request for `/api/v2/auth/login` is the plugin's route
//! `/api/v2/auth/login`, exactly as `/api/v1/plugins/<id>/api/v2/auth/login` is on the
//! API's own listener.
//!
//! It is the API's router underneath, so everything holds as it does there: the host
//! check, authentication (the plugin's routes decide whether they need apitorrent's
//! sign-in), the sandboxing of plugin responses, and the listen rules (loopback, or
//! authentication plus TLS, a trusted proxy or `--allow-insecure-lan`). With built-in TLS
//! it serves HTTPS with the same certificates. Nothing else of the API is reachable
//! through it: management paths (`/-/…`) and anything that isn't a plugin route get a 404.
//! The plugin itself still never opens a socket.

use std::net::SocketAddr;

use axum::Router;
use axum::extract::{OriginalUri, Request};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use tokio::net::TcpListener;
use tower::ServiceExt as _;

use apitorrent_core::Core;

use crate::routes::error;
use crate::{ServeError, ServerConfig, TlsServe};

/// A bound plugin listener, not yet serving.
#[derive(Debug)]
pub struct PluginListener {
    core: Core,
    config: ServerConfig,
    plugin: String,
    tls: Option<TlsServe>,
    listener: TcpListener,
}

/// The plugin route a request on the plugin's listener is for, as an API path, or `None`
/// for paths that could reach anything else (management actions, dot segments).
fn api_path(plugin: &str, path: &str) -> Option<String> {
    let routable = path.starts_with('/')
        && !path.starts_with("/-")
        && !path.contains('\\')
        && path.split('/').all(|seg| {
            !matches!(
                seg.to_ascii_lowercase().as_str(),
                "." | ".." | "%2e" | "%2e%2e" | ".%2e" | "%2e."
            )
        });
    routable.then(|| format!("/api/v1/plugins/{plugin}{path}"))
}

impl PluginListener {
    /// Check the listen rules for `listen` (as for the API itself) and bind it. `tls`: the
    /// API's built-in TLS, which this listener then shares.
    pub async fn bind(
        core: Core,
        mut config: ServerConfig,
        plugin: String,
        listen: SocketAddr,
        tls: Option<&TlsServe>,
    ) -> Result<Self, ServeError> {
        if !apitorrent_plugin_api::validate::valid_id(&plugin) {
            return Err(ServeError::Io(std::io::Error::other(format!(
                "`{plugin}` is not a plugin id"
            ))));
        }
        config.listen = listen;
        crate::check_listen(&config, tls.is_some())?;
        let listener = match TcpListener::bind(listen).await {
            Ok(l) => l,
            Err(e) if cfg!(windows) && e.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(ServeError::Reserved(listen));
            }
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            core,
            config,
            plugin,
            tls: tls.cloned(),
            listener,
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Serve until `shutdown` resolves.
    pub async fn run(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), ServeError> {
        let Self {
            core,
            config,
            plugin,
            tls,
            listener,
        } = self;
        let api = crate::build_router(core, &config, tls.as_ref().map(TlsServe::ctx));
        let app = mount(api, plugin.clone());
        let local = listener.local_addr()?;
        match tls {
            None => {
                tracing::info!(addr = %local, %plugin, "plugin listening");
                axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .with_graceful_shutdown(shutdown)
                .await?;
            }
            Some(tls) => {
                tracing::info!(addr = %local, %plugin, "plugin listening (HTTPS)");
                let server_config = tls
                    .store
                    .server_config()
                    .map_err(|e| ServeError::Tls(e.to_string()))?;
                crate::tls::serve_https(listener, server_config, app, shutdown).await?;
            }
        }
        Ok(())
    }
}

/// The API router, mounted so that `/<path>` is the plugin's route `<path>`.
pub(crate) fn mount(api: Router, plugin: String) -> Router {
    Router::new().fallback(move |mut req: Request| {
        let api = api.clone();
        let plugin = plugin.clone();
        async move {
            let original = req.uri().clone();
            let Some(path) = api_path(&plugin, original.path()) else {
                return not_found();
            };
            let pq = match original.query() {
                Some(q) => format!("{path}?{q}"),
                None => path,
            };
            let mut parts = original.clone().into_parts();
            parts.path_and_query = match pq.parse() {
                Ok(p) => Some(p),
                Err(_) => return not_found(),
            };
            let Ok(uri) = Uri::from_parts(parts) else {
                return not_found();
            };
            *req.uri_mut() = uri;
            // The plugin builds links from the URL the client used, not the inner one.
            req.extensions_mut().insert(OriginalUri(original));
            match api.oneshot(req).await {
                Ok(r) => r,
                Err(e) => match e {},
            }
        }
    })
}

fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not_found", "no such route").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plugin_routes_are_reachable() {
        assert_eq!(
            api_path("qbittorrent-api", "/api/v2/auth/login").as_deref(),
            Some("/api/v1/plugins/qbittorrent-api/api/v2/auth/login")
        );
        for bad in [
            "/-/approve",
            "/-",
            "api/v2",
            "/../torrents",
            "/a/./b",
            "/a/%2e%2e/b",
            "/a/%2E./b",
            "/a\\..\\b",
        ] {
            assert_eq!(api_path("qb", bad), None, "{bad}");
        }
    }
}

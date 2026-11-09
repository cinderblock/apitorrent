//! The contract between apitorrent and its plugin host, without the JavaScript engine.
//!
//! - [`validate`]: manifest rules, checked at install.
//! - [`network`]: the host patterns of the `network` capability, and which URLs and
//!   addresses they allow.
//! - [`capabilities`]: what an upgrade asks for beyond what was granted.
//! - [`schema`]: the JSON Schema subset for plugin settings.
//! - [`AppApi`]: what plugins may ask of apitorrent. `apitorrent-core` implements it.
//! - [`PluginService`]: what the HTTP server asks of the plugin host. `apitorrent-plugins`
//!   implements it.
//! - [`SourceFetcher`]: fetching a `source` plugin's links with that plugin's grants.
//!   `apitorrent-plugins` implements it; the core's rules use it.
//! - [`Credentials`]: how the host checks API tokens for plugins that sign clients in
//!   themselves. The server implements it over the auth database.
//! - [`TrackerAdapters`]: what the core asks of `tracker-adapter` plugins. The host
//!   implements it.
//!
//! Keeping these here, apart from the V8 host, means only the host and the daemon link V8;
//! the core and the server don't.

pub mod capabilities;
pub mod network;
pub mod schema;
pub mod validate;

mod app;
mod credentials;
mod service;
mod sources;
mod tracker;

pub use apitorrent_types::plugins::*;
pub use app::{AddOutcome, AppApi, AppError, AppEvent, AppInfo, TorrentControl};
pub use credentials::{Credentials, VerifiedToken};
pub use service::{PluginError, PluginHttpRequest, PluginHttpResponse, PluginService};
pub use sources::SourceFetcher;
pub use tracker::{PackFile, PackQuery, TrackerAdapters, UnregisteredQuery};

use std::future::Future;
use std::pin::Pin;

/// A boxed, sendable future. Trait methods return these so the traits stay object safe and
/// the work can run on whichever runtime the caller picks.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

#[cfg(test)]
mod export {
    use ts_rs::{Config, TS};

    /// Write the TypeScript types the plugin SDK needs into `packages/plugin-sdk`, the same
    /// way `apitorrent-types` feeds the web UI. Run with
    /// `cargo test -p apitorrent-plugin-api export_sdk_types`.
    #[test]
    fn export_sdk_types() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/plugin-sdk/src/generated");
        let cfg = Config::from_env().with_out_dir(dir);
        for result in [
            crate::AppEvent::export_all(&cfg),
            crate::AddOutcome::export_all(&cfg),
            crate::AppInfo::export_all(&cfg),
            crate::PackQuery::export_all(&cfg),
            crate::UnregisteredQuery::export_all(&cfg),
            crate::PluginRouteAuth::export_all(&cfg),
            apitorrent_types::TorrentDetail::export_all(&cfg),
            apitorrent_types::AddTorrentRequest::export_all(&cfg),
            crate::PluginCaller::export_all(&cfg),
            crate::PluginManifest::export_all(&cfg),
            apitorrent_types::automation::ReleaseCandidate::export_all(&cfg),
            apitorrent_types::automation::SubmitCandidatesOutcome::export_all(&cfg),
        ] {
            result.expect("export SDK types");
        }
    }
}

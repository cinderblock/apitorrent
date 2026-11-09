//! Starting the plugin host for the daemon.

use std::path::Path;
use std::sync::Arc;

use apitorrent_core::Core;
use apitorrent_core::plugin_api::CorePluginApi;
use apitorrent_plugin_api::PluginService;
use apitorrent_plugins::{HostConfig, PluginHost};

/// Start the plugin host over `<data dir>/plugins`, with every enabled plugin. A host that
/// can't start is logged and left off; the daemon runs without plugins rather than not at
/// all. The core's rules fetch source plugins' `.torrent` links through the host, with each
/// plugin's own grants.
pub fn start(data_dir: &Path, core: &Core) -> Option<Arc<dyn PluginService>> {
    let config = HostConfig::new(data_dir.join("plugins"));
    match PluginHost::start(config, CorePluginApi::new(core.clone())) {
        Ok(host) => {
            tracing::info!(dir = %data_dir.join("plugins").display(), "plugin host started");
            core.set_source_fetcher(Arc::new(host.clone()));
            Some(Arc::new(host))
        }
        Err(e) => {
            tracing::error!(error = %e, "could not start the plugin host; running without plugins");
            None
        }
    }
}

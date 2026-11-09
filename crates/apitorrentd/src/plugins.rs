//! Starting the plugin host for the daemon, and connecting it to the rest: the token check
//! for plugins that sign clients in (`tokens:verify`), the core's tracker adapters, the
//! ACME DNS-01 provider, plugin listeners (`--plugin-listen`), and the first-party plugins
//! that ship with the daemon.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use clap::Args;

use apitorrent_core::Core;
use apitorrent_core::plugin_api::CorePluginApi;
use apitorrent_plugin_api::PluginService;
use apitorrent_plugins::dns::PluginDnsProvider;
use apitorrent_plugins::tracker::PluginTrackerAdapters;
use apitorrent_plugins::{HostConfig, PluginHost};
use apitorrent_server::{AuthMode, PluginListener, ServerConfig, TlsServe, plugin_credentials};

#[derive(Debug, Args)]
pub struct PluginArgs {
    /// Serve one plugin's routes at the root of a listener of its own, as
    /// `<plugin id>=<address:port>` (`qbittorrent-api=127.0.0.1:8080`), for clients that
    /// can't be given a URL base. Repeatable. The listen rules of --api apply to it, and it
    /// shares built-in TLS.
    #[arg(
        long = "plugin-listen",
        env = "APITORRENT_PLUGIN_LISTEN",
        value_delimiter = ',',
        value_parser = parse_mount
    )]
    pub plugin_listen: Vec<(String, SocketAddr)>,
}

fn parse_mount(s: &str) -> Result<(String, SocketAddr), String> {
    let (id, addr) = s
        .split_once('=')
        .ok_or_else(|| format!("{s:?}: write <plugin id>=<address:port>"))?;
    let addr = addr
        .trim()
        .parse()
        .map_err(|_| format!("{addr:?} is not an address:port"))?;
    Ok((id.trim().to_owned(), addr))
}

/// Start the plugin host over `<data dir>/plugins`, with every enabled plugin, and connect
/// it. A host that can't start is logged and left off; the daemon runs without plugins
/// rather than not at all. The core's rules fetch source plugins' `.torrent` links through
/// the host, with each plugin's own grants.
pub async fn start(
    data_dir: &Path,
    core: &Core,
    auth: &AuthMode,
    dns: Option<&Arc<PluginDnsProvider>>,
) -> Option<Arc<dyn PluginService>> {
    let mut config = HostConfig::new(data_dir.join("plugins"));
    config.credentials = plugin_credentials(auth);
    let host = match PluginHost::start(config, CorePluginApi::new(core.clone())) {
        Ok(host) => host,
        Err(e) => {
            tracing::error!(error = %e, "could not start the plugin host; running without plugins");
            return None;
        }
    };
    tracing::info!(dir = %data_dir.join("plugins").display(), "plugin host started");
    core.set_source_fetcher(Arc::new(host.clone()));
    core.set_tracker_adapters(Some(Arc::new(PluginTrackerAdapters::new(host.clone()))));
    if let Some(dns) = dns {
        dns.attach(host.clone());
    }
    host.offer_bundled(crate::bundled::bundles()).await;
    Some(Arc::new(host))
}

/// Bind each `--plugin-listen` listener, ready to run.
pub async fn bind_listeners(
    args: &PluginArgs,
    core: &Core,
    server: &ServerConfig,
    tls: Option<&TlsServe>,
) -> Result<Vec<PluginListener>, String> {
    if !args.plugin_listen.is_empty() && server.plugins.is_none() {
        return Err("--plugin-listen needs plugins (they are off)".into());
    }
    let mut out = Vec::new();
    for (id, addr) in &args.plugin_listen {
        let l = PluginListener::bind(core.clone(), server.clone(), id.clone(), *addr, tls)
            .await
            .map_err(|e| format!("--plugin-listen {id}={addr}: {e}"))?;
        out.push(l);
    }
    Ok(out)
}

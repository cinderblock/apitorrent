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
    /// can't be given a URL base. `<plugin id>@<space>=<address:port>` serves that space's
    /// instance (`qbittorrent-api@tv=127.0.0.1:8081`); otherwise it is `default`'s.
    /// Repeatable. The listen rules of --api apply to it, and it shares built-in TLS.
    #[arg(
        long = "plugin-listen",
        env = "APITORRENT_PLUGIN_LISTEN",
        value_delimiter = ',',
        value_parser = parse_mount
    )]
    pub plugin_listen: Vec<PluginMount>,
}

/// One `--plugin-listen`: a plugin, the space it serves (`None`: `default`), an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginMount {
    pub plugin: String,
    pub space: Option<String>,
    pub addr: SocketAddr,
}

fn parse_mount(s: &str) -> Result<PluginMount, String> {
    let (id, addr) = s
        .split_once('=')
        .ok_or_else(|| format!("{s:?}: write <plugin id>[@<space>]=<address:port>"))?;
    let addr = addr
        .trim()
        .parse()
        .map_err(|_| format!("{addr:?} is not an address:port"))?;
    let (plugin, space) = match id.trim().split_once('@') {
        Some((plugin, slug)) => {
            let slug = slug.trim();
            if !apitorrent_types::spaces::valid_slug(slug) {
                return Err(format!("{slug:?} is not a space slug"));
            }
            (plugin.trim(), Some(slug.to_owned()))
        }
        None => (id.trim(), None),
    };
    Ok(PluginMount {
        plugin: plugin.to_owned(),
        space,
        addr,
    })
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
    for m in &args.plugin_listen {
        let named = match &m.space {
            Some(slug) => format!("{}@{slug}={}", m.plugin, m.addr),
            None => format!("{}={}", m.plugin, m.addr),
        };
        let mut l =
            PluginListener::bind(core.clone(), server.clone(), m.plugin.clone(), m.addr, tls)
                .await
                .map_err(|e| format!("--plugin-listen {named}: {e}"))?;
        if let Some(slug) = &m.space {
            l = l
                .with_space(slug)
                .map_err(|e| format!("--plugin-listen {named}: {e}"))?;
        }
        out.push(l);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_listen_takes_an_optional_space() {
        assert_eq!(
            parse_mount("qbittorrent-api=127.0.0.1:8080").unwrap(),
            PluginMount {
                plugin: "qbittorrent-api".into(),
                space: None,
                addr: "127.0.0.1:8080".parse().unwrap(),
            }
        );
        assert_eq!(
            parse_mount(" qbittorrent-api@tv = 127.0.0.1:8081").unwrap(),
            PluginMount {
                plugin: "qbittorrent-api".into(),
                space: Some("tv".into()),
                addr: "127.0.0.1:8081".parse().unwrap(),
            }
        );
        for bad in [
            "qbittorrent-api",
            "qbittorrent-api@=127.0.0.1:1",
            "qbittorrent-api@TV=127.0.0.1:1",
            "qbittorrent-api@tv=nowhere",
        ] {
            assert!(parse_mount(bad).is_err(), "{bad}");
        }
    }
}

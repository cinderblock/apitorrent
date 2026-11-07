//! `apitorrentd`: the headless apitorrent daemon.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use apitorrent_core::{Core, CoreConfig};
use apitorrent_engine::SessionConfig;
use apitorrent_server::ServerConfig;

/// The default API port. Chosen to avoid the ports of Deluge (8112), Transmission (9091),
/// qBittorrent (8080) and the *arr apps.
const DEFAULT_API: &str = "127.0.0.1:8742";

#[derive(Debug, Parser)]
#[command(version, about = "The apitorrent daemon")]
struct Args {
    /// Where the database and state live. Defaults to the platform's app data directory.
    #[arg(long, env = "APITORRENT_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// The default directory for downloads. Defaults to `<Downloads>/apitorrent`.
    #[arg(long, env = "APITORRENT_DOWNLOAD_DIR")]
    download_dir: Option<PathBuf>,

    /// Address for the HTTP API and web UI. Loopback only until authentication lands.
    #[arg(long, env = "APITORRENT_API", default_value = DEFAULT_API)]
    api: SocketAddr,

    /// Address for incoming BitTorrent connections.
    #[arg(long, env = "APITORRENT_PEER_LISTEN", default_value = "0.0.0.0:6881")]
    peer_listen: SocketAddr,

    /// An IP blocklist to load (P2P, DAT or CIDR lines).
    #[arg(long, env = "APITORRENT_BLOCKLIST")]
    blocklist: Option<PathBuf>,

    /// Most piece data held in memory at once, in MiB, across all torrents.
    #[arg(long, env = "APITORRENT_MEMORY_LIMIT_MIB", default_value_t = 256)]
    memory_limit_mib: u64,

    /// A built web UI to serve (the `web/dist` directory).
    #[arg(long, env = "APITORRENT_WEB_ROOT")]
    web_root: Option<PathBuf>,

    /// Extra host names the API answers to (for a reverse proxy). Repeatable.
    #[arg(
        long = "public-host",
        env = "APITORRENT_PUBLIC_HOSTS",
        value_delimiter = ','
    )]
    public_hosts: Vec<String>,
}

fn default_dirs() -> Option<(PathBuf, PathBuf)> {
    let project = directories::ProjectDirs::from("", "", "apitorrent")?;
    let downloads = directories::UserDirs::new()
        .and_then(|u| u.download_dir().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| project.data_dir().join("downloads"))
        .join("apitorrent");
    Some((project.data_dir().to_path_buf(), downloads))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutting down");
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();
    let defaults = default_dirs();
    let (Some(data_dir), Some(download_dir)) = (
        args.data_dir
            .or_else(|| defaults.as_ref().map(|d| d.0.clone())),
        args.download_dir
            .or_else(|| defaults.as_ref().map(|d| d.1.clone())),
    ) else {
        eprintln!("could not determine default directories; pass --data-dir and --download-dir");
        return ExitCode::FAILURE;
    };
    tracing::info!(data_dir = %data_dir.display(), download_dir = %download_dir.display(), "starting");

    let core = match Core::start(CoreConfig {
        data_dir,
        download_dir,
        session: SessionConfig {
            listen: args.peer_listen,
            memory_limit: args.memory_limit_mib.saturating_mul(1024 * 1024),
            ..SessionConfig::default()
        },
    })
    .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "could not start");
            return ExitCode::FAILURE;
        }
    };

    if let Some(path) = &args.blocklist {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let (list, errors) = apitorrent_engine::ban::Blocklist::parse(&text);
                for e in errors.iter().take(5) {
                    tracing::warn!(path = %path.display(), error = %e, "skipped a blocklist line");
                }
                if errors.len() > 5 {
                    tracing::warn!(more = errors.len() - 5, "more blocklist lines skipped");
                }
                core.set_blocklist(list);
            }
            Err(e) => {
                tracing::error!(path = %path.display(), error = %e, "could not read the blocklist");
                return ExitCode::FAILURE;
            }
        }
    }

    let server = ServerConfig {
        listen: args.api,
        public_hosts: args.public_hosts,
        web_root: args.web_root,
    };
    let result = apitorrent_server::serve(core.clone(), server, shutdown_signal()).await;
    if let Err(e) = core.shutdown().await {
        tracing::error!(error = %e, "shutdown failed");
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "API server failed");
            ExitCode::FAILURE
        }
    }
}

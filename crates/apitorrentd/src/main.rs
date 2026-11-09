//! `apitorrentd`: the headless apitorrent daemon.

mod plugins;
mod tls;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use tracing_subscriber::EnvFilter;
use url::Url;

use apitorrent_auth::{Auth, AuthConfig, Clock, PasswordMode, SetupToken, SystemClock};
use apitorrent_core::{Core, CoreConfig};
use apitorrent_engine::SessionConfig;
use apitorrent_server::{AuthMode, ServerConfig, TlsServer, TrustedProxies, WebUi};

/// The default API port. Chosen to avoid the ports of Deluge (8112), Transmission (9091),
/// qBittorrent (8080) and the *arr apps.
const DEFAULT_API: &str = "127.0.0.1:8742";

/// Message stream encryption for peer connections.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum EncryptionArg {
    /// Plain BitTorrent only.
    Disabled,
    /// Try encryption first and fall back to plain BitTorrent; accept both.
    Enabled,
    /// Like `enabled`, but when the peer offers both, only obfuscate the handshake and
    /// leave the payload unencrypted (saves CPU).
    PreferPlaintext,
    /// Encrypted connections only. Many peers can't connect.
    Required,
}

impl From<EncryptionArg> for apitorrent_mse::Policy {
    fn from(e: EncryptionArg) -> Self {
        use apitorrent_mse::{Policy, Preference};
        match e {
            EncryptionArg::Disabled => Policy::Disabled,
            EncryptionArg::Enabled => Policy::Enabled(Preference::Rc4),
            EncryptionArg::PreferPlaintext => Policy::Enabled(Preference::Plaintext),
            EncryptionArg::Required => Policy::Required,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum PasswordArg {
    /// Passkeys only.
    Off,
    /// A password alone may sign in (for browsers or hosts without passkeys).
    Fallback,
    /// Users who set a password must enter it after their passkey.
    SecondFactor,
}

impl From<PasswordArg> for PasswordMode {
    fn from(p: PasswordArg) -> Self {
        match p {
            PasswordArg::Off => Self::Off,
            PasswordArg::Fallback => Self::Fallback,
            PasswordArg::SecondFactor => Self::SecondFactor,
        }
    }
}

#[derive(Debug, Parser)]
#[command(version, about = "The apitorrent daemon")]
#[allow(clippy::struct_excessive_bools)]
struct Args {
    /// Where the database and state live. Defaults to the platform's app data directory.
    #[arg(long, env = "APITORRENT_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// The default directory for downloads. Defaults to `<Downloads>/apitorrent`.
    #[arg(long, env = "APITORRENT_DOWNLOAD_DIR")]
    download_dir: Option<PathBuf>,

    /// Address for the HTTP API and web UI. A non-loopback address needs built-in TLS (see
    /// --tls-cert, --tls-self-signed, --acme-domain), --trusted-proxy (a TLS reverse proxy
    /// in front) or --allow-insecure-lan.
    #[arg(long, env = "APITORRENT_API", default_value = DEFAULT_API)]
    api: SocketAddr,

    /// Address for incoming BitTorrent connections.
    #[arg(long, env = "APITORRENT_PEER_LISTEN", default_value = "0.0.0.0:6881")]
    peer_listen: SocketAddr,

    /// An IP blocklist to load (P2P, DAT or CIDR lines).
    #[arg(long, env = "APITORRENT_BLOCKLIST")]
    blocklist: Option<PathBuf>,

    /// Turn off local service discovery (finding peers on the LAN by multicast).
    #[arg(long, env = "APITORRENT_NO_LSD")]
    no_lsd: bool,

    /// Turn off the DHT (finding peers without trackers).
    #[arg(long, env = "APITORRENT_NO_DHT")]
    no_dht: bool,

    /// The UDP port for the DHT. It is separate from the peer port, whose UDP side is for
    /// uTP; peers learn it from our PORT messages.
    #[arg(long, env = "APITORRENT_DHT_PORT", default_value_t = 6882)]
    dht_port: u16,

    /// Turn off uTP (BitTorrent over UDP on the peer port). Peers then connect over TCP only.
    #[arg(long, env = "APITORRENT_NO_UTP")]
    no_utp: bool,

    /// Try TCP before uTP for outgoing connections (uTP is tried first by default, since it
    /// yields to other traffic on the network).
    #[arg(long, env = "APITORRENT_PREFER_TCP")]
    prefer_tcp: bool,

    /// Encryption for peer connections, until it is changed in the settings (which then
    /// take precedence).
    #[arg(
        long,
        env = "APITORRENT_ENCRYPTION",
        value_enum,
        default_value = "enabled"
    )]
    encryption: EncryptionArg,

    /// Most piece data held in memory at once, in MiB, across all torrents.
    #[arg(long, env = "APITORRENT_MEMORY_LIMIT_MIB", default_value_t = 256)]
    memory_limit_mib: u64,

    /// A built web UI to serve (the `web/dist` directory), in place of the one embedded in
    /// release builds.
    #[arg(long, env = "APITORRENT_WEB_ROOT")]
    web_root: Option<PathBuf>,

    /// Extra host names the API answers to (for a reverse proxy). Repeatable.
    #[arg(
        long = "public-host",
        env = "APITORRENT_PUBLIC_HOSTS",
        value_delimiter = ','
    )]
    public_hosts: Vec<String>,

    /// The https:// URL browsers use to reach the UI, such as
    /// `https://torrents.example.com`. Its host becomes a passkey relying party and an
    /// accepted host name. Repeatable.
    #[arg(
        long = "public-url",
        env = "APITORRENT_PUBLIC_URLS",
        value_delimiter = ','
    )]
    public_urls: Vec<Url>,

    /// Reverse proxies (addresses or networks) whose X-Forwarded-For and X-Forwarded-Proto
    /// headers are believed. Repeatable.
    #[arg(
        long = "trusted-proxy",
        env = "APITORRENT_TRUSTED_PROXIES",
        value_delimiter = ','
    )]
    trusted_proxies: Vec<String>,

    /// Serve plain HTTP on a non-loopback address without a trusted proxy. Session cookies,
    /// tokens and passwords then cross the network unencrypted, and browsers won't offer
    /// passkeys on a plain-HTTP LAN address.
    #[arg(long, env = "APITORRENT_ALLOW_INSECURE_LAN")]
    allow_insecure_lan: bool,

    /// Whether passwords may be used, and how.
    #[arg(long, env = "APITORRENT_PASSWORDS", value_enum, default_value = "off")]
    passwords: PasswordArg,

    /// Sign-in sessions end after this many hours without activity.
    #[arg(long, env = "APITORRENT_SESSION_IDLE_HOURS", default_value_t = 7 * 24)]
    session_idle_hours: u64,

    /// Sign-in sessions end this many days after signing in, however active.
    #[arg(long, env = "APITORRENT_SESSION_LIFETIME_DAYS", default_value_t = 30)]
    session_lifetime_days: u64,

    /// While no user exists, also write the setup token to `<data dir>/setup-token`
    /// (readable only by the daemon's user on Unix). The file is deleted once used.
    #[arg(long, env = "APITORRENT_WRITE_SETUP_TOKEN")]
    write_setup_token: bool,

    /// Don't run plugins. Installed plugins stay installed, and the plugin endpoints answer
    /// that plugins are turned off.
    #[arg(long, env = "APITORRENT_NO_PLUGINS")]
    no_plugins: bool,

    /// DEVELOPMENT ONLY: no sign-in for requests from this machine. Refused with a
    /// non-loopback --api or with --trusted-proxy. Every local program and user gets full
    /// control.
    #[arg(long)]
    insecure_no_auth_loopback: bool,

    #[command(flatten)]
    tls: tls::TlsArgs,
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

/// Print the setup token where only someone at the console (or reading its log) sees it.
/// The token goes in the URL fragment, which browsers never send to the server, so it
/// can't end up in an access log.
fn print_setup_token(token: &SetupToken, urls: &[String], now_ms: i64) {
    use std::fmt::Write as _;
    let minutes = ((token.expires_at - now_ms).max(0) + 59_999) / 60_000;
    let line = "=".repeat(72);
    let mut msg =
        format!("\n{line}\n apitorrent has no users yet. To create the first admin, open\n\n");
    for url in urls {
        let _ = writeln!(msg, "   {url}/setup#token={}", token.token);
    }
    let _ = write!(
        msg,
        "\n or enter this setup token on the setup page:\n\n   {}\n\n It works once and \
         expires in {minutes} minutes; a new one is printed then.\n{line}\n",
        token.token
    );
    eprintln!("{msg}");
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

    let trusted_proxies = match TrustedProxies::parse(&args.trusted_proxies) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("--trusted-proxy: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut public_hosts = args.public_hosts;
    public_hosts.extend(
        args.public_urls
            .iter()
            .filter_map(|u| u.host_str().map(str::to_owned)),
    );
    let mut public_urls = args.public_urls;
    // Built-in TLS: its names become hosts we answer to and passkey relying parties.
    let tls = match tls::setup(&args.tls, args.api, &data_dir, &public_hosts, &public_urls) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(t) = &tls {
        public_hosts.extend(t.public_hosts.iter().cloned());
        public_urls.extend(t.public_urls.iter().cloned());
    }

    let auth = if args.insecure_no_auth_loopback {
        AuthMode::DisabledOnLoopback
    } else {
        let mut urls: Vec<String> = public_urls
            .iter()
            .map(|u| u.as_str().trim_end_matches('/').to_owned())
            .collect();
        let scheme = if tls.is_some() { "https" } else { "http" };
        urls.push(format!("{scheme}://localhost:{}", args.api.port()));
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let hook_clock = clock.clone();
        let mut config = AuthConfig::new(data_dir.join("auth.db"));
        config.public_urls = public_urls;
        config.password_mode = args.passwords.into();
        config.session_idle = Duration::from_secs(args.session_idle_hours.max(1) * 3600);
        config.session_lifetime =
            Duration::from_secs(args.session_lifetime_days.max(1) * 24 * 3600);
        config.setup_token_file = args.write_setup_token.then(|| data_dir.join("setup-token"));
        config.on_setup_token = Some(Arc::new(move |t: &SetupToken| {
            print_setup_token(t, &urls, hook_clock.now_ms());
        }));
        config.clock = clock;
        match Auth::open(config) {
            Ok(a) => AuthMode::Enabled(a),
            Err(e) => {
                tracing::error!(error = %e, "could not open the auth database");
                return ExitCode::FAILURE;
            }
        }
    };
    if let AuthMode::Enabled(a) = &auth {
        let a = a.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(Duration::from_secs(30));
            loop {
                every.tick().await;
                if let Err(e) = a.tick() {
                    tracing::warn!(error = %e, "auth housekeeping failed");
                }
            }
        });
    }

    let core = match Core::start(CoreConfig {
        data_dir: data_dir.clone(),
        download_dir,
        session: SessionConfig {
            listen: args.peer_listen,
            memory_limit: args.memory_limit_mib.saturating_mul(1024 * 1024),
            lsd: (!args.no_lsd).then(apitorrent_engine::lsd::LsdConfig::default),
            dht: (!args.no_dht).then(|| apitorrent_dht::DhtConfig {
                bind_v4: Some(SocketAddr::from(([0, 0, 0, 0], args.dht_port))),
                bind_v6: Some(SocketAddr::from((
                    std::net::Ipv6Addr::UNSPECIFIED,
                    args.dht_port,
                ))),
                ..apitorrent_dht::DhtConfig::default()
            }),
            utp: !args.no_utp,
            prefer_utp: !args.prefer_tcp,
            encryption: args.encryption.into(),
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

    let web_ui = match args.web_root {
        Some(dir) => WebUi::Dir(dir),
        None => WebUi::embedded().unwrap_or_else(|| {
            tracing::info!(
                "this build has no embedded web UI (the `embedded-ui` feature); serving the API \
                 only. Pass --web-root to serve a built one"
            );
            WebUi::None
        }),
    };

    let plugins = if args.no_plugins {
        tracing::info!("plugins are turned off (--no-plugins)");
        None
    } else {
        plugins::start(&data_dir, &core)
    };

    let server = ServerConfig {
        listen: args.api,
        public_hosts,
        web_ui,
        auth,
        trusted_proxies,
        allow_insecure_lan: args.allow_insecure_lan,
        plugins: plugins.clone(),
    };
    let result = match tls {
        None => apitorrent_server::serve(core.clone(), server, shutdown_signal()).await,
        Some(tls) => match TlsServer::bind(core.clone(), server, tls.serve).await {
            Ok(listening) => {
                // Only now can the challenge listeners answer the CA.
                let _acme = tls.acme.map(apitorrent_tls::acme::AcmeManager::start);
                listening.run(shutdown_signal()).await
            }
            Err(e) => Err(e),
        },
    };
    if let Some(p) = &plugins {
        p.shutdown().await;
    }
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

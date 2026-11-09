//! Local mode: the core runs inside the app.
//!
//! The app's own window reaches it through the in-process server (no socket, so no other
//! program or user on this machine can). Optionally the same API and the web UI are served
//! on the network, exactly like the daemon: everyone there must sign in, and plain HTTP on
//! a LAN address needs a trusted reverse proxy or an explicit acceptance of the risk.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use apitorrent_auth::{Auth, AuthConfig, Clock as _, PasswordMode, SetupToken, SystemClock};
use apitorrent_core::{Core, CoreConfig};
use apitorrent_engine::SessionConfig;
use apitorrent_server::{
    AuthMode, Embedded, ServerConfig, TlsServe, TlsServer, TrustedProxies, WebUi,
};
use serde::Serialize;
use tokio::sync::oneshot;

use crate::profiles::{LanSettings, LocalProfile};

/// The DHT's UDP port is the peer port plus one, like the daemon's defaults (6881, 6882).
fn dht_port(peer_port: u16) -> u16 {
    peer_port.checked_add(1).unwrap_or(peer_port - 1)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LanStatus {
    pub listen: SocketAddr,
    /// Set when the listener failed (the port is taken, say).
    pub error: Option<String>,
    /// While no user exists: the link that creates the first one. Open it from a browser
    /// on the network (or here).
    pub setup_link: Option<String>,
    /// With HTTPS: the pairing code of the self-signed certificate, which other apps and
    /// the CLI check before trusting it.
    pub pairing_code: Option<String>,
}

struct Lan {
    settings: LanSettings,
    pairing_code: Option<String>,
    stop: Option<oneshot::Sender<()>>,
    error: Arc<Mutex<Option<String>>>,
    task: tokio::task::JoinHandle<()>,
}

pub struct LocalNode {
    pub core: Core,
    pub embedded: Embedded,
    pub data_dir: PathBuf,
    auth: Auth,
    setup: Arc<Mutex<Option<SetupToken>>>,
    lan: Mutex<Option<Lan>>,
    housekeeping: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for LocalNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalNode")
            .field("data_dir", &self.data_dir)
            .finish_non_exhaustive()
    }
}

impl LocalNode {
    /// Start the core for a Local profile. `default_data_dir` is used when the profile
    /// doesn't name one; `web_ui` is what the network listener serves at `/`.
    pub async fn start(
        profile: &LocalProfile,
        default_data_dir: PathBuf,
        default_download_dir: PathBuf,
        web_ui: WebUi,
    ) -> Result<Arc<Self>, String> {
        let data_dir = profile.data_dir.clone().unwrap_or(default_data_dir);
        let download_dir = profile.download_dir.clone().unwrap_or(default_download_dir);
        std::fs::create_dir_all(&data_dir)
            .map_err(|e| format!("can't create {}: {e}", data_dir.display()))?;

        let setup: Arc<Mutex<Option<SetupToken>>> = Arc::default();
        let hook = setup.clone();
        let mut config = AuthConfig::new(data_dir.join("auth.db"));
        let lan = profile.lan.as_ref();
        config.public_urls = lan
            .map(|l| {
                l.public_urls
                    .iter()
                    .filter_map(|u| u.parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        // Passkeys need https (or localhost). Without a public https URL, browsers on the
        // network can't use them, so passwords are the way in there.
        config.password_mode = if config.public_urls.is_empty() {
            PasswordMode::Fallback
        } else {
            PasswordMode::Off
        };
        config.on_setup_token = Some(Arc::new(move |t: &SetupToken| {
            *hook.lock().expect("not poisoned") = Some(t.clone());
        }));
        let auth =
            Auth::open(config).map_err(|e| format!("can't open the sign-in database: {e}"))?;
        let ticker = auth.clone();
        let housekeeping = tokio::spawn(async move {
            let mut every = tokio::time::interval(Duration::from_secs(30));
            loop {
                every.tick().await;
                if let Err(e) = ticker.tick() {
                    tracing::warn!(error = %e, "auth housekeeping failed");
                }
            }
        });

        let peer = profile.peer_port;
        let core = Core::start(CoreConfig {
            data_dir: data_dir.clone(),
            download_dir,
            session: SessionConfig {
                listen: SocketAddr::from(([0, 0, 0, 0], peer)),
                lsd: Some(apitorrent_engine::lsd::LsdConfig::default()),
                dht: Some(apitorrent_dht::DhtConfig {
                    bind_v4: Some(SocketAddr::from(([0, 0, 0, 0], dht_port(peer)))),
                    bind_v6: Some(SocketAddr::from((
                        std::net::Ipv6Addr::UNSPECIFIED,
                        dht_port(peer),
                    ))),
                    ..apitorrent_dht::DhtConfig::default()
                }),
                utp: true,
                prefer_utp: true,
                encryption: apitorrent_mse::Policy::Enabled(apitorrent_mse::Preference::Rc4),
                ..SessionConfig::default()
            },
        })
        .await
        .map_err(|e| {
            housekeeping.abort();
            format!("the torrent engine couldn't start: {e}")
        })?;

        let embedded = Embedded::new(core.clone(), AuthMode::Enabled(auth.clone()));
        let node = Arc::new(Self {
            core,
            embedded,
            data_dir,
            auth,
            setup,
            lan: Mutex::new(None),
            housekeeping,
        });
        if let Some(lan) = lan {
            node.start_lan(lan.clone(), web_ui);
        }
        Ok(node)
    }

    fn start_lan(&self, settings: LanSettings, web_ui: WebUi) {
        let mut public_hosts: Vec<String> = settings
            .public_urls
            .iter()
            .filter_map(|u| url::Url::parse(u).ok())
            .filter_map(|u| u.host_str().map(str::to_owned))
            .collect();
        public_hosts.push(settings.listen.ip().to_string());
        let error: Arc<Mutex<Option<String>>> = Arc::default();
        // HTTPS with a self-signed certificate that names this listener (and the public
        // hosts), kept in the profile's data directory so the pairing code stays put.
        let tls = if settings.https {
            match self.self_signed(&settings, &public_hosts) {
                Ok(t) => Some(t),
                Err(e) => {
                    tracing::error!(error = %e, "no certificate for the network listener");
                    *error.lock().expect("not poisoned") = Some(e);
                    return self.set_lan(settings, None, error, None, tokio::spawn(async {}));
                }
            }
        } else {
            None
        };
        let pairing_code = tls
            .as_ref()
            .and_then(|t| t.store.current())
            .map(|c| c.info.pairing_code());
        let config = ServerConfig {
            listen: settings.listen,
            public_hosts,
            web_ui,
            auth: AuthMode::Enabled(self.auth.clone()),
            trusted_proxies: TrustedProxies::parse(&settings.trusted_proxies).unwrap_or_default(),
            allow_insecure_lan: settings.allow_insecure_lan,
            // The desktop app runs no plugins yet.
            plugins: None,
        };
        let (stop, stopped) = oneshot::channel::<()>();
        let core = self.core.clone();
        let failed = error.clone();
        let task = tokio::spawn(async move {
            let shutdown = async move {
                let _ = stopped.await;
            };
            let result = match tls {
                Some(tls) => match TlsServer::bind(core, config, tls).await {
                    Ok(server) => server.run(shutdown).await,
                    Err(e) => Err(e),
                },
                None => apitorrent_server::serve(core, config, shutdown).await,
            };
            if let Err(e) = result {
                tracing::error!(error = %e, "the network listener stopped");
                *failed.lock().expect("not poisoned") = Some(e.to_string());
            }
        });
        self.set_lan(settings, pairing_code, error, Some(stop), task);
    }

    fn set_lan(
        &self,
        settings: LanSettings,
        pairing_code: Option<String>,
        error: Arc<Mutex<Option<String>>>,
        stop: Option<oneshot::Sender<()>>,
        task: tokio::task::JoinHandle<()>,
    ) {
        *self.lan.lock().expect("not poisoned") = Some(Lan {
            settings,
            pairing_code,
            stop,
            error,
            task,
        });
    }

    /// The self-signed certificate for the network listener, created or renewed as needed.
    fn self_signed(
        &self,
        settings: &LanSettings,
        public_hosts: &[String],
    ) -> Result<TlsServe, String> {
        let mut names = vec![
            "localhost".to_owned(),
            "127.0.0.1".to_owned(),
            "::1".to_owned(),
        ];
        let ip = settings.listen.ip();
        if !ip.is_unspecified() && !ip.is_loopback() {
            names.push(ip.to_string());
        }
        names.extend(public_hosts.iter().cloned());
        names.sort();
        names.dedup();
        let dir = self.data_dir.join("tls").join("self-signed");
        let loaded = apitorrent_tls::ensure_self_signed(&dir, &names, std::time::SystemTime::now())
            .map_err(|e| format!("couldn't make a certificate: {e}"))?;
        let store = apitorrent_tls::CertStore::new();
        store.install(loaded, apitorrent_tls::CertSource::SelfSigned);
        apitorrent_tls::spawn_self_signed_renewer(store.clone(), dir, names);
        Ok(TlsServe {
            store,
            acme: None,
            http_redirect: None,
            https_port: settings.listen.port(),
        })
    }

    pub fn lan_status(&self) -> Option<LanStatus> {
        let lan = self.lan.lock().expect("not poisoned");
        let lan = lan.as_ref()?;
        let setup_link = if self.auth.setup_required().unwrap_or(false) {
            let token = self.setup.lock().expect("not poisoned").clone();
            token
                .filter(|t| t.expires_at > SystemClock.now_ms())
                .map(|t| {
                    let scheme = if lan.settings.https { "https" } else { "http" };
                    let base = lan.settings.public_urls.first().map_or_else(
                        || format!("{scheme}://{}", lan.settings.listen),
                        |u| u.trim_end_matches('/').to_owned(),
                    );
                    format!("{base}/setup#token={}", t.token)
                })
        } else {
            None
        };
        Some(LanStatus {
            listen: lan.settings.listen,
            error: lan.error.lock().expect("not poisoned").clone(),
            setup_link,
            pairing_code: lan.pairing_code.clone(),
        })
    }

    /// Stop the network listener and the core, saving state.
    pub async fn shutdown(&self) {
        let lan = self.lan.lock().expect("not poisoned").take();
        if let Some(mut lan) = lan {
            if let Some(stop) = lan.stop.take() {
                let _ = stop.send(());
            }
            let _ = tokio::time::timeout(Duration::from_secs(5), &mut lan.task).await;
            lan.task.abort();
        }
        if let Err(e) = self.core.shutdown().await {
            tracing::error!(error = %e, "the core didn't shut down cleanly");
        }
        self.housekeeping.abort();
    }
}

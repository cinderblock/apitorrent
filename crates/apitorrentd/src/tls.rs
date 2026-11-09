//! Built-in TLS flags and setup.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use clap::{Args, ValueEnum};
use url::Url;

use apitorrent_server::TlsServe;
use apitorrent_tls::acme::{self, AcmeConfig, AcmeManager, ChallengeKind};
use apitorrent_tls::{CertSource, CertStore};

/// How often bring-your-own certificate files are checked for changes.
const RELOAD_EVERY: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ChallengeArg {
    /// Answered over plain HTTP on port 80: needs --http-redirect-port, reachable as port 80.
    #[value(name = "http-01")]
    Http01,
    /// Answered by the HTTPS listener itself: it must be reachable as port 443.
    #[value(name = "tls-alpn-01")]
    TlsAlpn01,
}

/// Built-in HTTPS. Without any of these flags the API serves plain HTTP.
#[derive(Debug, Args)]
#[allow(clippy::struct_field_names)]
pub struct TlsArgs {
    /// Serve HTTPS with this PEM certificate chain (leaf first). Reloaded when it changes.
    #[arg(long, env = "APITORRENT_TLS_CERT", requires = "tls_key")]
    pub tls_cert: Option<PathBuf>,

    /// The PEM private key for --tls-cert.
    #[arg(long, env = "APITORRENT_TLS_KEY", requires = "tls_cert")]
    pub tls_key: Option<PathBuf>,

    /// Serve HTTPS with a self-signed certificate, for the desktop app and CLI to pin after
    /// comparing its pairing code. Browsers will warn about it. Its key is kept in the data
    /// directory, so the pairing code survives restarts.
    #[arg(
        long,
        env = "APITORRENT_TLS_SELF_SIGNED",
        conflicts_with_all = ["tls_cert", "acme_domain"]
    )]
    pub tls_self_signed: bool,

    /// Get a certificate for this name over ACME (Let's Encrypt by default). Repeatable.
    /// Using it accepts the CA's terms of service.
    #[arg(
        long,
        env = "APITORRENT_ACME_DOMAINS",
        value_delimiter = ',',
        conflicts_with = "tls_cert"
    )]
    pub acme_domain: Vec<String>,

    /// An email address the CA may write to about the certificates (expiry warnings).
    #[arg(long, env = "APITORRENT_ACME_EMAIL", requires = "acme_domain")]
    pub acme_email: Option<String>,

    /// Use Let's Encrypt's staging CA: untrusted certificates, generous rate limits. For
    /// trying a setup out.
    #[arg(long, env = "APITORRENT_ACME_STAGING", requires = "acme_domain")]
    pub acme_staging: bool,

    /// Another ACME CA's directory URL (a private step-ca, say).
    #[arg(
        long,
        env = "APITORRENT_ACME_DIRECTORY",
        requires = "acme_domain",
        conflicts_with = "acme_staging"
    )]
    pub acme_directory: Option<Url>,

    /// PEM root certificates that verify --acme-directory's own HTTPS certificate, for a
    /// private CA the OS doesn't trust.
    #[arg(
        long,
        env = "APITORRENT_ACME_DIRECTORY_CA",
        requires = "acme_directory"
    )]
    pub acme_directory_ca: Option<PathBuf>,

    /// How the CA checks that this server controls the names.
    #[arg(
        long,
        env = "APITORRENT_ACME_CHALLENGE",
        value_enum,
        default_value = "tls-alpn-01"
    )]
    pub acme_challenge: ChallengeArg,

    /// Also listen for plain HTTP on this port (same address as --api): it answers ACME
    /// HTTP-01 challenges and redirects everything else to HTTPS.
    #[arg(long, env = "APITORRENT_HTTP_REDIRECT_PORT")]
    pub http_redirect_port: Option<u16>,
}

/// What the daemon needs from TLS setup.
#[derive(Debug)]
pub struct TlsSetup {
    pub serve: TlsServe,
    /// Extra public URLs (passkey relying parties): the HTTPS origins of the certificate's
    /// names that `--public-url` doesn't already cover.
    pub public_urls: Vec<Url>,
    /// Extra host names the API answers to.
    pub public_hosts: Vec<String>,
    /// Start it once the listeners are bound (see [`AcmeManager`]).
    pub acme: Option<AcmeManager>,
}

impl TlsArgs {
    pub fn enabled(&self) -> bool {
        self.tls_cert.is_some() || self.tls_self_signed || !self.acme_domain.is_empty()
    }
}

/// Set up the certificate store and its source (file watcher, self-signed renewer or ACME).
/// `None` when TLS isn't configured.
pub fn setup(
    args: &TlsArgs,
    api: SocketAddr,
    data_dir: &Path,
    public_hosts: &[String],
    public_urls: &[Url],
) -> Result<Option<TlsSetup>, String> {
    if !args.enabled() {
        if args.http_redirect_port.is_some() {
            return Err("--http-redirect-port needs built-in TLS (--tls-cert, --tls-self-signed or --acme-domain)".into());
        }
        return Ok(None);
    }
    let store = CertStore::new();
    let mut acme_manager = None;
    // Names whose HTTPS origin should become a public URL.
    let mut names: Vec<String> = Vec::new();

    if let (Some(cert), Some(key)) = (&args.tls_cert, &args.tls_key) {
        let loaded = apitorrent_tls::load_files(cert, key)
            .map_err(|e| format!("--tls-cert/--tls-key: {e}"))?;
        if !loaded.info.is_valid_at(SystemTime::now()) {
            tracing::error!(cert = %cert.display(), "THE TLS CERTIFICATE IS EXPIRED OR NOT YET VALID");
        }
        names.extend(
            loaded
                .info
                .dns_names
                .iter()
                .filter(|n| !n.starts_with("*.") && !is_local_name(n))
                .cloned(),
        );
        store.install(loaded, CertSource::Provided);
        apitorrent_tls::spawn_file_watcher(store.clone(), cert.clone(), key.clone(), RELOAD_EVERY);
    } else if args.tls_self_signed {
        let mut sans = vec![
            "localhost".to_owned(),
            "127.0.0.1".to_owned(),
            "::1".to_owned(),
        ];
        if !api.ip().is_unspecified() && !api.ip().is_loopback() {
            sans.push(api.ip().to_string());
        }
        sans.extend(public_hosts.iter().cloned());
        sans.sort();
        sans.dedup();
        let dir = data_dir.join("tls").join("self-signed");
        let loaded = apitorrent_tls::ensure_self_signed(&dir, &sans, SystemTime::now())
            .map_err(|e| format!("self-signed certificate: {e}"))?;
        print_pairing_banner(&loaded.info);
        store.install(loaded, CertSource::SelfSigned);
        apitorrent_tls::spawn_self_signed_renewer(store.clone(), dir, sans);
    } else {
        let challenge = match args.acme_challenge {
            ChallengeArg::Http01 => {
                if args.http_redirect_port.is_none() {
                    return Err(
                        "--acme-challenge http-01 needs --http-redirect-port (the port that \
                         port 80 reaches)"
                            .into(),
                    );
                }
                ChallengeKind::Http01
            }
            ChallengeArg::TlsAlpn01 => ChallengeKind::TlsAlpn01,
        };
        let mut config =
            AcmeConfig::new(args.acme_domain.clone(), data_dir.join("acme"), challenge);
        config.contact_email.clone_from(&args.acme_email);
        config.directory_url = match (&args.acme_directory, args.acme_staging) {
            (Some(url), _) => url.to_string(),
            (None, true) => acme::LETS_ENCRYPT_STAGING.to_owned(),
            (None, false) => acme::LETS_ENCRYPT_PRODUCTION.to_owned(),
        };
        if let Some(path) = &args.acme_directory_ca {
            let pem = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            config.directory_roots = apitorrent_tls::read_pem_certificates(&pem)
                .map_err(|e| format!("{}: {e}", path.display()))?;
        }
        config
            .validate()
            .map_err(|e| format!("--acme-domain: {e}"))?;
        names.extend(config.domains.iter().cloned());
        acme_manager = Some(AcmeManager::new(store.clone(), config).map_err(|e| e.to_string())?);
    }

    let mut extra_urls = Vec::new();
    let mut extra_hosts = Vec::new();
    for name in names {
        if !public_hosts.iter().any(|h| h.eq_ignore_ascii_case(&name)) {
            extra_hosts.push(name.clone());
        }
        let covered = public_urls
            .iter()
            .any(|u| u.host_str().is_some_and(|h| h.eq_ignore_ascii_case(&name)));
        if !covered {
            let origin = if api.port() == 443 {
                format!("https://{name}")
            } else {
                format!("https://{name}:{}", api.port())
            };
            extra_urls.push(Url::parse(&origin).map_err(|e| format!("{origin}: {e}"))?);
        }
    }
    // Redirects go to the port browsers use: a public https URL's, or the listen port.
    let https_port = public_urls
        .iter()
        .find(|u| u.scheme() == "https")
        .and_then(Url::port_or_known_default)
        .unwrap_or(api.port());
    Ok(Some(TlsSetup {
        serve: TlsServe {
            store,
            acme: acme_manager.as_ref().map(AcmeManager::status),
            http_redirect: args
                .http_redirect_port
                .map(|p| SocketAddr::new(api.ip(), p)),
            https_port,
        },
        public_urls: extra_urls,
        public_hosts: extra_hosts,
        acme: acme_manager,
    }))
}

fn is_local_name(name: &str) -> bool {
    name == "localhost" || name.ends_with(".localhost")
}

/// Print the pairing code where the operator sees it, like the setup token.
fn print_pairing_banner(info: &apitorrent_tls::CertInfo) {
    let line = "=".repeat(72);
    let ips: Vec<String> = info.ip_addresses.iter().map(IpAddr::to_string).collect();
    eprintln!(
        "\n{line}\n apitorrent is serving HTTPS with a self-signed certificate.\n\n \
         To pair the desktop app or CLI, check that it shows this code:\n\n   {}\n\n \
         (SHA-256 of the public key: {})\n Names: {}\n{line}\n",
        info.pairing_code(),
        info.spki_sha256_hex(),
        info.dns_names
            .iter()
            .cloned()
            .chain(ips)
            .collect::<Vec<_>>()
            .join(", "),
    );
}

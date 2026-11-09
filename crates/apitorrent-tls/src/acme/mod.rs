//! ACME (RFC 8555): get and renew a certificate automatically.
//!
//! The protocol client is [`instant_acme`] (see `plans/tls.md` for the evaluation); this
//! module is the state machine around it: the account, challenge provisioning, when to
//! renew, backoff, and where everything is kept.
//!
//! # Challenges
//!
//! - **TLS-ALPN-01** (RFC 8737): answered by the TLS listener itself. The validator
//!   connects to port 443 of the domain offering only the `acme-tls/1` protocol, and the
//!   [`CertStore`]'s resolver hands it a challenge certificate.
//! - **HTTP-01**: answered on port 80 by the daemon's HTTP listener
//!   (`--http-redirect-port`) from [`CertStore::http01_response`].
//! - **DNS-01**: through a [`DnsProvider`] (a `dns-provider` plugin). Required for wildcard
//!   names.
//!
//! # Storage
//!
//! Under `state_dir` (the daemon uses `<data dir>/acme`), one directory per ACME server,
//! named after its directory URL so staging and production never mix:
//!
//! - `account.json`: the account's id and private key (owner-only).
//! - `certs/<first domain>.pem`: the certificate chain followed by its private key, in one
//!   file so it's replaced atomically (owner-only).
//!
//! # Renewal
//!
//! At 2/3 of the certificate's lifetime (with a little jitter), or at once when there is
//! no certificate or it names other domains. Failures back off from 5 minutes, doubling,
//! up to 6 hours (1 hour once expiry is within a week). Each order uses a fresh key.

mod dns;
mod http;
mod schedule;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, NewAccount,
    NewOrder, OrderStatus, RetryPolicy,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::sign::CertifiedKey;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::task::JoinHandle;

pub use dns::{BoxFuture, DnsError, DnsProvider, TxtRecord};

use crate::cert::{LoadedCert, load_pem};
use crate::files::humantime_utc;
use crate::fsutil::{read_optional, write_atomic};
use crate::store::{CertSource, CertStore};
use crate::{TlsError, provider};

/// Let's Encrypt's production directory.
pub const LETS_ENCRYPT_PRODUCTION: &str = "https://acme-v02.api.letsencrypt.org/directory";
/// Let's Encrypt's staging directory: untrusted certificates, generous rate limits.
pub const LETS_ENCRYPT_STAGING: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";

/// How domain control is proven.
#[derive(Clone)]
pub enum ChallengeKind {
    Http01,
    TlsAlpn01,
    Dns01(Arc<dyn DnsProvider>),
}

impl std::fmt::Debug for ChallengeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Http01 => "http-01",
            Self::TlsAlpn01 => "tls-alpn-01",
            Self::Dns01(_) => "dns-01",
        })
    }
}

impl ChallengeKind {
    fn acme_type(&self) -> ChallengeType {
        match self {
            Self::Http01 => ChallengeType::Http01,
            Self::TlsAlpn01 => ChallengeType::TlsAlpn01,
            Self::Dns01(_) => ChallengeType::Dns01,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AcmeConfig {
    /// The names to put on the certificate. The first names the stored file.
    pub domains: Vec<String>,
    /// An address the CA may write to about the account (expiry warnings, policy changes).
    pub contact_email: Option<String>,
    /// The directory URL: [`LETS_ENCRYPT_PRODUCTION`], [`LETS_ENCRYPT_STAGING`], or another
    /// CA's.
    pub directory_url: String,
    /// Roots that verify a private ACME server's own HTTPS certificate (step-ca, Pebble).
    /// Empty means the OS trust store, as for Let's Encrypt.
    pub directory_roots: Vec<CertificateDer<'static>>,
    pub challenge: ChallengeKind,
    /// Where accounts and certificates are kept.
    pub state_dir: PathBuf,
    /// How long to wait for the CA to validate and to issue, per step.
    pub poll_timeout: Duration,
}

impl AcmeConfig {
    pub fn new(domains: Vec<String>, state_dir: PathBuf, challenge: ChallengeKind) -> Self {
        Self {
            domains,
            contact_email: None,
            directory_url: LETS_ENCRYPT_PRODUCTION.to_owned(),
            directory_roots: Vec::new(),
            challenge,
            state_dir,
            poll_timeout: Duration::from_secs(180),
        }
    }

    /// Normalise the domains and check the configuration makes sense.
    pub fn validate(&mut self) -> Result<(), AcmeError> {
        if self.domains.is_empty() {
            return Err(AcmeError::Config("no ACME domains".into()));
        }
        let mut seen = Vec::new();
        for d in &mut self.domains {
            let n = d.trim().trim_end_matches('.').to_ascii_lowercase();
            let bare = n.strip_prefix("*.").unwrap_or(&n);
            let valid = !bare.is_empty()
                && bare.len() <= 253
                && bare.contains('.')
                && bare.parse::<std::net::IpAddr>().is_err()
                && bare.split('.').all(|label| {
                    !label.is_empty()
                        && label.len() <= 63
                        && !label.starts_with('-')
                        && !label.ends_with('-')
                        && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                });
            if !valid {
                return Err(AcmeError::Config(format!(
                    "{d:?} is not a public DNS name (IP addresses and single-label names can't \
                     get ACME certificates)"
                )));
            }
            if n.starts_with("*.") && !matches!(self.challenge, ChallengeKind::Dns01(_)) {
                return Err(AcmeError::Config(format!(
                    "{n}: wildcard names need the dns-01 challenge"
                )));
            }
            if seen.contains(&n) {
                return Err(AcmeError::Config(format!("{n} is listed twice")));
            }
            seen.push(n.clone());
            *d = n;
        }
        if let Some(e) = &self.contact_email {
            let e = e.trim().trim_start_matches("mailto:");
            if !e.contains('@') || e.contains(',') || e.chars().any(char::is_whitespace) {
                return Err(AcmeError::Config(format!("{e:?} is not an email address")));
            }
            self.contact_email = Some(e.to_owned());
        }
        if !self.directory_url.starts_with("https://") {
            return Err(AcmeError::Config(
                "the ACME directory URL must be https".into(),
            ));
        }
        Ok(())
    }

    /// This CA's directory under `state_dir`.
    fn ca_dir(&self) -> PathBuf {
        let slug: String = self
            .directory_url
            .trim_start_matches("https://")
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.state_dir.join(slug.trim_matches('_'))
    }

    fn account_path(&self) -> PathBuf {
        self.ca_dir().join("account.json")
    }

    /// Where the certificate for these domains is kept.
    pub fn cert_path(&self) -> PathBuf {
        let first = self.domains.first().map_or("certificate", String::as_str);
        let name = first.replace("*.", "_wildcard.");
        self.ca_dir().join("certs").join(format!("{name}.pem"))
    }

    fn contact(&self) -> Vec<String> {
        self.contact_email
            .iter()
            .map(|e| format!("mailto:{e}"))
            .collect()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AcmeError {
    #[error("{0}")]
    Config(String),
    #[error("ACME: {0}")]
    Protocol(#[from] instant_acme::Error),
    #[error("{0}")]
    Validation(String),
    #[error("DNS provider: {0}")]
    Dns(#[from] DnsError),
    #[error(transparent)]
    Tls(#[from] TlsError),
}

/// Progress, for the API and logs. Times are Unix milliseconds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcmeStatus {
    pub domains: Vec<String>,
    pub directory_url: String,
    /// When the next issuance or renewal starts.
    pub next_attempt_ms: Option<i64>,
    pub last_issued_ms: Option<i64>,
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
}

/// A running ACME manager. Dropping it doesn't stop it; call [`AcmeHandle::stop`].
#[derive(Debug)]
pub struct AcmeHandle {
    status: watch::Receiver<AcmeStatus>,
    task: JoinHandle<()>,
}

impl AcmeHandle {
    pub fn status(&self) -> watch::Receiver<AcmeStatus> {
        self.status.clone()
    }

    pub fn stop(&self) {
        self.task.abort();
    }
}

#[derive(Serialize, Deserialize)]
struct StoredAccount {
    credentials: AccountCredentials,
    contact: Vec<String>,
}

/// An ACME manager, set up but not started. Start it once the listeners that answer
/// challenges are bound, or the first validation can fail.
#[derive(Debug)]
pub struct AcmeManager {
    store: CertStore,
    config: AcmeConfig,
    http: http::ReqwestClient,
    status: watch::Sender<AcmeStatus>,
}

impl AcmeManager {
    /// Validate `config` and install any stored certificate that is still valid.
    pub fn new(store: CertStore, mut config: AcmeConfig) -> Result<Self, AcmeError> {
        config.validate()?;
        let http = http::ReqwestClient::new(&config.directory_roots).map_err(AcmeError::Config)?;
        match load_stored(&config, SystemTime::now()) {
            Ok(Some(loaded)) => {
                tracing::info!(
                    names = ?loaded.info.dns_names,
                    expires = %humantime_utc(loaded.info.not_after),
                    "loaded the stored ACME certificate"
                );
                store.install(loaded, CertSource::Acme);
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "ignoring the stored ACME certificate"),
        }
        let status = watch::Sender::new(AcmeStatus {
            domains: config.domains.clone(),
            directory_url: config.directory_url.clone(),
            ..AcmeStatus::default()
        });
        Ok(Self {
            store,
            config,
            http,
            status,
        })
    }

    pub fn status(&self) -> watch::Receiver<AcmeStatus> {
        self.status.subscribe()
    }

    /// Issue and renew in the background.
    pub fn start(self) -> AcmeHandle {
        let status = self.status.subscribe();
        let task = tokio::spawn(run(self.store, self.config, self.http, self.status));
        AcmeHandle { status, task }
    }
}

/// The stored certificate, if there is one and it is valid now.
fn load_stored(config: &AcmeConfig, now: SystemTime) -> Result<Option<LoadedCert>, TlsError> {
    let path = config.cert_path();
    let Some(bundle) = read_optional(&path)? else {
        return Ok(None);
    };
    let loaded = load_pem(&bundle, &bundle)?;
    Ok(loaded.info.is_valid_at(now).then_some(loaded))
}

fn unix_ms(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

async fn run(
    store: CertStore,
    config: AcmeConfig,
    http: http::ReqwestClient,
    status: watch::Sender<AcmeStatus>,
) {
    let mut failures = 0u32;
    let mut last_failure = None;
    let mut jitter: f64 = rand::random();
    loop {
        let held = store
            .current()
            .filter(|c| c.source == CertSource::Acme)
            .map(|c| schedule::Held {
                not_before: c.info.not_before,
                not_after: c.info.not_after,
                matches_config: c.info.names_cover(&config.domains)
                    && c.info.dns_names.len() == config.domains.len(),
            });
        let when = schedule::next_attempt(held, failures, last_failure, jitter);
        status.send_modify(|s| s.next_attempt_ms = Some(unix_ms(when.max(SystemTime::now()))));
        if when > SystemTime::now() {
            tracing::info!(at = %humantime_utc(when), "next ACME certificate renewal");
        }
        sleep_until(when).await;

        tracing::info!(domains = ?config.domains, directory = %config.directory_url, "requesting an ACME certificate");
        match issue(&config, &store, &http).await {
            Ok(loaded) => {
                tracing::info!(
                    names = ?loaded.info.dns_names,
                    expires = %humantime_utc(loaded.info.not_after),
                    "installed a new ACME certificate"
                );
                store.install(loaded, CertSource::Acme);
                failures = 0;
                last_failure = None;
                jitter = rand::random();
                status.send_modify(|s| {
                    s.last_issued_ms = Some(unix_ms(SystemTime::now()));
                    s.last_error = None;
                    s.consecutive_failures = 0;
                });
            }
            Err(e) => {
                failures = failures.saturating_add(1);
                let now = SystemTime::now();
                last_failure = Some(now);
                let retry = schedule::next_attempt(held, failures, last_failure, jitter);
                tracing::error!(
                    error = %e,
                    failures,
                    retry_at = %humantime_utc(retry),
                    "ACME CERTIFICATE REQUEST FAILED"
                );
                status.send_modify(|s| {
                    s.last_error = Some(e.to_string());
                    s.consecutive_failures = failures;
                });
            }
        }
    }
}

/// Sleep until a wall-clock time. Sleeps in slices of at most an hour so a suspended or
/// adjusted clock is noticed (a monotonic sleep alone would overshoot after suspend).
async fn sleep_until(when: SystemTime) {
    loop {
        let Ok(left) = when.duration_since(SystemTime::now()) else {
            return;
        };
        if left.is_zero() {
            return;
        }
        tokio::time::sleep(left.min(Duration::from_secs(3600))).await;
    }
}

/// Load the account, or create it on first use, and keep its contact address current.
async fn account(config: &AcmeConfig, http: &http::ReqwestClient) -> Result<Account, AcmeError> {
    let path = config.account_path();
    let contact = config.contact();
    if let Some(bytes) = read_optional(&path)? {
        // `AccountCredentials` isn't `Clone`, so parse again when it has to be saved back.
        let parse = || {
            serde_json::from_slice::<StoredAccount>(&bytes)
                .map_err(|e| AcmeError::Config(format!("{}: {e}", path.display())))
        };
        let stored = parse()?;
        let account = Account::builder_with_http(Box::new(http.clone()))
            .from_credentials(stored.credentials)
            .await?;
        if stored.contact != contact {
            let refs: Vec<&str> = contact.iter().map(String::as_str).collect();
            account.update_contacts(&refs).await?;
            let mut updated = parse()?;
            updated.contact = contact;
            save_account(&path, &updated)?;
            tracing::info!("updated the ACME account's contact address");
        }
        return Ok(account);
    }
    let refs: Vec<&str> = contact.iter().map(String::as_str).collect();
    // Using ACME with this CA means accepting its terms of service; the README says so.
    let (account, credentials) = Account::builder_with_http(Box::new(http.clone()))
        .create(
            &NewAccount {
                contact: &refs,
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            config.directory_url.clone(),
            None,
        )
        .await?;
    save_account(
        &path,
        &StoredAccount {
            credentials,
            contact,
        },
    )?;
    tracing::info!(id = %account.id(), directory = %config.directory_url, "created an ACME account");
    Ok(account)
}

fn save_account(path: &Path, stored: &StoredAccount) -> Result<(), AcmeError> {
    let json = serde_json::to_vec_pretty(stored).map_err(|e| AcmeError::Config(e.to_string()))?;
    write_atomic(path, &json, true)?;
    Ok(())
}

/// Something provisioned for a challenge, to take down afterwards.
enum Provisioned {
    Http(String),
    Alpn(String),
    Dns(Arc<dyn DnsProvider>, TxtRecord),
}

/// Run one order to completion and store the result.
async fn issue(
    config: &AcmeConfig,
    store: &CertStore,
    http: &http::ReqwestClient,
) -> Result<LoadedCert, AcmeError> {
    let account = account(config, http).await?;
    let mut provisioned = Vec::new();
    let result = order(config, store, &account, &mut provisioned).await;
    for p in provisioned {
        match p {
            Provisioned::Http(token) => store.remove_http01(&token),
            Provisioned::Alpn(host) => store.remove_alpn_challenge(&host),
            Provisioned::Dns(provider, record) => {
                if let Err(e) = provider.remove_txt(&record).await {
                    tracing::warn!(record = %record.name, error = %e, "could not remove an ACME TXT record");
                }
            }
        }
    }
    let (chain, key_pem) = result?;
    let mut bundle = chain.trim_end().to_owned();
    bundle.push('\n');
    bundle.push_str(&key_pem);
    let loaded = load_pem(bundle.as_bytes(), bundle.as_bytes())?;
    write_atomic(&config.cert_path(), bundle.as_bytes(), true)?;
    Ok(loaded)
}

/// The order itself: returns the PEM chain and the PEM private key.
async fn order(
    config: &AcmeConfig,
    store: &CertStore,
    account: &Account,
    provisioned: &mut Vec<Provisioned>,
) -> Result<(String, String), AcmeError> {
    let identifiers: Vec<Identifier> = config
        .domains
        .iter()
        .map(|d| Identifier::Dns(d.clone()))
        .collect();
    let mut order = account.new_order(&NewOrder::new(&identifiers)).await?;
    let kind = config.challenge.acme_type();

    let mut authorizations = order.authorizations();
    while let Some(authz) = authorizations.next().await {
        let mut authz = authz?;
        match authz.status {
            AuthorizationStatus::Valid => continue,
            AuthorizationStatus::Pending => {}
            other => {
                return Err(AcmeError::Validation(format!(
                    "authorization for {} is {other:?}",
                    authz.identifier()
                )));
            }
        }
        let name = authz.identifier().to_string();
        let mut challenge = authz.challenge(kind.clone()).ok_or_else(|| {
            AcmeError::Validation(format!(
                "the CA offers no {:?} challenge for {name}",
                config.challenge
            ))
        })?;
        let key_auth = challenge.key_authorization();
        let host = name.trim_start_matches("*.").to_owned();
        match &config.challenge {
            ChallengeKind::Http01 => {
                store.set_http01(&challenge.token, key_auth.as_str());
                provisioned.push(Provisioned::Http(challenge.token.clone()));
            }
            ChallengeKind::TlsAlpn01 => {
                let cert = alpn_challenge_cert(&host, key_auth.digest().as_ref())?;
                store.set_alpn_challenge(&host, cert);
                provisioned.push(Provisioned::Alpn(host.clone()));
            }
            ChallengeKind::Dns01(provider) => {
                let record = TxtRecord::for_domain(&host, key_auth.dns_value());
                provider.publish_txt(&record).await?;
                provisioned.push(Provisioned::Dns(provider.clone(), record));
            }
        }
        challenge.set_ready().await?;
    }

    let retry = RetryPolicy::new()
        .initial_delay(Duration::from_secs(1))
        .backoff(1.5)
        .timeout(config.poll_timeout);
    let status = order.poll_ready(&retry).await?;
    if status != OrderStatus::Ready {
        return Err(AcmeError::Validation(
            validation_errors(&mut order, &kind, status).await,
        ));
    }

    let key = rcgen::KeyPair::generate().map_err(TlsError::from)?;
    let mut params =
        rcgen::CertificateParams::new(config.domains.clone()).map_err(TlsError::from)?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    let csr = params.serialize_request(&key).map_err(TlsError::from)?;
    order.finalize_csr(csr.der()).await?;
    let chain = order.poll_certificate(&retry).await?;
    Ok((chain, key.serialize_pem()))
}

/// Why an order didn't become ready, from the order's and challenges' problem documents.
async fn validation_errors(
    order: &mut instant_acme::Order,
    kind: &ChallengeType,
    status: OrderStatus,
) -> String {
    let mut msgs = Vec::new();
    if let Some(p) = &order.state().error {
        msgs.push(p.to_string());
    }
    let mut authorizations = order.authorizations();
    while let Some(Ok(mut authz)) = authorizations.next().await {
        let name = authz.identifier().to_string();
        if let Ok(state) = authz.refresh().await {
            for c in &state.challenges {
                if &c.r#type == kind
                    && let Some(e) = &c.error
                {
                    msgs.push(format!("{name}: {e}"));
                }
            }
        }
    }
    if msgs.is_empty() {
        format!("the order is {status:?}")
    } else {
        format!("the order is {status:?}: {}", msgs.join("; "))
    }
}

/// The TLS-ALPN-01 challenge certificate (RFC 8737 §3): self-signed, naming `host`, with
/// the critical `acmeIdentifier` extension holding the key authorization's SHA-256.
fn alpn_challenge_cert(host: &str, digest: &[u8]) -> Result<Arc<CertifiedKey>, TlsError> {
    let key = rcgen::KeyPair::generate()?;
    let mut params = rcgen::CertificateParams::new(vec![host.to_owned()])?;
    params.custom_extensions = vec![rcgen::CustomExtension::new_acme_identifier(digest)];
    let cert = params.self_signed(&key)?;
    let signing = provider()
        .key_provider
        .load_private_key(PrivateKeyDer::Pkcs8(key.serialize_der().into()))?;
    Ok(Arc::new(CertifiedKey::new(
        vec![cert.der().clone()],
        signing,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(domains: &[&str], challenge: ChallengeKind) -> AcmeConfig {
        AcmeConfig::new(
            domains.iter().map(|d| (*d).to_owned()).collect(),
            PathBuf::from("state"),
            challenge,
        )
    }

    #[derive(Debug)]
    struct NoDns;
    impl DnsProvider for NoDns {
        fn publish_txt<'a>(&'a self, _: &'a TxtRecord) -> BoxFuture<'a, Result<(), DnsError>> {
            Box::pin(async { Ok(()) })
        }
        fn remove_txt<'a>(&'a self, _: &'a TxtRecord) -> BoxFuture<'a, Result<(), DnsError>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[test]
    fn validates_domains() {
        let mut c = cfg(
            &["Torrents.Example.com.", "www.example.com"],
            ChallengeKind::TlsAlpn01,
        );
        c.contact_email = Some("mailto:me@example.com".into());
        c.validate().unwrap();
        assert_eq!(c.domains, ["torrents.example.com", "www.example.com"]);
        assert_eq!(c.contact(), ["mailto:me@example.com"]);
        for bad in [
            "localhost",
            "192.168.1.2",
            "a..b",
            "-a.example.com",
            "a.example.com:443",
            "",
        ] {
            assert!(
                cfg(&[bad], ChallengeKind::Http01).validate().is_err(),
                "{bad}"
            );
        }
        assert!(cfg(&[], ChallengeKind::Http01).validate().is_err());
        assert!(
            cfg(&["a.example", "A.example"], ChallengeKind::Http01)
                .validate()
                .is_err()
        );
        assert!(
            cfg(&["*.example.com"], ChallengeKind::Http01)
                .validate()
                .is_err()
        );
        cfg(&["*.example.com"], ChallengeKind::Dns01(Arc::new(NoDns)))
            .validate()
            .unwrap();
        let mut c = cfg(&["a.example"], ChallengeKind::Http01);
        c.contact_email = Some("not an email".into());
        assert!(c.validate().is_err());
        let mut c = cfg(&["a.example"], ChallengeKind::Http01);
        c.directory_url = "http://insecure/directory".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn storage_paths_separate_cas() {
        let mut prod = cfg(&["*.example.com"], ChallengeKind::Dns01(Arc::new(NoDns)));
        prod.validate().unwrap();
        let mut staging = prod.clone();
        staging.directory_url = LETS_ENCRYPT_STAGING.into();
        assert_ne!(prod.ca_dir(), staging.ca_dir());
        assert_eq!(
            prod.ca_dir(),
            Path::new("state").join("acme-v02.api.letsencrypt.org_directory")
        );
        assert_eq!(
            prod.cert_path(),
            prod.ca_dir()
                .join("certs")
                .join("_wildcard.example.com.pem")
        );
    }

    #[test]
    fn alpn_challenge_certificate() {
        let digest = [7u8; 32];
        let cert = alpn_challenge_cert("a.example", &digest).unwrap();
        // (rustls's `keys_match` can't check it: webpki rejects the unknown critical
        // extension, which is exactly what makes it useless outside the validation.)
        let der = cert.end_entity_cert().unwrap();
        let (_, parsed) = x509_parser::parse_x509_certificate(der).unwrap();
        // id-pe-acmeIdentifier, critical, an OCTET STRING of the digest.
        let ext = parsed
            .extensions()
            .iter()
            .find(|e| e.oid.to_id_string() == "1.3.6.1.5.5.7.1.31")
            .unwrap();
        assert!(ext.critical);
        assert_eq!(ext.value, [&[0x04, 32][..], &digest].concat());
    }

    #[test]
    fn stored_certificate_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cfg(&["a.example"], ChallengeKind::Http01);
        c.state_dir = dir.path().to_owned();
        c.validate().unwrap();
        assert!(load_stored(&c, SystemTime::now()).unwrap().is_none());
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["a.example".to_owned()]).unwrap();
        let now = SystemTime::now();
        params.not_before = (now - Duration::from_secs(3600)).into();
        params.not_after = (now + Duration::from_secs(90 * 24 * 3600)).into();
        let cert = params.self_signed(&key).unwrap();
        let bundle = format!("{}{}", cert.pem(), key.serialize_pem());
        write_atomic(&c.cert_path(), bundle.as_bytes(), true).unwrap();
        let loaded = load_stored(&c, SystemTime::now()).unwrap().unwrap();
        assert_eq!(loaded.info.dns_names, ["a.example"]);
        // Expired: not used.
        let later = now + Duration::from_secs(91 * 24 * 3600);
        assert!(load_stored(&c, later).unwrap().is_none());
    }
}

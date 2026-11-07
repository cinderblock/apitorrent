//! Users, credentials and sessions for apitorrent.
//!
//! - **Passkeys (WebAuthn) are primary.** They are discoverable credentials, so sign-in is
//!   usernameless and the login endpoints never reveal which accounts exist. A user may have
//!   several, each named and revocable. Each passkey belongs to one relying party (host).
//! - **Passwords are optional** (argon2id): off, a fallback that signs in alone, or a second
//!   factor after the passkey, per [`PasswordMode`].
//! - **Bootstrap**: while no user exists, a one-time setup token (printed on the daemon's
//!   console) lets the first admin enroll. It expires and is replaced while unused.
//! - **Sessions**: a 256-bit random id (the cookie), stored as its SHA-256, with idle and
//!   absolute expiry. Every login, re-authentication and second factor issues a new id.
//! - **API tokens**: named, scoped (`read` or `full`), shown once, stored hashed, revocable.
//! - **Rate limits and lockouts** on everything that takes a credential.
//!
//! Everything lives in its own SQLite file, `auth.db`. The crate is synchronous: password
//! hashing takes a moment, so async callers should run those calls on a blocking thread.

mod clock;
mod db;
mod error;
mod limit;
mod password;
mod secret;
mod store;

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::watch;

use rusqlite::Connection;
use serde_json::Value;
use url::Url;
use webauthn_rs::prelude::{
    CreationChallengeResponse, DiscoverableAuthentication, Passkey, PasskeyAuthentication,
    PasskeyRegistration, PublicKeyCredential, RegisterPublicKeyCredential,
    RequestChallengeResponse, Uuid, Webauthn, WebauthnBuilder,
};

pub use apitorrent_types::auth::{
    ApiTokenInfo, CeremonyStart, CreateTokenRequest, CreatedToken, PasskeyInfo, PasswordMode, Role,
    SessionInfo, SessionMethod, SessionState, TokenScope, UserInfo,
};
pub use clock::{Clock, ManualClock, SystemClock};
pub use error::{AuthError, Result};
pub use password::PasswordCost;

use crate::clock::ms;
use crate::secret::Digest32;
use crate::store::{PasskeyRow, SessionRow, TokenRow, UserRow};

/// The prefix of every API token, so they're recognisable in configs and secret scanners.
pub const TOKEN_PREFIX: &str = "apt_";

/// How long a passkey ceremony may take between its start and finish calls.
const CEREMONY_TTL: Duration = Duration::from_secs(5 * 60);
/// Pending ceremonies beyond this are refused, so nobody can fill memory with them.
const MAX_CEREMONIES: usize = 1024;
/// A session waiting for its second factor lives this long.
const PENDING_LIFETIME: Duration = Duration::from_secs(5 * 60);
/// `last_seen_at` is written at most this often per session or token.
const TOUCH_EVERY: Duration = Duration::from_secs(60);
/// Wrong setup tokens tolerated before the current one is replaced.
const SETUP_MAX_FAILURES: u32 = 10;
/// Audit events are kept this long.
const EVENT_RETENTION: Duration = Duration::from_secs(365 * 24 * 3600);
const MAX_USER_AGENT: usize = 256;

/// Called with each new setup token, so the daemon can print it.
pub type SetupHook = Arc<dyn Fn(&SetupToken) + Send + Sync>;

pub struct AuthConfig {
    /// The SQLite file, normally `<data dir>/auth.db`.
    pub db_path: PathBuf,
    /// Public `https://` URLs the UI is served at (through a reverse proxy, say). Each one's
    /// host becomes a passkey relying party. `localhost` is always one.
    pub public_urls: Vec<Url>,
    /// Shown by authenticators next to the passkey.
    pub rp_name: String,
    pub password_mode: PasswordMode,
    pub password_cost: PasswordCost,
    /// A session ends after this long without requests.
    pub session_idle: Duration,
    /// A session ends this long after sign-in, however active.
    pub session_lifetime: Duration,
    /// How long after signing in sensitive account changes are allowed.
    pub reauth_window: Duration,
    pub setup_token_ttl: Duration,
    /// Also write the setup token here (owner-only on Unix), and delete it once used.
    pub setup_token_file: Option<PathBuf>,
    pub on_setup_token: Option<SetupHook>,
    pub clock: Arc<dyn Clock>,
}

impl AuthConfig {
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
            public_urls: Vec::new(),
            rp_name: "apitorrent".to_owned(),
            password_mode: PasswordMode::Off,
            password_cost: PasswordCost::default(),
            session_idle: Duration::from_secs(7 * 24 * 3600),
            session_lifetime: Duration::from_secs(30 * 24 * 3600),
            reauth_window: Duration::from_secs(10 * 60),
            setup_token_ttl: Duration::from_secs(3600),
            setup_token_file: None,
            on_setup_token: None,
            clock: Arc::new(SystemClock),
        }
    }
}

impl fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthConfig")
            .field("db_path", &self.db_path)
            .field("public_urls", &self.public_urls)
            .field("password_mode", &self.password_mode)
            .field("session_idle", &self.session_idle)
            .field("session_lifetime", &self.session_lifetime)
            .field("reauth_window", &self.reauth_window)
            .field("setup_token_ttl", &self.setup_token_ttl)
            .field("setup_token_file", &self.setup_token_file)
            .finish_non_exhaustive()
    }
}

/// A freshly issued setup token.
#[derive(Clone)]
pub struct SetupToken {
    pub token: String,
    pub expires_at: i64,
}

impl fmt::Debug for SetupToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SetupToken")
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Where a request came from, for rate limits and the audit log.
#[derive(Debug, Clone, Default)]
pub struct ClientMeta {
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
}

impl ClientMeta {
    fn ip_string(&self) -> Option<String> {
        self.ip.map(|ip| ip.to_string())
    }
}

/// A new session: set `secret` as the cookie value.
#[derive(Clone)]
pub struct NewSession {
    pub secret: String,
    /// How long the cookie should live.
    pub max_age: Duration,
    /// The password second factor is still needed.
    pub pending_second_factor: bool,
}

impl fmt::Debug for NewSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewSession")
            .field("secret", &"<redacted>")
            .field("max_age", &self.max_age)
            .field("pending_second_factor", &self.pending_second_factor)
            .finish()
    }
}

/// An authenticated caller.
#[derive(Debug, Clone)]
pub struct Principal {
    pub user: UserInfo,
    pub credential: Credential,
}

#[derive(Debug, Clone)]
pub enum Credential {
    Session(SessionCredential),
    Token {
        id: String,
        scope: TokenScope,
    },
    /// `--insecure-no-auth-loopback`: a loopback request with authentication disabled.
    Local,
}

#[derive(Clone)]
pub struct SessionCredential {
    hash: Digest32,
    pub public_id: String,
    pub state: SessionState,
    pub pending_second_factor: bool,
}

impl fmt::Debug for SessionCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionCredential")
            .field("public_id", &self.public_id)
            .field("state", &self.state)
            .field("pending_second_factor", &self.pending_second_factor)
            .finish_non_exhaustive()
    }
}

impl Principal {
    /// The synthetic admin used when authentication is disabled for loopback.
    pub fn local() -> Self {
        Self {
            user: UserInfo {
                id: "local".to_owned(),
                name: "local".to_owned(),
                display_name: "Local (authentication disabled)".to_owned(),
                role: Role::Admin,
                has_password: false,
            },
            credential: Credential::Local,
        }
    }

    pub fn session(&self) -> Option<&SessionCredential> {
        match &self.credential {
            Credential::Session(s) => Some(s),
            _ => None,
        }
    }

    /// Signed in with a passkey, but the password second factor is still missing. Such a
    /// session can do nothing except finish signing in, check its status or sign out.
    pub fn pending_second_factor(&self) -> bool {
        self.session().is_some_and(|s| s.pending_second_factor)
    }

    pub fn may_read(&self) -> bool {
        !self.pending_second_factor()
    }

    pub fn may_write(&self) -> bool {
        self.may_read()
            && self.user.role != Role::Viewer
            && !matches!(
                self.credential,
                Credential::Token {
                    scope: TokenScope::Read,
                    ..
                }
            )
    }
}

/// One passkey relying party.
struct Rp {
    id: String,
    webauthn: Webauthn,
}

enum Pending {
    Setup {
        reg: PasskeyRegistration,
        user_id: Uuid,
        username: String,
        display_name: String,
        generation: u64,
    },
    Login {
        auth: DiscoverableAuthentication,
    },
    Reauth {
        auth: PasskeyAuthentication,
        session: Digest32,
    },
    AddPasskey {
        reg: PasskeyRegistration,
        session: Digest32,
    },
}

struct Ceremony {
    rp_id: String,
    expires_at: i64,
    pending: Pending,
}

#[derive(Default)]
struct SetupState {
    current: Option<IssuedSetup>,
    generation: u64,
    failures: u32,
}

struct IssuedSetup {
    digest: Digest32,
    expires_at: i64,
}

struct Inner {
    db: Mutex<Connection>,
    rps: Vec<Rp>,
    public_urls: Vec<Url>,
    password_mode: PasswordMode,
    password_cost: PasswordCost,
    session_idle: Duration,
    session_lifetime: Duration,
    reauth_window: Duration,
    setup_token_ttl: Duration,
    setup_token_file: Option<PathBuf>,
    on_setup_token: Option<SetupHook>,
    clock: Arc<dyn Clock>,
    ceremonies: Mutex<HashMap<String, Ceremony>>,
    setup: Mutex<SetupState>,
    limiter: limit::Limiter,
    gate: password::Gate,
    /// Bumped whenever a session or token ends early, so open sockets re-check at once.
    revoked: watch::Sender<u64>,
}

/// The auth service. Cheap to clone.
#[derive(Clone)]
pub struct Auth {
    inner: Arc<Inner>,
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth")
            .field(
                "relying_parties",
                &self.inner.rps.iter().map(|r| &r.id).collect::<Vec<_>>(),
            )
            .field("password_mode", &self.inner.password_mode)
            .finish_non_exhaustive()
    }
}

fn internal(e: impl fmt::Display) -> AuthError {
    AuthError::Internal(e.to_string())
}

fn build_rps(name: &str, public_urls: &[Url]) -> Result<Vec<Rp>> {
    let localhost = Url::parse("http://localhost").expect("valid URL");
    let localhost_tls = Url::parse("https://localhost").expect("valid URL");
    let webauthn = WebauthnBuilder::new("localhost", &localhost)
        .map_err(internal)?
        .allow_any_port(true)
        .append_allowed_origin(&localhost_tls)
        .rp_name(name)
        .build()
        .map_err(internal)?;
    let mut rps = vec![Rp {
        id: "localhost".to_owned(),
        webauthn,
    }];
    for url in public_urls {
        let host = match url.host() {
            Some(url::Host::Domain(d)) => d.to_ascii_lowercase(),
            Some(_) => {
                return Err(AuthError::InvalidRequest(format!(
                    "{url}: an IP address can't be a passkey relying party; use a host name"
                )));
            }
            None => return Err(AuthError::InvalidRequest(format!("{url}: no host"))),
        };
        if host == "localhost" {
            continue;
        }
        if url.scheme() != "https" {
            return Err(AuthError::InvalidRequest(format!(
                "{url}: public URLs must be https; browsers only allow passkeys on secure origins"
            )));
        }
        if rps.iter().any(|r| r.id == host) {
            return Err(AuthError::InvalidRequest(format!(
                "{url}: host listed twice"
            )));
        }
        let origin = Url::parse(&url.origin().ascii_serialization()).map_err(internal)?;
        let webauthn = WebauthnBuilder::new(&host, &origin)
            .map_err(internal)?
            .rp_name(name)
            .build()
            .map_err(internal)?;
        rps.push(Rp { id: host, webauthn });
    }
    Ok(rps)
}

/// The host part of a `Host` header, lowercased, without its port.
pub fn host_without_port(authority: &str) -> String {
    let a = authority.trim().to_ascii_lowercase();
    if a.starts_with('[') {
        return a.find(']').map_or(a.clone(), |end| a[..=end].to_owned());
    }
    a.split(':').next().unwrap_or_default().to_owned()
}

/// Lowercase letters, digits, `.`, `_` and `-`; starting with a letter or digit; 1 to 64.
fn normalize_username(name: &str) -> Result<String> {
    let n = name.trim().to_ascii_lowercase();
    let ok = (1..=64).contains(&n.len())
        && n.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(n)
    } else {
        Err(AuthError::InvalidRequest(
            "usernames are 1 to 64 lowercase letters, digits, '.', '_' or '-', starting with a \
             letter or digit"
                .to_owned(),
        ))
    }
}

/// A label for display: trimmed, 1 to 64 characters, no control characters.
fn clean_label(label: &str, what: &str) -> Result<String> {
    let l = label.trim();
    if l.is_empty() || l.chars().count() > 64 || l.chars().any(char::is_control) {
        return Err(AuthError::InvalidRequest(format!(
            "{what} must be 1 to 64 characters without control characters"
        )));
    }
    Ok(l.to_owned())
}

fn optional_label(label: Option<&str>, default: &str, what: &str) -> Result<String> {
    match label.map(str::trim) {
        None | Some("") => Ok(default.to_owned()),
        Some(l) => clean_label(l, what),
    }
}

/// The inner `publicKey` options, asking for a discoverable credential so sign-in can be
/// usernameless. webauthn-rs leaves resident keys optional.
fn registration_options(ccr: &CreationChallengeResponse) -> Result<Value> {
    let mut v = serde_json::to_value(ccr).map_err(internal)?;
    let pk = v
        .get_mut("publicKey")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| internal("registration options have no publicKey"))?;
    let sel = pk
        .entry("authenticatorSelection")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if let Some(sel) = sel.as_object_mut() {
        sel.insert("residentKey".into(), Value::from("required"));
        sel.insert("requireResidentKey".into(), Value::from(true));
    }
    Ok(v["publicKey"].take())
}

/// The inner `publicKey` options. webauthn-rs marks discoverable requests as conditional
/// mediation (autofill); the button flow is a plain modal request, so that is dropped.
fn authentication_options(rcr: &RequestChallengeResponse) -> Result<Value> {
    let mut v = serde_json::to_value(rcr).map_err(internal)?;
    let pk = v["publicKey"].take();
    if pk.is_null() {
        return Err(internal("authentication options have no publicKey"));
    }
    Ok(pk)
}

fn parse<T: serde::de::DeserializeOwned>(v: &Value) -> Result<T> {
    serde_json::from_value(v.clone())
        .map_err(|e| AuthError::InvalidRequest(format!("malformed credential: {e}")))
}

fn load_user(conn: &Connection, id: &str) -> Result<UserRow> {
    store::user_by_id(conn, id)?.ok_or(AuthError::Unauthorized)
}

/// A full (not pending) cookie session. API tokens and the no-auth local user can't
/// manage credentials.
fn account_session(p: &Principal) -> Result<&SessionCredential> {
    match p.session() {
        Some(s) if !s.pending_second_factor => Ok(s),
        Some(_) => Err(AuthError::Forbidden("finish signing in first")),
        None => Err(AuthError::Forbidden(
            "account changes need a signed-in browser session, not an API token",
        )),
    }
}

fn truncate(s: Option<&str>, max: usize) -> Option<String> {
    s.map(|s| s.chars().take(max).collect())
}

impl Auth {
    /// Open (or create) `auth.db` and set up the relying parties. If no user exists yet, a
    /// setup token is issued (and passed to [`AuthConfig::on_setup_token`]).
    pub fn open(config: AuthConfig) -> Result<Self> {
        let rps = build_rps(&config.rp_name, &config.public_urls)?;
        if let Some(parent) = config.db_path.parent() {
            std::fs::create_dir_all(parent).map_err(internal)?;
        }
        let conn = db::open(&config.db_path)?;
        let now = config.clock.now_ms();
        store::prune_events(&conn, now - ms(EVENT_RETENTION))?;
        store::delete_expired_sessions(&conn, now, ms(config.session_idle))?;
        let auth = Self {
            inner: Arc::new(Inner {
                db: Mutex::new(conn),
                rps,
                public_urls: config.public_urls,
                password_mode: config.password_mode,
                password_cost: config.password_cost,
                session_idle: config.session_idle,
                session_lifetime: config.session_lifetime,
                reauth_window: config.reauth_window,
                setup_token_ttl: config.setup_token_ttl,
                setup_token_file: config.setup_token_file,
                on_setup_token: config.on_setup_token,
                limiter: limit::Limiter::new(config.clock.clone()),
                clock: config.clock,
                ceremonies: Mutex::new(HashMap::new()),
                setup: Mutex::new(SetupState::default()),
                gate: password::Gate::new(2),
                revoked: watch::Sender::new(0),
            }),
        };
        auth.tick()?;
        Ok(auth)
    }

    fn now(&self) -> i64 {
        self.inner.clock.now_ms()
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        self.inner.db.lock().expect("auth db lock")
    }

    /// Changes whenever a session or token is revoked, replaced or signed out. Long-lived
    /// connections watch it and re-check their credential with [`Auth::still_valid`].
    pub fn revocations(&self) -> watch::Receiver<u64> {
        self.inner.revoked.subscribe()
    }

    fn bump_revocations(&self) {
        self.inner.revoked.send_modify(|v| *v = v.wrapping_add(1));
    }

    pub fn password_mode(&self) -> PasswordMode {
        self.inner.password_mode
    }

    /// No user exists yet.
    pub fn setup_required(&self) -> Result<bool> {
        Ok(store::user_count(&self.db())? == 0)
    }

    /// Whether passkeys work for requests to `host` (a `Host` header value).
    pub fn passkeys_available(&self, host: &str) -> bool {
        self.rp(host).is_ok()
    }

    /// The configured public URL, if any: where to go for passkeys when they aren't
    /// available on the current host.
    pub fn public_url(&self) -> Option<&Url> {
        self.inner.public_urls.first()
    }

    fn rp(&self, host: &str) -> Result<&Rp> {
        let host = host_without_port(host);
        self.inner.rps.iter().find(|r| r.id == host).ok_or_else(|| {
            AuthError::Unavailable(format!(
                "passkeys aren't available on {host}; open this page through localhost or \
                     the configured public URL"
            ))
        })
    }

    // ----- Setup token -----

    /// Housekeeping: issue a setup token while none is valid and no user exists, and drop
    /// it once one does. Call it periodically (the daemon does, every 30 seconds).
    pub fn tick(&self) -> Result<()> {
        let needed = self.setup_required()?;
        let mut setup = self.inner.setup.lock().expect("setup lock");
        let now = self.now();
        if needed {
            let valid = setup.current.as_ref().is_some_and(|c| c.expires_at > now);
            if !valid {
                self.issue_setup_token(&mut setup, now);
            }
        } else if setup.current.take().is_some() {
            self.remove_setup_file();
        }
        Ok(())
    }

    fn issue_setup_token(&self, setup: &mut SetupState, now: i64) {
        let token = secret::readable_token();
        let expires_at = now + ms(self.inner.setup_token_ttl);
        setup.generation += 1;
        setup.failures = 0;
        setup.current = Some(IssuedSetup {
            digest: secret::sha256(secret::normalize_readable(&token).as_bytes()),
            expires_at,
        });
        if let Some(path) = &self.inner.setup_token_file
            && let Err(e) = write_private(path, &format!("{token}\n"))
        {
            tracing::warn!(path = %path.display(), error = %e, "could not write the setup token file");
        }
        let issued = SetupToken { token, expires_at };
        if let Some(hook) = &self.inner.on_setup_token {
            hook(&issued);
        }
    }

    fn remove_setup_file(&self) {
        if let Some(path) = &self.inner.setup_token_file {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Check a setup token; returns the token generation it belongs to.
    fn check_setup_token(&self, token: &str, meta: &ClientMeta) -> Result<u64> {
        if !self.setup_required()? {
            return Err(AuthError::AlreadySetUp);
        }
        self.tick()?;
        let mut setup = self.inner.setup.lock().expect("setup lock");
        let now = self.now();
        let given = secret::sha256(secret::normalize_readable(token).as_bytes());
        let ok = setup
            .current
            .as_ref()
            .is_some_and(|c| c.expires_at > now && secret::digests_equal(&c.digest, &given));
        if ok {
            return Ok(setup.generation);
        }
        setup.failures += 1;
        let addr = limit::address_key(meta.ip);
        self.inner.limiter.fail(&addr, &limit::ADDRESS);
        if setup.failures >= SETUP_MAX_FAILURES {
            tracing::warn!(
                "too many wrong setup tokens; issuing a new one (the old one no longer works)"
            );
            self.issue_setup_token(&mut setup, now);
        }
        drop(setup);
        self.log(
            "setup_token_failed",
            None,
            meta,
            "wrong or expired setup token",
        );
        Err(AuthError::InvalidSetupToken)
    }

    /// Whether `generation` is still the live token. Called under the database lock, inside
    /// the transaction that creates the first user.
    fn setup_generation_live(&self, generation: u64) -> bool {
        let setup = self.inner.setup.lock().expect("setup lock");
        setup.generation == generation
            && setup
                .current
                .as_ref()
                .is_some_and(|c| c.expires_at > self.now())
    }

    fn finish_setup_token(&self) {
        let mut setup = self.inner.setup.lock().expect("setup lock");
        setup.current = None;
        drop(setup);
        self.remove_setup_file();
    }

    // ----- Helpers -----

    fn limit_address(&self, meta: &ClientMeta) -> Result<()> {
        self.inner
            .limiter
            .check(&limit::address_key(meta.ip), &limit::ADDRESS)
    }

    /// Record a failed attempt against the client's address and return the error to send.
    fn failed(&self, meta: &ClientMeta, user: Option<&str>, kind: &str, detail: &str) -> AuthError {
        let addr = limit::address_key(meta.ip);
        if self.inner.limiter.fail(&addr, &limit::ADDRESS) {
            tracing::warn!(addr, "auth lockout after repeated failures");
            self.log("lockout", user, meta, &addr);
        }
        self.log(kind, user, meta, detail);
        AuthError::Unauthorized
    }

    fn log(&self, kind: &str, user: Option<&str>, meta: &ClientMeta, detail: &str) {
        let conn = self.db();
        self.log_with(&conn, kind, user, meta, detail);
    }

    fn log_with(
        &self,
        conn: &Connection,
        kind: &str,
        user: Option<&str>,
        meta: &ClientMeta,
        detail: &str,
    ) {
        if let Err(e) = store::event(
            conn,
            self.now(),
            kind,
            user,
            meta.ip_string().as_deref(),
            detail,
        ) {
            tracing::warn!(error = %e, "could not write an auth event");
        }
    }

    fn store_ceremony(&self, rp_id: &str, pending: Pending) -> Result<String> {
        let now = self.now();
        let mut map = self.inner.ceremonies.lock().expect("ceremony lock");
        if map.len() >= MAX_CEREMONIES {
            map.retain(|_, c| c.expires_at > now);
            if map.len() >= MAX_CEREMONIES {
                return Err(AuthError::RateLimited(Duration::from_secs(30)));
            }
        }
        let id = secret::token256();
        map.insert(
            id.clone(),
            Ceremony {
                rp_id: rp_id.to_owned(),
                expires_at: now + ms(CEREMONY_TTL),
                pending,
            },
        );
        Ok(id)
    }

    /// Ceremonies are single-use: taking one removes it, whatever happens next.
    fn take_ceremony(&self, id: &str, rp_id: &str) -> Result<Pending> {
        let now = self.now();
        let c = self
            .inner
            .ceremonies
            .lock()
            .expect("ceremony lock")
            .remove(id)
            .ok_or_else(|| {
                AuthError::InvalidRequest("unknown or expired ceremony; start again".into())
            })?;
        if c.expires_at <= now || c.rp_id != rp_id {
            return Err(AuthError::InvalidRequest(
                "unknown or expired ceremony; start again".into(),
            ));
        }
        Ok(c.pending)
    }

    fn new_session(
        &self,
        conn: &Connection,
        user_id: &str,
        method: SessionMethod,
        pending: bool,
        meta: &ClientMeta,
        replaces: Option<&Principal>,
    ) -> Result<NewSession> {
        let now = self.now();
        if let Some(old) = replaces.and_then(Principal::session) {
            store::delete_session(conn, &old.hash)?;
            self.bump_revocations();
        }
        store::delete_expired_sessions(conn, now, ms(self.inner.session_idle))?;
        let secret = secret::token256();
        let lifetime = if pending {
            PENDING_LIFETIME
        } else {
            self.inner.session_lifetime
        };
        store::insert_session(
            conn,
            &secret::sha256(secret.as_bytes()),
            &SessionRow {
                public_id: secret::public_id(),
                user_id: user_id.to_owned(),
                method,
                pending_second_factor: pending,
                created_at: now,
                last_seen_at: now,
                authenticated_at: now,
                expires_at: now + ms(lifetime),
                user_agent: truncate(meta.user_agent.as_deref(), MAX_USER_AGENT),
                ip: meta.ip_string(),
            },
        )?;
        Ok(NewSession {
            secret,
            max_age: lifetime,
            pending_second_factor: pending,
        })
    }

    /// A full session that signed in (or re-authenticated) recently.
    fn recent_session<'p>(&self, p: &'p Principal) -> Result<&'p SessionCredential> {
        let s = account_session(p)?;
        if s.state.reauth_until <= self.now() {
            return Err(AuthError::ReauthRequired);
        }
        Ok(s)
    }

    fn verify_password(&self, password: &str, phc: Option<&str>) -> Result<bool> {
        let _slot = self.inner.gate.enter()?;
        if let Some(h) = phc {
            Ok(password::verify(password, h))
        } else {
            password::verify_dummy(password, self.inner.password_cost);
            Ok(false)
        }
    }

    fn hash_password(&self, password: &str) -> Result<String> {
        password::validate(password)?;
        let _slot = self.inner.gate.enter()?;
        password::hash(password, self.inner.password_cost)
    }

    /// After a successful verification, upgrade a hash made with older parameters.
    fn maybe_rehash(&self, user: &UserRow, password: &str) {
        let Some(h) = &user.password_hash else { return };
        if !password::needs_rehash(h, self.inner.password_cost) {
            return;
        }
        match self.hash_password(password) {
            Ok(new) => {
                if let Err(e) = store::set_password_hash(&self.db(), &user.id, Some(&new)) {
                    tracing::warn!(error = %e, "could not upgrade a password hash");
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not upgrade a password hash"),
        }
    }

    // ----- Authenticating requests -----

    /// The principal for a session cookie value, if it is a live session.
    pub fn authenticate_session(&self, secret: &str) -> Result<Option<Principal>> {
        if secret.len() != 43 {
            return Ok(None);
        }
        let hash = secret::sha256(secret.as_bytes());
        let now = self.now();
        let conn = self.db();
        let Some(row) = store::session_by_hash(&conn, &hash)? else {
            return Ok(None);
        };
        let idle =
            !row.pending_second_factor && now - row.last_seen_at > ms(self.inner.session_idle);
        if row.expires_at <= now || idle {
            store::delete_session(&conn, &hash)?;
            return Ok(None);
        }
        if now - row.last_seen_at >= ms(TOUCH_EVERY) {
            store::touch_session(&conn, &hash, now)?;
        }
        let Some(user) = store::user_by_id(&conn, &row.user_id)? else {
            return Ok(None);
        };
        Ok(Some(Principal {
            user: user.info(),
            credential: Credential::Session(SessionCredential {
                hash,
                public_id: row.public_id,
                state: SessionState {
                    method: row.method,
                    created_at: row.created_at,
                    expires_at: row.expires_at,
                    reauth_until: row.authenticated_at + ms(self.inner.reauth_window),
                },
                pending_second_factor: row.pending_second_factor,
            }),
        }))
    }

    /// The principal for an `Authorization: Bearer` token, if it is a live API token.
    pub fn authenticate_token(&self, token: &str) -> Result<Option<Principal>> {
        if !token.starts_with(TOKEN_PREFIX) || token.len() > 128 {
            return Ok(None);
        }
        let hash = secret::sha256(token.as_bytes());
        let now = self.now();
        let conn = self.db();
        let Some(row) = store::token_by_hash(&conn, &hash)? else {
            return Ok(None);
        };
        if row.info.expires_at.is_some_and(|e| e <= now) {
            return Ok(None);
        }
        if row
            .info
            .last_used_at
            .is_none_or(|t| now - t >= ms(TOUCH_EVERY))
        {
            store::touch_token(&conn, &row.info.id, now)?;
        }
        let Some(user) = store::user_by_id(&conn, &row.user_id)? else {
            return Ok(None);
        };
        Ok(Some(Principal {
            user: user.info(),
            credential: Credential::Token {
                id: row.info.id,
                scope: row.info.scope,
            },
        }))
    }

    /// Whether a principal authenticated earlier is still valid (for a long-lived WebSocket).
    pub fn still_valid(&self, p: &Principal) -> Result<bool> {
        let conn = self.db();
        let now = self.now();
        Ok(match &p.credential {
            Credential::Local => true,
            Credential::Session(s) => store::session_by_hash(&conn, &s.hash)?.is_some_and(|row| {
                row.expires_at > now && now - row.last_seen_at <= ms(self.inner.session_idle)
            }),
            Credential::Token { id, .. } => store::tokens_for_user(&conn, &p.user.id)?
                .iter()
                .any(|t| &t.info.id == id && t.info.expires_at.is_none_or(|e| e > now)),
        })
    }

    // ----- Setup -----

    pub fn start_setup(
        &self,
        host: &str,
        token: &str,
        username: &str,
        display_name: Option<&str>,
        meta: &ClientMeta,
    ) -> Result<CeremonyStart> {
        self.limit_address(meta)?;
        let generation = self.check_setup_token(token, meta)?;
        let rp = self.rp(host)?;
        let username = normalize_username(username)?;
        let display_name = optional_label(display_name, &username, "display names")?;
        let user_id = Uuid::new_v4();
        let (ccr, reg) = rp
            .webauthn
            .start_passkey_registration(user_id, &username, &display_name, None)
            .map_err(internal)?;
        let options = registration_options(&ccr)?;
        let ceremony_id = self.store_ceremony(
            &rp.id,
            Pending::Setup {
                reg,
                user_id,
                username,
                display_name,
                generation,
            },
        )?;
        Ok(CeremonyStart {
            ceremony_id,
            options,
        })
    }

    pub fn finish_setup(
        &self,
        host: &str,
        ceremony_id: &str,
        credential: &Value,
        passkey_name: Option<&str>,
        meta: &ClientMeta,
    ) -> Result<NewSession> {
        self.limit_address(meta)?;
        let rp = self.rp(host)?;
        let Pending::Setup {
            reg,
            user_id,
            username,
            display_name,
            generation,
        } = self.take_ceremony(ceremony_id, &rp.id)?
        else {
            return Err(AuthError::InvalidRequest("not a setup ceremony".into()));
        };
        let name = optional_label(passkey_name, "Passkey", "passkey names")?;
        let cred: RegisterPublicKeyCredential = parse(credential)?;
        let passkey = rp
            .webauthn
            .finish_passkey_registration(&cred, &reg)
            .map_err(|e| {
                tracing::debug!(error = %e, "setup registration failed");
                self.failed(meta, None, "setup_failed", &e.to_string())
            })?;
        let user = UserRow {
            id: user_id.to_string(),
            name: username,
            display_name,
            role: Role::Admin,
            password_hash: None,
        };
        let mut conn = self.db();
        let tx = conn.transaction()?;
        if store::user_count(&tx)? > 0 {
            return Err(AuthError::AlreadySetUp);
        }
        if !self.setup_generation_live(generation) {
            return Err(AuthError::InvalidSetupToken);
        }
        let now = self.now();
        store::insert_user(&tx, &user, now)?;
        store::insert_passkey(
            &tx,
            &PasskeyRow {
                id: secret::public_id(),
                user_id: user.id.clone(),
                rp_id: rp.id.clone(),
                name,
                passkey: serde_json::to_string(&passkey).map_err(internal)?,
                created_at: now,
                last_used_at: Some(now),
            },
            passkey.cred_id().as_ref(),
        )?;
        let session = self.new_session(&tx, &user.id, SessionMethod::Passkey, false, meta, None)?;
        self.log_with(
            &tx,
            "setup",
            Some(&user.id),
            meta,
            "first admin enrolled with a passkey",
        );
        tx.commit()?;
        drop(conn);
        self.finish_setup_token();
        tracing::info!(user = %user.name, "setup complete: first admin enrolled");
        Ok(session)
    }

    /// Bootstrap with a password instead of a passkey ([`PasswordMode::Fallback`] only), for
    /// hosts where passkeys can't work.
    pub fn setup_with_password(
        &self,
        token: &str,
        username: &str,
        display_name: Option<&str>,
        password: &str,
        meta: &ClientMeta,
    ) -> Result<NewSession> {
        if self.inner.password_mode != PasswordMode::Fallback {
            return Err(AuthError::Unavailable(
                "password sign-in is disabled on this instance".into(),
            ));
        }
        self.limit_address(meta)?;
        let generation = self.check_setup_token(token, meta)?;
        let username = normalize_username(username)?;
        let display_name = optional_label(display_name, &username, "display names")?;
        let hash = self.hash_password(password)?;
        let user = UserRow {
            id: Uuid::new_v4().to_string(),
            name: username,
            display_name,
            role: Role::Admin,
            password_hash: Some(hash),
        };
        let mut conn = self.db();
        let tx = conn.transaction()?;
        if store::user_count(&tx)? > 0 {
            return Err(AuthError::AlreadySetUp);
        }
        if !self.setup_generation_live(generation) {
            return Err(AuthError::InvalidSetupToken);
        }
        store::insert_user(&tx, &user, self.now())?;
        let session =
            self.new_session(&tx, &user.id, SessionMethod::Password, false, meta, None)?;
        self.log_with(
            &tx,
            "setup",
            Some(&user.id),
            meta,
            "first admin enrolled with a password",
        );
        tx.commit()?;
        drop(conn);
        self.finish_setup_token();
        tracing::info!(user = %user.name, "setup complete: first admin enrolled");
        Ok(session)
    }

    // ----- Login -----

    pub fn start_login(&self, host: &str, meta: &ClientMeta) -> Result<CeremonyStart> {
        self.limit_address(meta)?;
        let rp = self.rp(host)?;
        let (rcr, auth) = rp
            .webauthn
            .start_discoverable_authentication()
            .map_err(internal)?;
        let options = authentication_options(&rcr)?;
        let ceremony_id = self.store_ceremony(&rp.id, Pending::Login { auth })?;
        Ok(CeremonyStart {
            ceremony_id,
            options,
        })
    }

    /// Finish a passkey sign-in. `replaces` is the caller's current session, if any, which
    /// ends here: every sign-in gets a new session id.
    pub fn finish_login(
        &self,
        host: &str,
        ceremony_id: &str,
        credential: &Value,
        meta: &ClientMeta,
        replaces: Option<&Principal>,
    ) -> Result<NewSession> {
        self.limit_address(meta)?;
        let rp = self.rp(host)?;
        let Pending::Login { auth } = self.take_ceremony(ceremony_id, &rp.id)? else {
            return Err(AuthError::InvalidRequest("not a login ceremony".into()));
        };
        let cred: PublicKeyCredential = parse(credential)?;
        let (user_id, cred_id) = rp
            .webauthn
            .identify_discoverable_authentication(&cred)
            .map_err(|_| self.failed(meta, None, "login_failed", "no user handle"))?;
        let row = store::passkey_by_credential(&self.db(), cred_id)?;
        let Some(row) = row.filter(|r| r.user_id == user_id.to_string() && r.rp_id == rp.id) else {
            return Err(self.failed(meta, None, "login_failed", "unknown passkey"));
        };
        let mut passkey: Passkey = serde_json::from_str(&row.passkey).map_err(internal)?;
        let result = rp
            .webauthn
            .finish_discoverable_authentication_v2(&cred, auth, &passkey)
            .map_err(|e| self.failed(meta, Some(&row.user_id), "login_failed", &e.to_string()))?;
        let changed = passkey.update_credential(&result).unwrap_or(false);
        let conn = self.db();
        let updated = if changed {
            Some(serde_json::to_string(&passkey).map_err(internal)?)
        } else {
            None
        };
        store::update_passkey_use(&conn, &row.id, updated.as_deref(), self.now())?;
        let user = load_user(&conn, &row.user_id)?;
        let pending =
            self.inner.password_mode == PasswordMode::SecondFactor && user.password_hash.is_some();
        let session = self.new_session(
            &conn,
            &user.id,
            SessionMethod::Passkey,
            pending,
            meta,
            replaces,
        )?;
        self.log_with(
            &conn,
            "login",
            Some(&user.id),
            meta,
            if pending {
                "passkey; second factor pending"
            } else {
                "passkey"
            },
        );
        drop(conn);
        self.inner.limiter.succeed(&limit::address_key(meta.ip));
        Ok(session)
    }

    /// Sign in with a username and password ([`PasswordMode::Fallback`] only).
    pub fn password_login(
        &self,
        username: &str,
        password: &str,
        meta: &ClientMeta,
        replaces: Option<&Principal>,
    ) -> Result<NewSession> {
        if self.inner.password_mode != PasswordMode::Fallback {
            return Err(AuthError::Unavailable(
                "password sign-in is disabled on this instance".into(),
            ));
        }
        self.limit_address(meta)?;
        // An invalid name still costs a hash, so timing doesn't tell them apart.
        let name = normalize_username(username).unwrap_or_default();
        let account = limit::account_key(&name);
        self.inner.limiter.check(&account, &limit::ACCOUNT)?;
        let user = if name.is_empty() {
            None
        } else {
            store::user_by_name(&self.db(), &name)?
        };
        let ok = self.verify_password(
            password,
            user.as_ref().and_then(|u| u.password_hash.as_deref()),
        )?;
        let Some(user) = user.filter(|_| ok) else {
            if self.inner.limiter.fail(&account, &limit::ACCOUNT) {
                tracing::warn!(account = %name, "password lockout after repeated failures");
            }
            return Err(self.failed(meta, None, "login_failed", &format!("password for {name}")));
        };
        self.inner.limiter.succeed(&account);
        self.inner.limiter.succeed(&limit::address_key(meta.ip));
        self.maybe_rehash(&user, password);
        let conn = self.db();
        let session = self.new_session(
            &conn,
            &user.id,
            SessionMethod::Password,
            false,
            meta,
            replaces,
        )?;
        self.log_with(&conn, "login", Some(&user.id), meta, "password");
        Ok(session)
    }

    /// The password step after a passkey ([`PasswordMode::SecondFactor`]).
    pub fn complete_second_factor(
        &self,
        p: &Principal,
        password: &str,
        meta: &ClientMeta,
    ) -> Result<NewSession> {
        if !p.pending_second_factor() {
            return Err(AuthError::Forbidden("no second factor is pending"));
        }
        self.limit_address(meta)?;
        let account = limit::account_key(&p.user.name);
        self.inner.limiter.check(&account, &limit::ACCOUNT)?;
        let user = load_user(&self.db(), &p.user.id)?;
        if !self.verify_password(password, user.password_hash.as_deref())? {
            self.inner.limiter.fail(&account, &limit::ACCOUNT);
            return Err(self.failed(meta, Some(&user.id), "second_factor_failed", "password"));
        }
        self.inner.limiter.succeed(&account);
        self.inner.limiter.succeed(&limit::address_key(meta.ip));
        self.maybe_rehash(&user, password);
        let conn = self.db();
        let session = self.new_session(
            &conn,
            &user.id,
            SessionMethod::PasskeyAndPassword,
            false,
            meta,
            Some(p),
        )?;
        self.log_with(&conn, "login", Some(&user.id), meta, "second factor");
        Ok(session)
    }

    /// End the caller's session, if it has one.
    pub fn logout(&self, p: &Principal, meta: &ClientMeta) -> Result<()> {
        if let Some(s) = p.session() {
            let conn = self.db();
            store::delete_session(&conn, &s.hash)?;
            self.bump_revocations();
            self.log_with(&conn, "logout", Some(&p.user.id), meta, "");
        }
        Ok(())
    }

    // ----- Re-authentication -----

    pub fn start_reauth(
        &self,
        host: &str,
        p: &Principal,
        meta: &ClientMeta,
    ) -> Result<CeremonyStart> {
        self.limit_address(meta)?;
        let s = account_session(p)?;
        let rp = self.rp(host)?;
        let rows = store::passkeys_for_user(&self.db(), &p.user.id, Some(&rp.id))?;
        if rows.is_empty() {
            return Err(AuthError::Unavailable(
                "you have no passkey for this host".into(),
            ));
        }
        let passkeys = rows
            .iter()
            .map(|r| serde_json::from_str::<Passkey>(&r.passkey).map_err(internal))
            .collect::<Result<Vec<_>>>()?;
        let (rcr, auth) = rp
            .webauthn
            .start_passkey_authentication(&passkeys)
            .map_err(internal)?;
        let options = authentication_options(&rcr)?;
        let ceremony_id = self.store_ceremony(
            &rp.id,
            Pending::Reauth {
                auth,
                session: s.hash,
            },
        )?;
        Ok(CeremonyStart {
            ceremony_id,
            options,
        })
    }

    pub fn finish_reauth(
        &self,
        host: &str,
        p: &Principal,
        ceremony_id: &str,
        credential: &Value,
        meta: &ClientMeta,
    ) -> Result<NewSession> {
        self.limit_address(meta)?;
        let s = account_session(p)?;
        let rp = self.rp(host)?;
        let Pending::Reauth { auth, session } = self.take_ceremony(ceremony_id, &rp.id)? else {
            return Err(AuthError::InvalidRequest(
                "not a re-authentication ceremony".into(),
            ));
        };
        if !secret::digests_equal(&session, &s.hash) {
            return Err(AuthError::InvalidRequest(
                "that ceremony belongs to another session".into(),
            ));
        }
        let cred: PublicKeyCredential = parse(credential)?;
        let result = rp
            .webauthn
            .finish_passkey_authentication(&cred, &auth)
            .map_err(|e| self.failed(meta, Some(&p.user.id), "reauth_failed", &e.to_string()))?;
        let conn = self.db();
        if let Some(row) = store::passkey_by_credential(&conn, result.cred_id().as_ref())?
            .filter(|r| r.user_id == p.user.id)
        {
            let mut passkey: Passkey = serde_json::from_str(&row.passkey).map_err(internal)?;
            let updated = if passkey.update_credential(&result).unwrap_or(false) {
                Some(serde_json::to_string(&passkey).map_err(internal)?)
            } else {
                None
            };
            store::update_passkey_use(&conn, &row.id, updated.as_deref(), self.now())?;
        }
        let session = self.new_session(&conn, &p.user.id, s.state.method, false, meta, Some(p))?;
        self.log_with(&conn, "reauth", Some(&p.user.id), meta, "passkey");
        drop(conn);
        self.inner.limiter.succeed(&limit::address_key(meta.ip));
        Ok(session)
    }

    /// Re-authenticate with the password ([`PasswordMode::Fallback`] only; with a second
    /// factor configured, re-authentication takes the passkey).
    pub fn reauth_with_password(
        &self,
        p: &Principal,
        password: &str,
        meta: &ClientMeta,
    ) -> Result<NewSession> {
        if self.inner.password_mode != PasswordMode::Fallback {
            return Err(AuthError::Unavailable(
                "re-authenticate with your passkey".into(),
            ));
        }
        self.limit_address(meta)?;
        let s = account_session(p)?;
        let account = limit::account_key(&p.user.name);
        self.inner.limiter.check(&account, &limit::ACCOUNT)?;
        let user = load_user(&self.db(), &p.user.id)?;
        if !self.verify_password(password, user.password_hash.as_deref())? {
            self.inner.limiter.fail(&account, &limit::ACCOUNT);
            return Err(self.failed(meta, Some(&user.id), "reauth_failed", "password"));
        }
        self.inner.limiter.succeed(&account);
        let conn = self.db();
        let session = self.new_session(&conn, &user.id, s.state.method, false, meta, Some(p))?;
        self.log_with(&conn, "reauth", Some(&user.id), meta, "password");
        Ok(session)
    }

    // ----- Passkeys -----

    pub fn list_passkeys(&self, p: &Principal) -> Result<Vec<PasskeyInfo>> {
        account_session(p)?;
        Ok(store::passkeys_for_user(&self.db(), &p.user.id, None)?
            .iter()
            .map(PasskeyRow::info)
            .collect())
    }

    pub fn start_add_passkey(&self, host: &str, p: &Principal) -> Result<CeremonyStart> {
        let s = self.recent_session(p)?;
        let rp = self.rp(host)?;
        let user_id = Uuid::parse_str(&p.user.id).map_err(internal)?;
        let exclude = store::passkeys_for_user(&self.db(), &p.user.id, Some(&rp.id))?
            .iter()
            .map(|r| {
                serde_json::from_str::<Passkey>(&r.passkey)
                    .map(|k| k.cred_id().clone())
                    .map_err(internal)
            })
            .collect::<Result<Vec<_>>>()?;
        let (ccr, reg) = rp
            .webauthn
            .start_passkey_registration(user_id, &p.user.name, &p.user.display_name, Some(exclude))
            .map_err(internal)?;
        let options = registration_options(&ccr)?;
        let ceremony_id = self.store_ceremony(
            &rp.id,
            Pending::AddPasskey {
                reg,
                session: s.hash,
            },
        )?;
        Ok(CeremonyStart {
            ceremony_id,
            options,
        })
    }

    pub fn finish_add_passkey(
        &self,
        host: &str,
        p: &Principal,
        ceremony_id: &str,
        credential: &Value,
        name: Option<&str>,
        meta: &ClientMeta,
    ) -> Result<PasskeyInfo> {
        let s = self.recent_session(p)?;
        let rp = self.rp(host)?;
        let Pending::AddPasskey { reg, session } = self.take_ceremony(ceremony_id, &rp.id)? else {
            return Err(AuthError::InvalidRequest(
                "not a registration ceremony".into(),
            ));
        };
        if !secret::digests_equal(&session, &s.hash) {
            return Err(AuthError::InvalidRequest(
                "that ceremony belongs to another session".into(),
            ));
        }
        let name = optional_label(name, "Passkey", "passkey names")?;
        let cred: RegisterPublicKeyCredential = parse(credential)?;
        let passkey = rp
            .webauthn
            .finish_passkey_registration(&cred, &reg)
            .map_err(|e| AuthError::InvalidRequest(format!("registration failed: {e}")))?;
        let now = self.now();
        let row = PasskeyRow {
            id: secret::public_id(),
            user_id: p.user.id.clone(),
            rp_id: rp.id.clone(),
            name,
            passkey: serde_json::to_string(&passkey).map_err(internal)?,
            created_at: now,
            last_used_at: None,
        };
        let conn = self.db();
        match store::insert_passkey(&conn, &row, passkey.cred_id().as_ref()) {
            Ok(()) => {}
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                return Err(AuthError::Conflict(
                    "that passkey is already registered".into(),
                ));
            }
            Err(e) => return Err(e.into()),
        }
        self.log_with(&conn, "passkey_added", Some(&p.user.id), meta, &row.name);
        Ok(row.info())
    }

    pub fn rename_passkey(&self, p: &Principal, id: &str, name: &str) -> Result<()> {
        account_session(p)?;
        let name = clean_label(name, "passkey names")?;
        if store::rename_passkey(&self.db(), &p.user.id, id, &name)? {
            Ok(())
        } else {
            Err(AuthError::NotFound)
        }
    }

    pub fn remove_passkey(&self, p: &Principal, id: &str, meta: &ClientMeta) -> Result<()> {
        self.recent_session(p)?;
        let conn = self.db();
        let all = store::passkeys_for_user(&conn, &p.user.id, None)?;
        if !all.iter().any(|r| r.id == id) {
            return Err(AuthError::NotFound);
        }
        let user = load_user(&conn, &p.user.id)?;
        let password_signs_in =
            user.password_hash.is_some() && self.inner.password_mode == PasswordMode::Fallback;
        if all.len() == 1 && !password_signs_in {
            return Err(AuthError::Conflict(
                "that is your only way to sign in; add another passkey first".into(),
            ));
        }
        store::delete_passkey(&conn, &p.user.id, id)?;
        self.log_with(&conn, "passkey_removed", Some(&p.user.id), meta, id);
        Ok(())
    }

    // ----- Passwords -----

    pub fn set_password(&self, p: &Principal, password: &str, meta: &ClientMeta) -> Result<()> {
        if self.inner.password_mode == PasswordMode::Off {
            return Err(AuthError::Unavailable(
                "passwords are disabled on this instance".into(),
            ));
        }
        self.recent_session(p)?;
        let hash = self.hash_password(password)?;
        let conn = self.db();
        store::set_password_hash(&conn, &p.user.id, Some(&hash))?;
        self.log_with(&conn, "password_set", Some(&p.user.id), meta, "");
        Ok(())
    }

    pub fn remove_password(&self, p: &Principal, meta: &ClientMeta) -> Result<()> {
        self.recent_session(p)?;
        let conn = self.db();
        if store::passkeys_for_user(&conn, &p.user.id, None)?.is_empty() {
            return Err(AuthError::Conflict(
                "your password is your only way to sign in; add a passkey first".into(),
            ));
        }
        store::set_password_hash(&conn, &p.user.id, None)?;
        self.log_with(&conn, "password_removed", Some(&p.user.id), meta, "");
        Ok(())
    }

    // ----- API tokens -----

    pub fn list_tokens(&self, p: &Principal) -> Result<Vec<ApiTokenInfo>> {
        account_session(p)?;
        Ok(store::tokens_for_user(&self.db(), &p.user.id)?
            .into_iter()
            .map(|t| t.info)
            .collect())
    }

    pub fn create_token(
        &self,
        p: &Principal,
        req: &CreateTokenRequest,
        meta: &ClientMeta,
    ) -> Result<CreatedToken> {
        self.recent_session(p)?;
        let name = clean_label(&req.name, "token names")?;
        if req
            .expires_in_days
            .is_some_and(|d| !(1..=3650).contains(&d))
        {
            return Err(AuthError::InvalidRequest(
                "tokens expire after 1 to 3650 days, or never".into(),
            ));
        }
        let now = self.now();
        let token = format!("{TOKEN_PREFIX}{}", secret::token256());
        let row = TokenRow {
            user_id: p.user.id.clone(),
            info: ApiTokenInfo {
                id: secret::public_id(),
                name,
                scope: req.scope,
                prefix: token.chars().take(TOKEN_PREFIX.len() + 6).collect(),
                created_at: now,
                expires_at: req
                    .expires_in_days
                    .map(|d| now + ms(Duration::from_secs(u64::from(d) * 86_400))),
                last_used_at: None,
            },
        };
        let conn = self.db();
        store::insert_token(&conn, &row, &secret::sha256(token.as_bytes()))?;
        self.log_with(
            &conn,
            "token_created",
            Some(&p.user.id),
            meta,
            &format!("{} ({})", row.info.name, store::scope_str(row.info.scope)),
        );
        Ok(CreatedToken {
            token,
            info: row.info,
        })
    }

    pub fn revoke_token(&self, p: &Principal, id: &str, meta: &ClientMeta) -> Result<()> {
        account_session(p)?;
        let conn = self.db();
        if !store::delete_token(&conn, &p.user.id, id)? {
            return Err(AuthError::NotFound);
        }
        self.bump_revocations();
        self.log_with(&conn, "token_revoked", Some(&p.user.id), meta, id);
        Ok(())
    }

    // ----- Sessions -----

    pub fn list_sessions(&self, p: &Principal) -> Result<Vec<SessionInfo>> {
        let s = account_session(p)?;
        let now = self.now();
        let idle = ms(self.inner.session_idle);
        Ok(store::sessions_for_user(&self.db(), &p.user.id)?
            .iter()
            .filter(|r| r.expires_at > now && now - r.last_seen_at <= idle)
            .map(|r| r.info(r.public_id == s.public_id))
            .collect())
    }

    pub fn revoke_session(&self, p: &Principal, id: &str, meta: &ClientMeta) -> Result<()> {
        account_session(p)?;
        let conn = self.db();
        if !store::delete_session_by_public_id(&conn, &p.user.id, id)? {
            return Err(AuthError::NotFound);
        }
        self.bump_revocations();
        self.log_with(&conn, "session_revoked", Some(&p.user.id), meta, id);
        Ok(())
    }
}

/// Write a file only its owner can read (on Unix; on Windows it inherits the directory's
/// ACL).
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(path);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()
}

#[cfg(test)]
mod tests;

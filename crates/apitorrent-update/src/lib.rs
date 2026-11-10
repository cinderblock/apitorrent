//! Signed self-updates for `apitorrentd`.
//!
//! - [`key`]: the minisign public key compiled in from `release/update-signing.pub`. A
//!   build without that file reports new versions but never installs one.
//! - [`github`]: the latest *published* release of the repository.
//! - [`manifest`]: the signed list of binaries in a release, with their SHA-256.
//! - [`install`]: download, verify and swap the binary.
//! - [`restart`]: start the new binary once the daemon has shut down.
//!
//! An install needs, in order: a valid signature over the manifest by the compiled-in key,
//! a manifest version newer than the running one (no downgrades), an entry for this
//! build's target, and a download whose size and SHA-256 match. There is no way to skip
//! any of them.

pub mod github;
pub mod install;
pub mod key;
pub mod manifest;
pub mod restart;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use apitorrent_types::update::{UpdatePhase, UpdateRelease, UpdateSettings, UpdateStatus};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

pub use github::{Release, Source};
pub use key::{KeyError, UpdateKey};
pub use manifest::{Manifest, ManifestAsset, ManifestError};

/// The target triple this binary was built for, as the manifest names it.
pub const TARGET: &str = env!("APITORRENT_UPDATE_TARGET");

/// How often the daily check runs.
const CHECK_EVERY: Duration = Duration::from_secs(24 * 3600);
/// The first check after start waits this long, so a crash loop doesn't hammer GitHub.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("installing updates is turned off in this build: {0}")]
    Disabled(String),
    #[error("couldn't reach the release server: {0}")]
    Network(String),
    #[error("there is no published release yet")]
    NoRelease,
    #[error("the release has no {0}")]
    MissingAsset(&'static str),
    #[error("the update's signature is not valid: {0}")]
    Signature(#[from] KeyError),
    #[error("{0}")]
    Manifest(#[from] ManifestError),
    #[error("{offered} is not newer than the running {current}; refusing to downgrade")]
    NotNewer { current: String, offered: String },
    #[error("the download doesn't match the signed manifest: {0}")]
    Mismatch(String),
    #[error("{0}")]
    Io(String),
    #[error("an update is already being checked or installed")]
    Busy,
}

impl UpdateError {
    /// A short machine-readable code for API errors.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Disabled(_) => "update_disabled",
            Self::Network(_) => "update_network",
            Self::NoRelease => "no_release",
            Self::MissingAsset(_) => "update_missing_asset",
            Self::Signature(_) => "update_bad_signature",
            Self::Manifest(_) => "update_bad_manifest",
            Self::NotNewer { .. } => "update_not_newer",
            Self::Mismatch(_) => "update_mismatch",
            Self::Io(_) => "update_io",
            Self::Busy => "update_busy",
        }
    }
}

/// What the daemon passes in.
#[derive(Debug, Clone)]
pub struct UpdaterConfig {
    /// Settings and the last check are kept in `<data_dir>/update.json`.
    pub data_dir: PathBuf,
    pub current_version: semver::Version,
    /// The executable to replace.
    pub exe: PathBuf,
    /// `None` switches installs off ([`UpdateKey::embedded`] in production).
    pub key: Option<UpdateKey>,
    pub source: Source,
    /// The manifest entry to install ([`TARGET`] in production).
    pub target: String,
}

impl UpdaterConfig {
    /// The running daemon's configuration: this binary, the compiled-in key, GitHub.
    pub fn for_this_binary(data_dir: PathBuf) -> Result<Self, UpdateError> {
        let exe = std::env::current_exe()
            .and_then(|p| p.canonicalize())
            .map_err(|e| UpdateError::Io(format!("couldn't find the running executable: {e}")))?;
        Ok(Self {
            data_dir,
            current_version: semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("semver"),
            exe,
            key: UpdateKey::embedded(),
            source: Source::github(),
            target: TARGET.to_owned(),
        })
    }
}

/// `<data_dir>/update.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Saved {
    #[serde(default)]
    settings: UpdateSettings,
    #[serde(default)]
    checked_ms: Option<i64>,
}

#[derive(Debug, Default)]
struct State {
    saved: Saved,
    latest: Option<Release>,
    last_error: Option<String>,
    phase: UpdatePhase,
}

/// A finished install: the new binary is in place, waiting for a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub from: semver::Version,
    pub to: semver::Version,
}

/// Checks for and installs updates. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Updater {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    config: UpdaterConfig,
    client: reqwest::Client,
    state: Mutex<State>,
    /// One check or install at a time.
    busy: tokio::sync::Mutex<()>,
    /// Set once an install has put a new binary in place.
    installed: watch::Sender<Option<Installed>>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

impl Updater {
    pub fn new(config: UpdaterConfig) -> Result<Self, UpdateError> {
        let client = config.source.client()?;
        let saved = load(&config.data_dir.join("update.json"));
        install::remove_leftover(&config.exe);
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                client,
                state: Mutex::new(State {
                    saved,
                    ..State::default()
                }),
                busy: tokio::sync::Mutex::new(()),
                installed: watch::channel(None).0,
            }),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().expect("not poisoned")
    }

    /// The executable an install replaces.
    pub fn exe(&self) -> &Path {
        &self.inner.config.exe
    }

    /// Why installs are off, if they are.
    pub fn installs_disabled_reason(&self) -> Option<String> {
        self.inner.config.key.is_none().then(|| {
            "this build has no update signing key (release/update-signing.pub), so it can only \
             report new versions"
                .to_owned()
        })
    }

    pub fn status(&self) -> UpdateStatus {
        let s = self.lock();
        let current = &self.inner.config.current_version;
        let reason = self.installs_disabled_reason();
        UpdateStatus {
            current_version: current.to_string(),
            available: s.latest.as_ref().is_some_and(|r| r.version > *current),
            latest: s.latest.as_ref().map(|r| UpdateRelease {
                version: r.version.to_string(),
                url: r.url.clone(),
                notes: r.notes.clone(),
                published_ms: r.published_ms,
            }),
            installs_enabled: reason.is_none(),
            installs_disabled_reason: reason,
            checked_ms: s.saved.checked_ms,
            last_error: s.last_error.clone(),
            phase: s.phase,
            settings: s.saved.settings,
        }
    }

    pub fn settings(&self) -> UpdateSettings {
        self.lock().saved.settings
    }

    pub fn set_settings(&self, settings: UpdateSettings) -> Result<UpdateStatus, UpdateError> {
        let saved = {
            let mut s = self.lock();
            s.saved.settings = settings;
            s.saved.clone()
        };
        self.save(&saved)?;
        Ok(self.status())
    }

    fn save(&self, saved: &Saved) -> Result<(), UpdateError> {
        let path = self.inner.config.data_dir.join("update.json");
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(saved).expect("serializable");
        std::fs::write(&tmp, json)
            .and_then(|()| std::fs::rename(&tmp, &path))
            .map_err(|e| UpdateError::Io(format!("couldn't save {}: {e}", path.display())))
    }

    fn set_phase(&self, phase: UpdatePhase) {
        self.lock().phase = phase;
    }

    fn record<T>(&self, r: &Result<T, UpdateError>) {
        let mut s = self.lock();
        s.last_error = r.as_ref().err().map(ToString::to_string);
    }

    /// Ask GitHub for the latest published release.
    pub async fn check(&self) -> Result<UpdateStatus, UpdateError> {
        let _busy = self.inner.busy.try_lock().map_err(|_| UpdateError::Busy)?;
        self.set_phase(UpdatePhase::Checking);
        let r = self.check_locked().await;
        self.record(&r);
        self.set_phase(UpdatePhase::Idle);
        r.map(|_| self.status())
    }

    async fn check_locked(&self) -> Result<Option<Release>, UpdateError> {
        let latest = github::latest(&self.inner.config.source, &self.inner.client).await?;
        let saved = {
            let mut s = self.lock();
            s.latest.clone_from(&latest);
            s.saved.checked_ms = Some(now_ms());
            s.saved.clone()
        };
        if let Err(e) = self.save(&saved) {
            tracing::warn!(error = %e, "couldn't save the update check time");
        }
        if let Some(r) = &latest
            && r.version > self.inner.config.current_version
        {
            tracing::info!(
                current = %self.inner.config.current_version,
                latest = %r.version,
                url = %r.url,
                "a new apitorrent release is available"
            );
        }
        Ok(latest)
    }

    /// Check, then download, verify and swap in the latest release's binary. On success
    /// the new binary is in place; the caller restarts (see [`Self::installed`]).
    pub async fn install(&self) -> Result<Installed, UpdateError> {
        if let Some(reason) = self.installs_disabled_reason() {
            return Err(UpdateError::Disabled(reason));
        }
        let _busy = self.inner.busy.try_lock().map_err(|_| UpdateError::Busy)?;
        self.set_phase(UpdatePhase::Installing);
        let r = self.install_locked().await;
        self.record(&r);
        match &r {
            Ok(done) => {
                self.set_phase(UpdatePhase::Restarting);
                tracing::info!(from = %done.from, to = %done.to, "update installed");
                self.inner.installed.send_replace(Some(done.clone()));
            }
            Err(e) => {
                self.set_phase(UpdatePhase::Idle);
                tracing::warn!(error = %e, "update failed");
            }
        }
        r
    }

    async fn install_locked(&self) -> Result<Installed, UpdateError> {
        let c = &self.inner.config;
        let key = c.key.as_ref().expect("checked by install");
        let release = self.check_locked().await?.ok_or(UpdateError::NoRelease)?;
        let manifest_url = release
            .asset(manifest::MANIFEST_NAME)
            .ok_or(UpdateError::MissingAsset(manifest::MANIFEST_NAME))?;
        let sig_url = release
            .asset(manifest::SIGNATURE_NAME)
            .ok_or(UpdateError::MissingAsset(manifest::SIGNATURE_NAME))?;
        let source = &c.source;
        let client = &self.inner.client;
        let manifest_bytes = github::fetch_small(source, client, manifest_url).await?;
        let sig = github::fetch_small(source, client, sig_url).await?;
        let manifest = verify_manifest(
            key,
            &manifest_bytes,
            &String::from_utf8_lossy(&sig),
            &c.current_version,
        )?;
        let asset = manifest.asset(&c.target)?;
        let to = manifest.semver()?;
        let dir = c
            .exe
            .parent()
            .ok_or_else(|| UpdateError::Io("the executable has no directory".to_owned()))?;
        install::check_writable(dir)?;
        tracing::info!(version = %to, url = %asset.url, "downloading the update");
        let staged = install::download(source, client, asset, dir).await?;
        install::swap(staged, &c.exe)?;
        Ok(Installed {
            from: c.current_version.clone(),
            to,
        })
    }

    /// Resolves to `Some` once an install has put a new binary in place.
    pub fn installed(&self) -> watch::Receiver<Option<Installed>> {
        self.inner.installed.subscribe()
    }

    /// The daily check, and the automatic install when the settings allow it. Runs until
    /// the returned task is aborted.
    pub fn spawn_daily(&self) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(FIRST_CHECK_DELAY).await;
            // A failed check counts too: try again tomorrow, not in a tight loop.
            let mut attempted: Option<i64> = None;
            loop {
                let last = this.lock().saved.checked_ms.max(attempted);
                tokio::time::sleep(next_check_in(last, now_ms())).await;
                if !this.settings().check_daily {
                    // Look again in an hour, in case the setting changes.
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    continue;
                }
                attempted = Some(now_ms());
                match this.check().await {
                    Ok(status) => {
                        if status.available
                            && status.settings.auto_install
                            && status.installs_enabled
                        {
                            tracing::info!(
                                "installing the update automatically (autoInstall is on)"
                            );
                            if this.install().await.is_ok() {
                                return;
                            }
                        }
                    }
                    Err(UpdateError::Busy) => {}
                    Err(e) => tracing::warn!(error = %e, "the update check failed"),
                }
            }
        })
    }
}

fn next_check_in(last: Option<i64>, now: i64) -> Duration {
    let Some(last) = last else {
        return Duration::ZERO;
    };
    let due = last.saturating_add(i64::try_from(CHECK_EVERY.as_millis()).unwrap_or(i64::MAX));
    Duration::from_millis(u64::try_from(due.saturating_sub(now)).unwrap_or(0)).min(CHECK_EVERY)
}

/// Verify `manifest` against `signature` with `key`, parse it, and refuse anything not
/// newer than `current`. The signature is checked before the JSON is even parsed.
pub fn verify_manifest(
    key: &UpdateKey,
    manifest: &[u8],
    signature: &str,
    current: &semver::Version,
) -> Result<Manifest, UpdateError> {
    key.verify(manifest, signature)?;
    let m = Manifest::parse(manifest)?;
    let offered = m.semver()?;
    if offered <= *current {
        return Err(UpdateError::NotNewer {
            current: current.to_string(),
            offered: offered.to_string(),
        });
    }
    Ok(m)
}

fn load(path: &Path) -> Saved {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            tracing::warn!(path = %path.display(), error = %e, "ignoring an unreadable update.json");
            Saved::default()
        }),
        Err(_) => Saved::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_schedule() {
        assert_eq!(next_check_in(None, 1_000), Duration::ZERO);
        let day = 24 * 3600 * 1000;
        assert_eq!(next_check_in(Some(0), day + 5), Duration::ZERO);
        assert_eq!(next_check_in(Some(0), day - 1000), Duration::from_secs(1));
        // A clock that went backwards waits at most a day.
        assert_eq!(next_check_in(Some(10 * day), 0), CHECK_EVERY);
    }

    #[test]
    fn downgrades_and_same_version_are_refused() {
        let k = key::tests::TestKey::new();
        let key = UpdateKey::parse(&k.tauri_public()).unwrap();
        let manifest = |v: &str| {
            serde_json::to_vec(&Manifest {
                product: manifest::PRODUCT.into(),
                version: v.into(),
                pub_date: None,
                notes_url: None,
                assets: vec![],
            })
            .unwrap()
        };
        let current = semver::Version::parse("1.2.3").unwrap();
        for v in ["1.2.3", "1.2.2", "0.9.0", "1.2.3-rc.1"] {
            let m = manifest(v);
            assert!(
                matches!(
                    verify_manifest(&key, &m, &k.sign(&m), &current),
                    Err(UpdateError::NotNewer { .. })
                ),
                "{v}"
            );
        }
        let m = manifest("1.2.4");
        verify_manifest(&key, &m, &k.sign(&m), &current).unwrap();
        // A signature over other bytes doesn't carry over.
        assert!(matches!(
            verify_manifest(&key, &manifest("9.9.9"), &k.sign(&m), &current),
            Err(UpdateError::Signature(KeyError::Invalid))
        ));
    }
}

//! Automation: release candidates, rules, stable aliases and distribution channels.
//!
//! - **Candidates** come from `source` plugins (`api.candidates.submit`, through
//!   [`Core::submit_candidates`]) and from subscribed channels. They are kept in
//!   `automation.db`, so a rule created later still sees releases a source offered before.
//! - **Rules** ([`rules`]) match candidates, order them by version and keep the latest N as
//!   torrents, with a label, a save path and a file selection. Torrents pushed out by newer
//!   releases get the rule's superseded action, which reuses seed limits. A dry run shows the
//!   plan without changing anything.
//! - **Aliases** ([`alias`]) keep a stable path naming the newest completed release.
//! - **Channels** ([`channels`]) publish a signed, revisioned list of torrents; other
//!   instances subscribe with the thin client's strict TLS and pinning and run their own
//!   rules over it.
//!
//! All of it is driven by one background pass (every minute, and at once when candidates,
//! rules or subscriptions change) plus a subscription poller.

pub mod alias;
pub mod channel;
mod channels;
mod db;
pub mod matcher;
mod plan;
mod rules;
pub mod version;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use tokio::sync::Notify;

use apitorrent_plugin_api::SourceFetcher;
use apitorrent_types::automation::{ReleaseCandidate, StoredCandidate, SubmitCandidatesOutcome};
use apitorrent_types::spaces::SpaceId;

use crate::{Core, CoreError, Key};

pub use channel::{ChannelKey, VerifyError};
use db::{AutoDb, DbError};

/// How often the rules run without being woken.
const PASS_EVERY: Duration = Duration::from_secs(60);
/// How often aliases and pending file selections are looked at.
const TICK_EVERY: Duration = Duration::from_secs(10);
/// How often subscriptions are checked for being due.
const POLL_CHECK_EVERY: Duration = Duration::from_secs(30);
/// Candidates unseen for this long are forgotten (unless a rule still keeps them).
const CANDIDATE_TTL_MS: i64 = 180 * 24 * 3600 * 1000;
/// The most candidates kept per source.
const MAX_CANDIDATES_PER_SOURCE: usize = 5000;
/// The most candidates one submission may carry.
const MAX_SUBMISSION: usize = 1000;
/// The largest `.torrent` a rule fetches or a channel entry carries.
pub const MAX_METAINFO: usize = 16 * 1024 * 1024;

/// The automation state a [`Core`] holds.
pub(crate) struct Automation {
    db: Arc<Mutex<AutoDb>>,
    key: ChannelKey,
    fetcher: RwLock<Option<Arc<dyn SourceFetcher>>>,
    /// Run a pass now.
    wake: Notify,
    /// Check subscriptions now.
    poll_wake: Notify,
    /// One pass at a time.
    pass: tokio::sync::Mutex<()>,
    /// One channel revision change at a time.
    feeds: tokio::sync::Mutex<()>,
    /// One poll per subscription at a time.
    polling: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl std::fmt::Debug for Automation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Automation")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl From<DbError> for CoreError {
    fn from(e: DbError) -> Self {
        match e {
            DbError::Sqlite(e) => CoreError::Database(e),
            DbError::Json(e) => CoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("automation.db: {e}"),
            )),
        }
    }
}

impl Automation {
    /// Open `automation.db` and the channel key in `data_dir`.
    pub(crate) fn open(data_dir: &Path) -> Result<Self, CoreError> {
        let db = AutoDb::open(&data_dir.join("automation.db"))?;
        let key = ChannelKey::load_or_create(&data_dir.join("automation").join("channel-key.p8"))?;
        Ok(Self {
            db: Arc::new(Mutex::new(db)),
            key,
            fetcher: RwLock::new(None),
            wake: Notify::new(),
            poll_wake: Notify::new(),
            pass: tokio::sync::Mutex::new(()),
            feeds: tokio::sync::Mutex::new(()),
            polling: Mutex::new(HashMap::new()),
        })
    }

    /// Whether a space still has rules, channels or subscriptions.
    pub(crate) fn space_in_use(&self, space: SpaceId) -> Result<bool, CoreError> {
        Ok(self
            .db
            .lock()
            .expect("automation db poisoned")
            .space_in_use(space)?)
    }
}

/// Milliseconds since the Unix epoch.
fn now_ms() -> i64 {
    crate::now_ms()
}

/// A random id: 16 hex digits.
fn new_id() -> String {
    format!("{:016x}", rand::random::<u64>())
}

/// Lowercase letters, digits and `-`, starting with a letter; 1 to 64 characters.
fn valid_slug(id: &str) -> bool {
    id.len() <= 64
        && id.starts_with(|c: char| c.is_ascii_lowercase())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A candidate's problems, if any.
fn check_candidate(c: &ReleaseCandidate) -> Result<(), String> {
    if c.id.trim().is_empty() || c.id.len() > 512 {
        return Err("`id` must be 1 to 512 characters".into());
    }
    if c.title.trim().is_empty() || c.title.len() > 1000 {
        return Err("`title` must be 1 to 1000 characters".into());
    }
    if c.version.as_ref().is_some_and(|v| v.len() > 200) {
        return Err("`version` is longer than 200 characters".into());
    }
    if c.published.as_ref().is_some_and(|v| v.len() > 100) {
        return Err("`published` is longer than 100 characters".into());
    }
    if let Some(m) = &c.links.magnet {
        apitorrent_metainfo::Magnet::parse(m).map_err(|e| format!("`links.magnet`: {e}"))?;
    }
    for (name, link) in [("torrent", &c.links.torrent), ("webseed", &c.links.webseed)] {
        if let Some(l) = link {
            let u = url::Url::parse(l).map_err(|e| format!("`links.{name}`: {e}"))?;
            if !matches!(u.scheme(), "https" | "http") {
                return Err(format!("`links.{name}` must be an http(s) URL"));
            }
        }
    }
    let meta = serde_json::to_vec(&c.metadata).map_err(|e| e.to_string())?;
    if meta.len() > 16 * 1024 {
        return Err("`metadata` is larger than 16 KiB".into());
    }
    Ok(())
}

impl Core {
    fn auto(&self) -> &Automation {
        &self.inner.automation
    }

    /// Run `f` on the automation database off the async threads.
    async fn auto_db<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut AutoDb) -> Result<T, DbError> + Send + 'static,
    ) -> Result<T, CoreError> {
        let db = self.auto().db.clone();
        tokio::task::spawn_blocking(move || f(&mut db.lock().expect("automation db poisoned")))
            .await
            .expect("automation database task panicked")
            .map_err(CoreError::from)
    }

    /// Let rules fetch `source` plugins' `.torrent` links through the plugin host, with
    /// each plugin's own grants. Without one, such candidates wait (magnets and channel
    /// entries need no fetching).
    pub fn set_source_fetcher(&self, fetcher: Arc<dyn SourceFetcher>) {
        *self.auto().fetcher.write().expect("fetcher lock poisoned") = Some(fetcher);
        self.auto().wake.notify_one();
    }

    fn source_fetcher(&self) -> Option<Arc<dyn SourceFetcher>> {
        self.auto()
            .fetcher
            .read()
            .expect("fetcher lock poisoned")
            .clone()
    }

    /// The instance's channel signing key (Ed25519, base64): what subscribers pin.
    pub fn channel_public_key(&self) -> String {
        self.auto().key.public_base64()
    }

    /// Store candidates from a source (`plugin:<id>`, or `channel:<id>` internally), for one
    /// space's rules or (`None`, a source that isn't space-aware) every space's. Each is
    /// checked on its own; bad ones are listed, the rest stored. Rules run soon after.
    pub async fn submit_candidates(
        &self,
        space: Option<SpaceId>,
        source: &str,
        candidates: Vec<ReleaseCandidate>,
    ) -> Result<SubmitCandidatesOutcome, CoreError> {
        self.store_candidates(
            space.unwrap_or(db::ALL_SPACES),
            source,
            candidates.into_iter().map(|c| (c, None)).collect(),
        )
        .await
    }

    /// [`Self::submit_candidates`], with `.torrent` files.
    async fn store_candidates(
        &self,
        space: SpaceId,
        source: &str,
        candidates: Vec<(ReleaseCandidate, Option<Vec<u8>>)>,
    ) -> Result<SubmitCandidatesOutcome, CoreError> {
        if candidates.len() > MAX_SUBMISSION {
            return Err(CoreError::InvalidRequest(format!(
                "at most {MAX_SUBMISSION} candidates at a time"
            )));
        }
        let mut outcome = SubmitCandidatesOutcome::default();
        let mut good = Vec::with_capacity(candidates.len());
        let mut seen = HashSet::new();
        for (c, meta) in candidates {
            match check_candidate(&c) {
                Ok(()) if !seen.insert(c.id.clone()) => {
                    outcome.rejected.push(format!("{}: listed twice", c.id));
                }
                Ok(()) => good.push((c, meta)),
                Err(e) => outcome.rejected.push(format!("{}: {e}", c.id)),
            }
        }
        outcome.accepted = u32::try_from(good.len()).unwrap_or(u32::MAX);
        if !good.is_empty() {
            let source = source.to_owned();
            let now = now_ms();
            self.auto_db(move |db| {
                db.upsert_candidates(space, &source, &good, now)?;
                db.cap_candidates(space, &source, MAX_CANDIDATES_PER_SOURCE)?;
                Ok(())
            })
            .await?;
            self.auto().wake.notify_one();
        }
        Ok(outcome)
    }

    /// The candidates a space's rules see, most recently seen first; from `source` only, if
    /// given.
    pub async fn candidates(
        &self,
        space: SpaceId,
        source: Option<String>,
    ) -> Result<Vec<StoredCandidate>, CoreError> {
        self.auto_db(move |db| db.candidates(Some(space), source.as_deref()))
            .await
    }

    /// Forget a source's candidates in a space. Torrents rules took from them stay.
    pub async fn forget_candidates(
        &self,
        space: SpaceId,
        source: String,
    ) -> Result<usize, CoreError> {
        self.auto_db(move |db| db.delete_candidates(Some(space), &source))
            .await
    }

    /// Run the rules, the aliases and the subscriptions in the background until shutdown.
    pub(crate) async fn automation_loop(self) {
        tokio::spawn(self.clone().subscription_loop());
        let mut tick = tokio::time::interval(TICK_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_pass: Option<tokio::time::Instant> = None;
        loop {
            if self
                .inner
                .stopping
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                return;
            }
            let woken = tokio::select! {
                () = self.auto().wake.notified() => true,
                _ = tick.tick() => false,
                () = self.inner.stop.notified() => return,
            };
            let due = last_pass.is_none_or(|t| t.elapsed() >= PASS_EVERY);
            if woken || due {
                self.rules_pass().await;
                last_pass = Some(tokio::time::Instant::now());
            } else {
                self.light_pass().await;
            }
        }
    }

    async fn subscription_loop(self) {
        let mut tick = tokio::time::interval(POLL_CHECK_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            if self
                .inner
                .stopping
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                return;
            }
            tokio::select! {
                () = self.auto().poll_wake.notified() => {}
                _ = tick.tick() => {}
                () = self.inner.stop.notified() => return,
            }
            self.poll_due_subscriptions().await;
        }
    }

    /// A rule's save path: absolute as given, or under its space's download folder (where
    /// aliases are relative to, too).
    fn rule_save_path(&self, space: SpaceId, p: Option<&str>) -> Result<Option<PathBuf>, String> {
        let Some(p) = p.map(str::trim).filter(|p| !p.is_empty()) else {
            return Ok(None);
        };
        let path = Path::new(p);
        if path.is_absolute() {
            self.check_save_path(space, path)
                .map_err(|e| e.to_string())?;
            return Ok(Some(path.to_path_buf()));
        }
        let plain = path
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
            && !p.contains(':');
        if !plain {
            return Err(format!(
                "the save path `{p}` must be absolute, or relative to the download directory \
                 with no `..`"
            ));
        }
        Ok(Some(self.download_dir(space).join(path)))
    }

    /// Where file `index` of a torrent is on disk.
    fn file_disk_path(&self, key: &Key, index: usize) -> Option<PathBuf> {
        let torrents = self.inner.torrents.read().expect("torrents lock poisoned");
        let e = torrents.get(key)?;
        match &e.file_paths {
            Some(paths) => paths
                .get(index)
                .map(|c| c.iter().fold(e.save_path.clone(), |p, x| p.join(x))),
            None => Self::disk_paths(&e.torrent, &e.save_path, e.naming)
                .into_iter()
                .nth(index),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apitorrent_types::automation::CandidateLinks;

    #[test]
    fn candidates_are_checked() {
        let ok = ReleaseCandidate {
            id: "a".into(),
            title: "A".into(),
            version: None,
            published: None,
            links: CandidateLinks {
                torrent: Some("https://example.org/a.torrent".into()),
                magnet: Some("magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567".into()),
                webseed: None,
            },
            metadata: serde_json::Map::new(),
        };
        assert!(check_candidate(&ok).is_ok());
        let mut bad = ok.clone();
        bad.links.torrent = Some("file:///etc/passwd".into());
        assert!(check_candidate(&bad).is_err());
        let mut bad = ok.clone();
        bad.links.magnet = Some("magnet:?dn=x".into());
        assert!(check_candidate(&bad).is_err());
        let mut bad = ok;
        bad.id = String::new();
        assert!(check_candidate(&bad).is_err());
        assert!(valid_slug("ubuntu-lts"));
        assert!(!valid_slug("Ubuntu") && !valid_slug("-x") && !valid_slug(""));
    }
}

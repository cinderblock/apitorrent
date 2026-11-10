//! Distribution channels: publishing a signed, revisioned list of torrents, and
//! subscribing to another instance's.
//!
//! **Publishing.** A channel selects torrents by rule (the ones it keeps) or by label. Its
//! revision moves only when the entries change, and never goes backwards; each feed is
//! signed with the instance's Ed25519 key ([`super::channel`]).
//!
//! **Subscribing.** A subscription fetches a feed over the authenticated API with
//! `apitorrent-client`: the thin client's strict TLS (OS trust, a private CA, or a pinned
//! key from the pairing code) and an API token. The feed must verify against the publisher
//! key pinned at subscription (or, if none was given, the key of the first feed, which then
//! stays pinned), name the subscribed channel, and not go back in revision. Its entries
//! become candidates from `channel:<subscription id>`, for this instance's own rules.

use base64::Engine as _;

use apitorrent_client::{Remote, SpkiHash, Token, Trust};
use apitorrent_metainfo::{Magnet, Torrent};
use apitorrent_types::ApiError;
use apitorrent_types::automation::{
    CHANNEL_FORMAT, CandidateLinks, ChannelDocument, ChannelEntry, ChannelInfo, ChannelSelector,
    ChannelSpec, CreateChannelRequest, ReleaseCandidate, RuleItemState, SignedChannel,
    SubscriptionInfo, SubscriptionSpec, SubscriptionTrust,
};

use apitorrent_types::spaces::SpaceId;

use super::db::{ChannelRow, SubscriptionRow};
use super::{MAX_METAINFO, channel, new_id, now_ms, valid_slug};
use crate::{Core, CoreError, Key};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// The most channels and subscriptions an instance keeps.
const MAX_CHANNELS: usize = 100;
const MAX_SUBSCRIPTIONS: usize = 100;
/// The most entries one channel publishes.
const MAX_ENTRIES: usize = 1000;

fn invalid(e: impl Into<String>) -> CoreError {
    CoreError::InvalidRequest(e.into())
}

/// The candidate source a subscription's entries come from.
pub(crate) fn subscription_source(id: &str) -> String {
    format!("channel:{id}")
}

impl Core {
    // --- Publishing ---------------------------------------------------------------------

    fn check_channel(spec: &ChannelSpec) -> Result<(), CoreError> {
        if spec.name.trim().is_empty() || spec.name.len() > 200 {
            return Err(invalid("a channel's name must be 1 to 200 characters"));
        }
        if spec.description.len() > 2000 {
            return Err(invalid("the description is longer than 2000 characters"));
        }
        match &spec.selector {
            ChannelSelector::Label { label } if label.trim().is_empty() => {
                Err(invalid("name the label to publish"))
            }
            _ => Ok(()),
        }
    }

    async fn check_selector(&self, space: SpaceId, spec: &ChannelSpec) -> Result<(), CoreError> {
        if let ChannelSelector::Rule { rule_id } = &spec.selector {
            let id = rule_id.clone();
            let found = self.auto_db(move |db| db.rule(&id)).await?;
            if found.is_none_or(|r| r.space != space) {
                return Err(invalid(format!("there is no rule {rule_id}")));
            }
        }
        Ok(())
    }

    fn channel_info(&self, row: ChannelRow) -> ChannelInfo {
        ChannelInfo {
            id: row.id,
            channel: row.spec,
            revision: row.revision,
            entries: row.entries,
            updated_at: row.updated_at,
            publisher_key: self.channel_public_key(),
        }
    }

    /// A space's channels.
    pub async fn channels(&self, space: SpaceId) -> Result<Vec<ChannelInfo>, CoreError> {
        let rows = self.auto_db(|db| db.channels()).await?;
        Ok(rows
            .into_iter()
            .filter(|r| r.space == space)
            .map(|r| self.channel_info(r))
            .collect())
    }

    async fn channel_row(&self, space: SpaceId, id: &str) -> Result<ChannelRow, CoreError> {
        let id = id.to_owned();
        self.auto_db(move |db| db.channel(&id))
            .await?
            .filter(|r| r.space == space)
            .ok_or(CoreError::NotFound)
    }

    pub async fn channel(&self, space: SpaceId, id: &str) -> Result<ChannelInfo, CoreError> {
        Ok(self.channel_info(self.channel_row(space, id).await?))
    }

    pub async fn create_channel(
        &self,
        space: SpaceId,
        req: CreateChannelRequest,
    ) -> Result<ChannelInfo, CoreError> {
        self.space(space)?;
        if !valid_slug(&req.id) {
            return Err(invalid(
                "a channel id is lowercase letters, digits and `-`, starting with a letter; at \
                 most 64",
            ));
        }
        Self::check_channel(&req.channel)?;
        self.check_selector(space, &req.channel).await?;
        let (id, spec) = (req.id.clone(), req.channel);
        let inserted = self
            .auto_db(move |db| {
                if db.channels()?.len() >= MAX_CHANNELS {
                    return Ok(None);
                }
                db.insert_channel(space, &id, &spec, now_ms()).map(Some)
            })
            .await?;
        match inserted {
            None => Err(invalid(format!("at most {MAX_CHANNELS} channels"))),
            // Channel ids are instance-wide (they name feeds); one is taken.
            Some(false) => Err(CoreError::Duplicate),
            Some(true) => {
                // Settle revision 1 now, so the first subscriber sees a stable revision.
                self.channel_feed(space, &req.id).await?;
                self.channel(space, &req.id).await
            }
        }
    }

    pub async fn update_channel(
        &self,
        space: SpaceId,
        id: &str,
        spec: ChannelSpec,
    ) -> Result<ChannelInfo, CoreError> {
        self.channel_row(space, id).await?;
        Self::check_channel(&spec)?;
        self.check_selector(space, &spec).await?;
        let id2 = id.to_owned();
        if !self
            .auto_db(move |db| db.update_channel(&id2, &spec))
            .await?
        {
            return Err(CoreError::NotFound);
        }
        if self.channel(space, id).await?.channel.enabled {
            self.channel_feed(space, id).await?;
        }
        self.channel(space, id).await
    }

    pub async fn delete_channel(&self, space: SpaceId, id: &str) -> Result<(), CoreError> {
        self.channel_row(space, id).await?;
        let id = id.to_owned();
        if self.auto_db(move |db| db.delete_channel(&id)).await? {
            Ok(())
        } else {
            Err(CoreError::NotFound)
        }
    }

    /// A space's channel's signed feed. Its revision moves on first if the entries changed.
    pub async fn channel_feed(&self, space: SpaceId, id: &str) -> Result<SignedChannel, CoreError> {
        let _feeds = self.auto().feeds.lock().await;
        let row = self.channel_row(space, id).await?;
        if !row.spec.enabled {
            return Err(CoreError::NotFound);
        }
        let entries = self.channel_entries(space, &row.spec).await?;
        let hash = channel::entries_hash(&entries);
        let mut revision = row.revision;
        if row.entries_hash.as_deref() != Some(hash.as_slice()) {
            revision += 1;
            let (id2, n) = (
                id.to_owned(),
                u32::try_from(entries.len()).unwrap_or(u32::MAX),
            );
            let h = hash.clone();
            self.auto_db(move |db| db.set_channel_revision(&id2, revision, &h, n, now_ms()))
                .await?;
            tracing::info!(channel = %id, revision, "channel revision");
        }
        let doc = ChannelDocument {
            format: CHANNEL_FORMAT,
            channel: row.id,
            name: row.spec.name,
            description: row.spec.description,
            revision,
            issued_at: now_ms(),
            entries,
        };
        Ok(self.auto().key.sign(&doc))
    }

    /// The torrents a channel lists, in a stable order.
    async fn channel_entries(
        &self,
        space: SpaceId,
        spec: &ChannelSpec,
    ) -> Result<Vec<ChannelEntry>, CoreError> {
        // (torrent id, title, version, published, metadata)
        type Pick = (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            serde_json::Map<String, serde_json::Value>,
        );
        let picks: Vec<Pick> = match &spec.selector {
            ChannelSelector::Rule { rule_id } => {
                let id = rule_id.clone();
                let (items, candidates) = self
                    .auto_db(move |db| Ok((db.items(&id)?, db.candidates(Some(space), None)?)))
                    .await?;
                items
                    .into_iter()
                    .filter(|i| i.item.state == RuleItemState::Active)
                    .filter_map(|i| {
                        let tid = i.item.torrent_id.clone()?;
                        let c = candidates.iter().find(|c| {
                            c.source == i.item.source && c.candidate.id == i.item.candidate_id
                        });
                        Some((
                            tid,
                            Some(i.item.title.clone()),
                            i.item.version.clone(),
                            c.and_then(|c| c.candidate.published.clone()),
                            c.map(|c| c.candidate.metadata.clone()).unwrap_or_default(),
                        ))
                    })
                    .collect()
            }
            ChannelSelector::Label { label } => {
                let mut list: Vec<_> = self
                    .list(Some(space))
                    .into_iter()
                    .filter(|t| t.label.as_deref() == Some(label.as_str()))
                    .collect();
                list.sort_by(|a, b| a.id.cmp(&b.id));
                list.into_iter()
                    .map(|t| {
                        (
                            t.id,
                            None,
                            None,
                            Some(super::version::format_rfc3339(t.added_at)),
                            serde_json::Map::new(),
                        )
                    })
                    .collect()
            }
        };
        let mut out = Vec::new();
        for (id, title, version, published, metadata) in picks.into_iter().take(MAX_ENTRIES) {
            let Some((name, magnet, size)) = self.channel_torrent(&Key::new(space, id.clone()))
            else {
                continue;
            };
            let torrent = if spec.include_metainfo {
                let id2 = id.clone();
                self.db(move |db| db.torrent_file(&id2))
                    .await?
                    .map(|b| B64.encode(b))
            } else {
                None
            };
            out.push(ChannelEntry {
                id,
                title: title.unwrap_or(name),
                version,
                published,
                metadata,
                magnet,
                torrent,
                size,
            });
        }
        Ok(out)
    }

    /// A torrent's name, magnet link and size, for a channel.
    fn channel_torrent(&self, key: &Key) -> Option<(String, String, u64)> {
        let id = key.id.as_str();
        if let Some(e) = self
            .inner
            .torrents
            .read()
            .expect("torrents lock poisoned")
            .get(key)
        {
            let info = &e.torrent.info;
            let mut m = Magnet::new(info.info_hashes());
            m.display_name = Some(info.name.clone());
            m.trackers = e.trackers.iter().flatten().cloned().collect();
            m.web_seeds.clone_from(&e.torrent.web_seeds);
            let size = info
                .files
                .iter()
                .filter(|f| !f.is_pad())
                .map(|f| f.length)
                .sum();
            return Some((info.name.clone(), m.to_uri(), size));
        }
        let pending = self.inner.pending.read().expect("pending lock poisoned");
        let p = pending.get(key)?;
        Some((
            p.magnet
                .display_name
                .clone()
                .unwrap_or_else(|| id.to_owned()),
            p.uri.clone(),
            0,
        ))
    }

    // --- Subscribing --------------------------------------------------------------------

    fn subscription_info(row: SubscriptionRow) -> SubscriptionInfo {
        SubscriptionInfo {
            id: row.id,
            name: row.spec.name,
            url: row.spec.url,
            channel: row.spec.channel,
            has_token: row.token.is_some(),
            trust: row.spec.trust,
            publisher_key: row.publisher_key,
            interval_minutes: row.spec.interval_minutes,
            enabled: row.spec.enabled,
            revision: row.revision,
            entries: row.entries,
            last_checked: row.last_checked,
            last_error: row.last_error,
            created_at: row.created_at,
        }
    }

    /// A space's subscriptions.
    pub async fn subscriptions(&self, space: SpaceId) -> Result<Vec<SubscriptionInfo>, CoreError> {
        let rows = self.auto_db(|db| db.subscriptions()).await?;
        Ok(rows
            .into_iter()
            .filter(|r| r.space == space)
            .map(Self::subscription_info)
            .collect())
    }

    async fn subscription_row(
        &self,
        space: SpaceId,
        id: &str,
    ) -> Result<SubscriptionRow, CoreError> {
        let id = id.to_owned();
        self.auto_db(move |db| db.subscription(&id))
            .await?
            .filter(|r| r.space == space)
            .ok_or(CoreError::NotFound)
    }

    pub async fn subscription(
        &self,
        space: SpaceId,
        id: &str,
    ) -> Result<SubscriptionInfo, CoreError> {
        Ok(Self::subscription_info(
            self.subscription_row(space, id).await?,
        ))
    }

    /// Check a subscription and settle its trust: a pairing code becomes the full hash of
    /// the key the publisher presents, if the code names it.
    async fn settle_subscription(&self, spec: &mut SubscriptionSpec) -> Result<(), CoreError> {
        if spec.name.trim().is_empty() || spec.name.len() > 200 {
            return Err(invalid("a subscription's name must be 1 to 200 characters"));
        }
        let base = apitorrent_client::remote::parse_base_url(&spec.url)
            .map_err(|e| invalid(e.to_string()))?;
        spec.url = base.to_string();
        if !valid_slug(&spec.channel) {
            return Err(invalid("that isn't a channel id"));
        }
        if !(5..=7 * 24 * 60).contains(&spec.interval_minutes) {
            return Err(invalid("check every 5 minutes to 7 days"));
        }
        if let Some(t) = &spec.token
            && (t.trim().is_empty() || t.len() > 500)
        {
            return Err(invalid("that isn't an API token"));
        }
        if let Some(k) = &spec.publisher_key {
            let raw = B64
                .decode(k.trim())
                .map_err(|_| invalid("the publisher key is base64 (32 bytes)"))?;
            if raw.len() != 32 {
                return Err(invalid("the publisher key is 32 bytes, base64"));
            }
            spec.publisher_key = Some(B64.encode(raw));
        }
        match &mut spec.trust {
            SubscriptionTrust::Os => {}
            SubscriptionTrust::PrivateCa { ca_pem } => {
                // Checked by building a verifier with it.
                apitorrent_client::trust::verifier(&Trust::PrivateCa {
                    ca_pem: ca_pem.clone(),
                })
                .map_err(|e| invalid(e.to_string()))?;
            }
            SubscriptionTrust::Pinned { pin } => {
                let full = if let Ok(h) = SpkiHash::parse(pin) {
                    h
                } else {
                    {
                        // A pairing code: learn the key the server presents, and pin it only
                        // if the code names it.
                        let probe = apitorrent_client::probe(&spec.url, &Trust::Os)
                            .await
                            .map_err(|e| invalid(e.to_string()))?;
                        let presented = probe
                            .presented
                            .ok_or_else(|| invalid("the publisher didn't present a certificate"))?;
                        if !presented.matches_code(pin) {
                            return Err(invalid(format!(
                                "the publisher's key ({presented}) doesn't match that pairing \
                                 code"
                            )));
                        }
                        presented
                    }
                };
                *pin = full.to_hex();
            }
        }
        Ok(())
    }

    pub async fn create_subscription(
        &self,
        space: SpaceId,
        mut spec: SubscriptionSpec,
    ) -> Result<SubscriptionInfo, CoreError> {
        self.space(space)?;
        self.settle_subscription(&mut spec).await?;
        let row = SubscriptionRow {
            id: new_id(),
            space,
            token: spec.token.take(),
            publisher_key: spec.publisher_key.clone(),
            spec,
            revision: None,
            entries: 0,
            last_checked: None,
            last_error: None,
            created_at: now_ms(),
        };
        let id = row.id.clone();
        let saved = self
            .auto_db(move |db| {
                if db.subscriptions()?.len() >= MAX_SUBSCRIPTIONS {
                    return Ok(false);
                }
                db.save_subscription(&row)?;
                Ok(true)
            })
            .await?;
        if !saved {
            return Err(invalid(format!(
                "at most {MAX_SUBSCRIPTIONS} subscriptions"
            )));
        }
        self.auto().poll_wake.notify_one();
        self.subscription(space, &id).await
    }

    /// Change a subscription. A missing token keeps the stored one; the pinned publisher
    /// key changes only if a new one is given.
    pub async fn update_subscription(
        &self,
        space: SpaceId,
        id: &str,
        mut spec: SubscriptionSpec,
    ) -> Result<SubscriptionInfo, CoreError> {
        let mut row = self.subscription_row(space, id).await?;
        self.settle_subscription(&mut spec).await?;
        if let Some(t) = spec.token.take() {
            row.token = Some(t);
        }
        if let Some(k) = spec.publisher_key.clone() {
            row.publisher_key = Some(k);
        }
        row.spec = spec;
        self.auto_db(move |db| db.save_subscription(&row)).await?;
        self.auto().poll_wake.notify_one();
        self.subscription(space, id).await
    }

    /// Delete a subscription and its candidates. Torrents rules took from it stay.
    pub async fn delete_subscription(&self, space: SpaceId, id: &str) -> Result<(), CoreError> {
        self.subscription_row(space, id).await?;
        let (id, id2) = (id.to_owned(), id.to_owned());
        let found = self
            .auto_db(move |db| {
                db.delete_candidates(None, &subscription_source(&id))?;
                db.delete_subscription(&id)
            })
            .await?;
        self.auto()
            .polling
            .lock()
            .expect("polling lock poisoned")
            .remove(&id2);
        if found {
            Ok(())
        } else {
            Err(CoreError::NotFound)
        }
    }

    /// Fetch a subscription's feed now (after any poll already under way); the result is
    /// in its `lastError` and `revision`.
    pub async fn refresh_subscription(
        &self,
        space: SpaceId,
        id: &str,
    ) -> Result<SubscriptionInfo, CoreError> {
        self.subscription_row(space, id).await?;
        let lock = self.poll_lock(id);
        let _polling = lock.lock().await;
        let row = self.subscription_row(space, id).await?;
        self.poll_subscription(row).await;
        self.subscription(space, id).await
    }

    fn poll_lock(&self, id: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        self.auto()
            .polling
            .lock()
            .expect("polling lock poisoned")
            .entry(id.to_owned())
            .or_default()
            .clone()
    }

    pub(crate) async fn poll_due_subscriptions(&self) {
        let Ok(rows) = self.auto_db(|db| db.subscriptions()).await else {
            return;
        };
        let now = now_ms();
        for row in rows {
            let interval = i64::from(row.spec.interval_minutes) * 60_000;
            let due = row.last_checked.is_none_or(|t| now - t >= interval);
            if !(row.spec.enabled && due) {
                continue;
            }
            // One being refreshed by hand right now is left to that.
            let lock = self.poll_lock(&row.id);
            let Ok(_polling) = lock.try_lock() else {
                continue;
            };
            self.poll_subscription(row).await;
        }
    }

    /// Poll once and record the outcome. Callers hold the subscription's poll lock.
    async fn poll_subscription(&self, row: SubscriptionRow) {
        let id = row.id.clone();
        let result = self.fetch_subscription(&row).await;
        let now = now_ms();
        let record = match result {
            Ok(Polled::Unchanged) => {
                self.auto_db(move |db| db.record_poll(&id, now, None, None, None))
                    .await
            }
            Ok(Polled::Accepted {
                revision,
                entries,
                key,
            }) => {
                tracing::info!(subscription = %id, revision, entries, "channel feed accepted");
                self.auto().wake.notify_one();
                self.auto_db(move |db| {
                    db.record_poll(&id, now, None, Some((revision, entries)), Some(&key))
                })
                .await
            }
            Err(e) => {
                tracing::warn!(subscription = %id, error = %e, "channel feed refused");
                self.auto_db(move |db| db.record_poll(&id, now, Some(&e), None, None))
                    .await
            }
        };
        if let Err(e) = record {
            tracing::warn!(error = %e, "could not record a subscription poll");
        }
    }

    /// Fetch, verify and store one subscription's feed.
    async fn fetch_subscription(&self, row: &SubscriptionRow) -> Result<Polled, String> {
        let spec = &row.spec;
        let trust = match &spec.trust {
            SubscriptionTrust::Os => Trust::Os,
            SubscriptionTrust::PrivateCa { ca_pem } => Trust::PrivateCa {
                ca_pem: ca_pem.clone(),
            },
            SubscriptionTrust::Pinned { pin } => Trust::Pinned {
                spki_sha256: SpkiHash::parse(pin).map_err(|e| e.to_string())?,
            },
        };
        let remote = Remote::new(&spec.url, &trust, row.token.clone().map(Token::new))
            .map_err(|e| e.to_string())?;
        let resp = remote
            .request(
                "GET",
                &format!("/api/v1/channels/{}/feed", spec.channel),
                None,
                None,
            )
            .await
            .map_err(|e| e.to_string())?;
        if resp.status != 200 {
            let why = serde_json::from_slice::<ApiError>(&resp.body).map_or_else(
                |_| {
                    String::from_utf8_lossy(&resp.body)
                        .chars()
                        .take(200)
                        .collect()
                },
                |e| e.message,
            );
            return Err(format!("the publisher answered {}: {why}", resp.status));
        }
        let feed: SignedChannel = serde_json::from_slice(&resp.body)
            .map_err(|e| format!("the publisher's answer isn't a channel feed: {e}"))?;
        let (doc, key) =
            channel::verify(&feed, row.publisher_key.as_deref()).map_err(|e| e.to_string())?;
        if doc.channel != spec.channel {
            return Err(format!(
                "the feed is for channel `{}`, not `{}`",
                doc.channel, spec.channel
            ));
        }
        match row.revision {
            Some(r) if doc.revision < r => {
                return Err(format!(
                    "the feed's revision went backwards ({} after {r}); refused",
                    doc.revision
                ));
            }
            Some(r) if doc.revision == r => return Ok(Polled::Unchanged),
            _ => {}
        }
        let entries = u32::try_from(doc.entries.len()).unwrap_or(u32::MAX);
        let mut candidates = Vec::with_capacity(doc.entries.len());
        for e in doc.entries {
            match entry_candidate(e) {
                Ok(c) => candidates.push(c),
                Err(why) => tracing::warn!(subscription = %row.id, %why, "channel entry skipped"),
            }
        }
        if !candidates.is_empty() {
            self.store_candidates(row.space, &subscription_source(&row.id), candidates)
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(Polled::Accepted {
            revision: doc.revision,
            entries,
            key,
        })
    }
}

enum Polled {
    Unchanged,
    Accepted {
        revision: u64,
        entries: u32,
        key: String,
    },
}

/// A channel entry as a candidate. Its `.torrent`, if any, must be the torrent it names.
fn entry_candidate(e: ChannelEntry) -> Result<(ReleaseCandidate, Option<Vec<u8>>), String> {
    let metainfo = match &e.torrent {
        Some(b64) => {
            let bytes = B64
                .decode(b64.trim())
                .map_err(|err| format!("{}: bad base64: {err}", e.id))?;
            if bytes.len() > MAX_METAINFO {
                return Err(format!("{}: the .torrent is too large", e.id));
            }
            let t = Torrent::from_bytes(&bytes).map_err(|err| format!("{}: {err}", e.id))?;
            let hashes = t.info_hashes();
            let names = [
                Some(hashes.id_string()),
                hashes.get_v1().map(|h| h.to_hex()),
            ];
            if !names.iter().flatten().any(|n| *n == e.id) {
                return Err(format!("{}: the .torrent is a different torrent", e.id));
            }
            Some(bytes)
        }
        None => None,
    };
    Ok((
        ReleaseCandidate {
            id: e.id,
            title: e.title,
            version: e.version,
            published: e.published,
            links: CandidateLinks {
                torrent: None,
                magnet: Some(e.magnet),
                webseed: None,
            },
            metadata: e.metadata,
        },
        metainfo,
    ))
}

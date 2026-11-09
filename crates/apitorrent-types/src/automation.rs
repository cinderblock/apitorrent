//! Automation: release candidates from sources, rules that take them, stable "latest"
//! aliases, and distribution channels between instances (`/api/v1/rules`,
//! `/api/v1/candidates`, `/api/v1/channels`, `/api/v1/subscriptions`).
//!
//! A **source** (a plugin with the `source` role, or a subscribed channel) submits
//! [`ReleaseCandidate`]s. A [`RuleSpec`] picks some of them, orders them by version and keeps
//! the latest N as torrents. A [`ChannelSpec`] publishes a signed, revisioned list of
//! torrents for other instances to subscribe to.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{SeedLimits, TorrentId};

/// A release a source offers. Sources submit these; rules decide which become torrents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ReleaseCandidate {
    /// Unique within its source, and stable: the same release always has the same id.
    pub id: String,
    pub title: String,
    /// The release's version as the source names it (`24.04.1`, `2026.11.08`).
    #[serde(default)]
    pub version: Option<String>,
    /// When it was published: ISO 8601 / RFC 3339 (`2026-11-08T12:00:00Z`).
    #[serde(default)]
    pub published: Option<String>,
    #[serde(default)]
    pub links: CandidateLinks,
    /// Anything else the source knows (flavour, architecture, size, …), for rules to match.
    #[serde(default)]
    #[ts(type = "Record<string, unknown>")]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

/// Where a candidate's data comes from. A rule needs `torrent` or `magnet` to take it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct CandidateLinks {
    /// A `.torrent` file's URL. A plugin source's link is fetched with that plugin's own
    /// network grants, only once a rule takes the candidate.
    #[serde(default)]
    pub torrent: Option<String>,
    #[serde(default)]
    pub magnet: Option<String>,
    /// A BEP 19 web seed. Added to a magnet link as `ws=`; a `.torrent` file carries its own.
    #[serde(default)]
    pub webseed: Option<String>,
}

/// A candidate as stored, with its source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct StoredCandidate {
    /// `plugin:<plugin id>` or `channel:<subscription id>`.
    pub source: String,
    pub candidate: ReleaseCandidate,
    /// The `.torrent` itself came with the candidate (channel entries do), so taking it
    /// fetches nothing.
    pub has_metainfo: bool,
    #[ts(type = "number")]
    pub first_seen: i64,
    #[ts(type = "number")]
    pub last_seen: i64,
}

/// What a source's submission did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SubmitCandidatesOutcome {
    /// Candidates stored (new or updated).
    pub accepted: u32,
    /// Why each refused candidate was refused, with its id.
    pub rejected: Vec<String>,
}

/// How a text is compared. Comparisons ignore case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum MatchMode {
    #[default]
    Contains,
    Equals,
    /// A shell-style pattern over the whole text: `*`, `?`, `[a-z]`.
    Glob,
    /// A regular expression (Rust `regex` syntax), found anywhere in the text.
    Regex,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct TextMatch {
    #[serde(default)]
    pub mode: MatchMode,
    pub pattern: String,
}

/// A condition on one metadata field. Numbers and booleans compare as their JSON text
/// (`true`, `42`); a list matches when any element does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct MetadataMatch {
    pub field: String,
    #[serde(flatten)]
    pub matches: TextMatch,
}

/// Which candidates a rule considers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
pub struct CandidateFilter {
    /// Sources to take from (`plugin:ubuntu-releases`, `channel:<id>`); empty means any.
    pub sources: Vec<String>,
    /// The title must match this.
    pub title: Option<TextMatch>,
    /// The title must not match this.
    pub exclude_title: Option<TextMatch>,
    /// Every one of these must match.
    pub metadata: Vec<MetadataMatch>,
}

/// How versions are compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum VersionScheme {
    /// Dotted numbers with an optional pre-release (`24.04.1`, `v2.1.0-rc.1`), compared as
    /// SemVer does but leniently: any number of parts, leading zeros allowed.
    #[default]
    Semver,
    /// A date or date and time: ISO 8601 / RFC 3339, `YYYY.MM.DD` or `YYYYMMDD`.
    Date,
    /// Text with runs of digits compared as numbers (`build-9` < `build-10`).
    Natural,
}

/// Which part of a candidate holds its version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum VersionField {
    #[default]
    Version,
    Title,
    Published,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
pub struct VersionOrder {
    pub scheme: VersionScheme,
    pub field: VersionField,
}

/// Which files of a taken torrent to download, by glob. As in `.gitignore`, a glob without a
/// `/` matches file names in any folder (`*.iso`); one with a `/` matches the whole path
/// inside the torrent, folder included (`ubuntu-26.04/casper/vmlinuz`; `**` crosses folders,
/// `*` doesn't). Nothing listed means every file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
pub struct FileSelection {
    /// A file is wanted if it matches any of these (or this is empty)…
    pub include: Vec<String>,
    /// …and none of these.
    pub exclude: Vec<String>,
}

/// What happens to a torrent a rule took once newer releases push it out of the latest N.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, TS)]
#[serde(tag = "action", rename_all = "camelCase")]
#[ts(export)]
pub enum SupersededAction {
    /// Leave it alone.
    #[default]
    KeepSeeding,
    /// Pause it now.
    Stop,
    /// Give it these seed limits as its own: it seeds until a ratio or a time, then is
    /// paused, removed, or removed with its data.
    SeedLimits { limits: SeedLimits },
}

/// A stable path that always names the newest completed release's file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct AliasSpec {
    /// Relative to the download directory: `library/ubuntu/latest-desktop-amd64.iso`.
    pub path: String,
    /// A file glob (as in [`FileSelection`]) choosing the torrent's file: the largest
    /// matching one. Empty: the largest file.
    #[serde(default)]
    pub file: Option<String>,
}

/// A rule: which candidates to take, how many to keep, and where they go.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct RuleSpec {
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default, rename = "match")]
    pub filter: CandidateFilter,
    #[serde(default)]
    pub order: VersionOrder,
    /// Keep the latest N matching releases; 0 keeps every one.
    #[serde(default = "one")]
    pub keep_latest: u32,
    #[serde(default)]
    pub label: Option<String>,
    /// Where new torrents go: an absolute path, or one relative to the download directory.
    #[serde(default)]
    pub save_path: Option<String>,
    #[serde(default)]
    pub files: FileSelection,
    #[serde(default)]
    pub superseded: SupersededAction,
    /// Add torrents paused.
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub alias: Option<AliasSpec>,
}

fn yes() -> bool {
    true
}

fn one() -> u32 {
    1
}

/// Where one candidate a rule took stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum RuleItemState {
    /// Among the latest N; its torrent was added.
    Active,
    /// Pushed out by newer releases; the superseded action was applied.
    Superseded,
    /// Its torrent was removed, by someone or by seed limits. It is never taken again.
    Gone,
    /// Adding it failed; it is retried on the next pass.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct RuleItem {
    pub source: String,
    pub candidate_id: String,
    pub title: String,
    pub version: Option<String>,
    pub torrent_id: Option<TorrentId>,
    pub state: RuleItemState,
    pub error: Option<String>,
    #[ts(type = "number")]
    pub taken_at: i64,
}

/// How an alias was last made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum AliasMethod {
    /// A hard link to the torrent's file: no extra space.
    HardLink,
    /// A copy (or a copy-on-write clone where the filesystem can): the alias is on another
    /// volume.
    Copy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct AliasStatus {
    /// The alias, as an absolute path on the server.
    pub path: String,
    pub torrent_id: Option<TorrentId>,
    /// The file it names, inside that torrent.
    pub file: Option<String>,
    pub method: Option<AliasMethod>,
    #[ts(type = "number | null")]
    pub updated_at: Option<i64>,
    /// Why the last update didn't happen (a file in use is retried).
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct RuleInfo {
    pub id: String,
    pub rule: RuleSpec,
    #[ts(type = "number")]
    pub created_at: i64,
    #[ts(type = "number")]
    pub updated_at: i64,
    pub items: Vec<RuleItem>,
    pub alias: Option<AliasStatus>,
    /// Why the last pass couldn't run the rule (a bad pattern, say).
    pub error: Option<String>,
}

/// What a dry run says about one matching candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum DryRunVerdict {
    /// Among the latest N, not taken yet: it would be added.
    Take,
    /// Already taken, and stays.
    Keep,
    /// Taken, but now outside the latest N: the superseded action would apply.
    Supersede,
    /// Matches, but older than the latest N: left alone.
    Older,
    /// Taken before and since removed: never taken again.
    Gone,
    /// Can't be used: no link, or its version doesn't parse.
    Unusable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DryRunEntry {
    pub source: String,
    pub candidate_id: String,
    pub title: String,
    pub version: Option<String>,
    pub published: Option<String>,
    /// 1 for the newest; `null` for unusable ones.
    pub rank: Option<u32>,
    pub verdict: DryRunVerdict,
    /// Why, for `unusable`.
    pub reason: Option<String>,
}

/// "What would this rule take from the current candidates." Nothing is changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DryRun {
    /// Every matching candidate, newest first, unusable ones last.
    pub entries: Vec<DryRunEntry>,
    /// Candidates considered (from the rule's sources) before matching.
    pub considered: u32,
}

/// The body of `POST /rules/dry-run`: a rule (saved or not), and optionally the id of the
/// saved rule whose taken items count.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct DryRunRequest {
    pub rule: RuleSpec,
    #[serde(default)]
    pub rule_id: Option<String>,
}

// --- Distribution channels ---------------------------------------------------------------

/// What a channel publishes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
#[ts(export)]
pub enum ChannelSelector {
    /// The torrents a rule currently keeps (its active items), with their candidates'
    /// versions and metadata.
    #[serde(rename_all = "camelCase")]
    Rule { rule_id: String },
    /// Every torrent with this label.
    Label { label: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ChannelSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub selector: ChannelSelector,
    /// Include each `.torrent` file (subscribers then fetch nothing but the data); otherwise
    /// magnet links only.
    #[serde(default = "yes")]
    pub include_metainfo: bool,
    #[serde(default = "yes")]
    pub enabled: bool,
}

/// The body of `POST /channels`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct CreateChannelRequest {
    /// Lowercase letters, digits and `-`, starting with a letter; at most 64. Subscribers
    /// name the channel by it.
    pub id: String,
    #[serde(flatten)]
    pub channel: ChannelSpec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ChannelInfo {
    pub id: String,
    pub channel: ChannelSpec,
    /// Bumped whenever the entries change; never goes backwards.
    #[ts(type = "number")]
    pub revision: u64,
    pub entries: u32,
    #[ts(type = "number")]
    pub updated_at: i64,
    /// This instance's channel signing key (Ed25519, base64): what subscribers pin.
    pub publisher_key: String,
}

/// One torrent in a channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ChannelEntry {
    pub id: TorrentId,
    pub title: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub published: Option<String>,
    #[serde(default)]
    #[ts(type = "Record<string, unknown>")]
    pub metadata: serde_json::Map<String, serde_json::Value>,
    pub magnet: String,
    /// The `.torrent` file, base64, when the channel includes metainfo.
    #[serde(default)]
    pub torrent: Option<String>,
    #[ts(type = "number")]
    pub size: u64,
}

/// The signed content of a channel feed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ChannelDocument {
    /// [`CHANNEL_FORMAT`].
    pub format: u32,
    /// The channel's id, so a feed can't be replayed as another channel.
    pub channel: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[ts(type = "number")]
    pub revision: u64,
    #[ts(type = "number")]
    pub issued_at: i64,
    pub entries: Vec<ChannelEntry>,
}

/// The channel feed format this server speaks.
pub const CHANNEL_FORMAT: u32 = 1;

/// `GET /channels/{id}/feed`: the document's exact JSON bytes and an Ed25519 signature over
/// `"apitorrent-channel-v1\0"` followed by those bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SignedChannel {
    /// [`CHANNEL_FORMAT`].
    pub format: u32,
    /// Base64 of the [`ChannelDocument`]'s JSON.
    pub payload: String,
    /// Base64 of the 64-byte signature.
    pub signature: String,
    /// Base64 of the 32-byte Ed25519 public key that signed it.
    pub public_key: String,
}

/// How a subscriber trusts the publisher's TLS certificate, as the thin client does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
#[ts(export)]
pub enum SubscriptionTrust {
    /// The operating system's verifier and trust store.
    Os,
    /// Only this CA (PEM).
    #[serde(rename_all = "camelCase")]
    PrivateCa { ca_pem: String },
    /// The publisher's key. On create, the short pairing code or the full SHA-256 of the
    /// key (hex or base64); stored and returned as the full hash in hex.
    Pinned { pin: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SubscriptionSpec {
    pub name: String,
    /// The publisher's base URL: `https://seedbox.example` (plain `http://` only to
    /// loopback).
    pub url: String,
    /// The channel's id on the publisher.
    pub channel: String,
    /// An API token for the publisher (a `read` token is enough). Write-only: never
    /// returned. On update, leave it out to keep the stored one.
    #[serde(default)]
    pub token: Option<String>,
    pub trust: SubscriptionTrust,
    /// The publisher's channel key (base64), pinned. Leave it out to pin the key the
    /// first feed is signed with (it arrives over the verified TLS connection).
    #[serde(default)]
    pub publisher_key: Option<String>,
    #[serde(default = "thirty")]
    pub interval_minutes: u32,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn thirty() -> u32 {
    30
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SubscriptionInfo {
    pub id: String,
    pub name: String,
    pub url: String,
    pub channel: String,
    pub has_token: bool,
    pub trust: SubscriptionTrust,
    pub publisher_key: Option<String>,
    pub interval_minutes: u32,
    pub enabled: bool,
    #[ts(type = "number | null")]
    pub revision: Option<u64>,
    pub entries: u32,
    #[ts(type = "number | null")]
    pub last_checked: Option<i64>,
    pub last_error: Option<String>,
    #[ts(type = "number")]
    pub created_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_defaults_and_shape() {
        let r: RuleSpec = serde_json::from_str(
            r#"{"name":"ubuntu","match":{"sources":["plugin:ubuntu-releases"],
                "metadata":[{"field":"flavor","mode":"equals","pattern":"desktop"}]},
                "superseded":{"action":"seedLimits","limits":{"ratio":2,"action":"remove"}}}"#,
        )
        .unwrap();
        assert!(r.enabled);
        assert_eq!(r.keep_latest, 1);
        assert_eq!(r.order.scheme, VersionScheme::Semver);
        assert_eq!(r.filter.metadata[0].matches.mode, MatchMode::Equals);
        match r.superseded {
            SupersededAction::SeedLimits { limits } => assert!((limits.ratio - 2.0).abs() < 1e-9),
            other => panic!("{other:?}"),
        }
        let back = serde_json::to_value(&r).unwrap();
        assert_eq!(back["match"]["metadata"][0]["field"], "flavor");
        assert_eq!(back["superseded"]["action"], "seedLimits");
    }

    #[test]
    fn channel_request_flattens() {
        let c: CreateChannelRequest = serde_json::from_str(
            r#"{"id":"ubuntu","name":"Ubuntu","selector":{"kind":"label","label":"iso"}}"#,
        )
        .unwrap();
        assert!(c.channel.include_metainfo);
        assert_eq!(
            c.channel.selector,
            ChannelSelector::Label {
                label: "iso".into()
            }
        );
    }
}

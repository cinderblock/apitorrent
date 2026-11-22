//! Changing many torrents at once: `PATCH /torrents` and `POST /torrents/settings`.
//!
//! Every field of a [`BulkEdit`] is optional: only the fields present change, and each
//! torrent keeps the rest of its own settings. A limit field left out keeps that torrent's
//! value, so "set every upload limit to 1 MiB/s" leaves each download limit as it was.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{SeedLimits, TorrentLimits, UnwantedPolicy};

/// `PATCH /torrents`: change settings on several torrents of the space.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct BulkEdit {
    /// The torrents to change. Each is changed on its own: one failing leaves the others.
    pub ids: Vec<String>,
    /// The new label; an empty string removes the label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub limits: Option<BulkLimits>,
    /// The torrents' own seed limits; `{ "limits": null }` makes them follow their group's
    /// or the instance's again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub seed_limits: Option<crate::SetSeedLimitsRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub trackers: Option<BulkTrackers>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub unwanted_policy: Option<UnwantedPolicy>,
    /// Move the torrents' files to this folder (absolute, inside the space's storage
    /// roots). Checked at once; the moves then run one at a time, and each torrent shows
    /// as `moving` until its move is done. A move that fails leaves the files where they
    /// were and shows why in the torrent's `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub save_path: Option<String>,
}

/// Rate and connection limits to set; a field left out keeps each torrent's own value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct BulkLimits {
    /// Bytes per second; 0 removes the torrent's own limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub download_rate: Option<u64>,
    /// Bytes per second; 0 removes the torrent's own limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub upload_rate: Option<u64>,
    /// Peers at once; 0 uses the server's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub max_connections: Option<u32>,
}

impl BulkLimits {
    /// `current` with this edit's fields in place.
    #[must_use]
    pub fn apply(&self, current: TorrentLimits) -> TorrentLimits {
        TorrentLimits {
            download_rate: self.download_rate.unwrap_or(current.download_rate),
            upload_rate: self.upload_rate.unwrap_or(current.upload_rate),
            max_connections: match self.max_connections {
                Some(0) => None,
                Some(n) => Some(n),
                None => current.max_connections,
            },
        }
    }
}

/// Changes to each torrent's announce list, applied in order: replace, remove, add.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct BulkTrackers {
    /// Swap one announce URL for another wherever a torrent lists it, keeping its tier.
    #[serde(default)]
    pub replace: Vec<TrackerReplace>,
    /// Drop these URLs from every torrent that lists them.
    #[serde(default)]
    pub remove: Vec<String>,
    /// Append these URLs, each as a tier of its own, to every torrent that doesn't list
    /// them yet.
    #[serde(default)]
    pub add: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct TrackerReplace {
    pub from: String,
    pub to: String,
}

impl BulkTrackers {
    /// Whether the edit changes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.replace.is_empty() && self.remove.is_empty() && self.add.is_empty()
    }

    /// `tiers` with this edit applied. URLs compare after trimming; a replacement whose new
    /// URL is already listed just drops the old one. Tiers left empty are dropped.
    #[must_use]
    pub fn apply(&self, tiers: &[Vec<String>]) -> Vec<Vec<String>> {
        let mut out: Vec<Vec<String>> = tiers
            .iter()
            .map(|t| t.iter().map(|u| u.trim().to_owned()).collect())
            .collect();
        let listed = |out: &Vec<Vec<String>>, url: &str| out.iter().flatten().any(|u| u == url);
        for r in &self.replace {
            let (from, to) = (r.from.trim(), r.to.trim());
            if from == to || to.is_empty() {
                continue;
            }
            let has_to = listed(&out, to);
            for tier in &mut out {
                if has_to {
                    tier.retain(|u| u != from);
                } else {
                    for u in tier.iter_mut().filter(|u| *u == from) {
                        to.clone_into(u);
                    }
                }
            }
        }
        for url in &self.remove {
            let url = url.trim();
            for tier in &mut out {
                tier.retain(|u| u != url);
            }
        }
        for url in &self.add {
            let url = url.trim();
            if !url.is_empty() && !listed(&out, url) {
                out.push(vec![url.to_owned()]);
            }
        }
        out.retain(|t| !t.is_empty());
        out
    }
}

/// What a bulk change did: the torrents changed, and why the others weren't.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct BulkReport {
    pub updated: Vec<String>,
    pub failed: Vec<BulkFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct BulkFailure {
    pub id: String,
    /// An [`crate::ApiError`] code: `not_found`, `invalid_request`, ….
    pub code: String,
    pub message: String,
}

/// `POST /torrents/settings`: the torrents to read the editable settings of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct TorrentSettingsRequest {
    pub ids: Vec<String>,
}

/// A torrent's own settings, the ones a [`BulkEdit`] changes. Torrents not in the space
/// are left out of the answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct TorrentSettings {
    pub id: String,
    pub label: Option<String>,
    /// The folder its files are saved in.
    pub save_path: String,
    pub limits: TorrentLimits,
    /// Its own seed limits; `null` follows its group's or the instance's.
    pub seed_limits: Option<SeedLimits>,
    pub trackers: Vec<Vec<String>>,
    pub unwanted_policy: UnwantedPolicy,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiers(t: &[&[&str]]) -> Vec<Vec<String>> {
        t.iter()
            .map(|t| t.iter().map(|&u| u.to_owned()).collect())
            .collect()
    }

    #[test]
    fn limits_keep_fields_left_out() {
        let current = TorrentLimits {
            download_rate: 100,
            upload_rate: 200,
            max_connections: Some(30),
        };
        let edit = BulkLimits {
            upload_rate: Some(50),
            ..BulkLimits::default()
        };
        assert_eq!(
            edit.apply(current),
            TorrentLimits {
                upload_rate: 50,
                ..current
            }
        );
        let reset = BulkLimits {
            max_connections: Some(0),
            ..BulkLimits::default()
        };
        assert_eq!(reset.apply(current).max_connections, None);
    }

    #[test]
    fn trackers_replace_remove_add() {
        let t = tiers(&[&["http://a/ann", "http://b/ann"], &["udp://c:80"]]);
        let edit = BulkTrackers {
            replace: vec![TrackerReplace {
                from: "http://a/ann".into(),
                to: "https://a/ann".into(),
            }],
            remove: vec!["udp://c:80".into()],
            add: vec!["http://d/ann".into(), " http://b/ann ".into()],
        };
        assert_eq!(
            edit.apply(&t),
            tiers(&[&["https://a/ann", "http://b/ann"], &["http://d/ann"]])
        );
    }

    #[test]
    fn replace_onto_a_listed_url_drops_the_old_one() {
        let t = tiers(&[&["http://a/ann"], &["http://b/ann"]]);
        let edit = BulkTrackers {
            replace: vec![TrackerReplace {
                from: "http://a/ann".into(),
                to: "http://b/ann".into(),
            }],
            ..BulkTrackers::default()
        };
        assert_eq!(edit.apply(&t), tiers(&[&["http://b/ann"]]));
    }

    #[test]
    fn optional_fields_are_left_out_of_json() {
        let edit = BulkEdit {
            ids: vec!["x".into()],
            label: Some(String::new()),
            ..BulkEdit::default()
        };
        assert_eq!(
            serde_json::to_string(&edit).unwrap(),
            r#"{"ids":["x"],"label":""}"#
        );
    }
}

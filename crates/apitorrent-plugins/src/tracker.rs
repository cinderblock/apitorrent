//! `tracker-adapter` plugins, as the core sees them.
//!
//! An adapter names the trackers it covers when it provides the role:
//!
//! ```ts
//! api.roles.provide("tracker-adapter", {
//!   trackers: ["tracker.example.org", "*.example.net"],
//!   isUnregistered({ tracker, message }) { return /deleted|dupe/i.test(message); },
//!   async findPack({ name, files }) { … return { magnet } or { torrent: bytes } or null },
//! });
//! ```
//!
//! For a tracker an adapter covers, its `isUnregistered` replaces the core's built-in
//! patterns, and its `findPack` (optional) names the pack that holds a torrent's files. A
//! name pattern covers that host exactly; `*.example.net` covers its subdomains, not
//! `example.net` itself. When two adapters cover a tracker, the first by plugin id wins.

use apitorrent_plugin_api::{
    BoxFuture, PackQuery, PluginLogLevel, PluginRole, TrackerAdapters, UnregisteredQuery,
};
use apitorrent_types::AddSource;
use serde::Deserialize;

use crate::PluginHost;

/// A host name, or `*.` and a domain: lowercase letters, digits, `-` and `.`.
pub(crate) fn valid_tracker_pattern(p: &str) -> bool {
    let host = p.strip_prefix("*.").unwrap_or(p);
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
}

fn pattern_matches(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(domain) => host
            .strip_suffix(domain)
            .is_some_and(|rest| rest.len() > 1 && rest.ends_with('.')),
        None => pattern == host,
    }
}

/// The host of an announce URL (`udp://tracker.example.org:6969/announce`), lowercased.
fn tracker_host(tracker: &str) -> Option<String> {
    let url = url::Url::parse(tracker).ok()?;
    Some(url.host_str()?.trim_end_matches('.').to_ascii_lowercase())
}

/// The role object a plugin provided: its plain data and the names of its methods.
#[derive(Debug, Deserialize)]
struct Provided {
    #[serde(default)]
    data: serde_json::Value,
    #[serde(default)]
    methods: Vec<String>,
}

/// [`TrackerAdapters`] over the host's running `tracker-adapter` plugins.
#[derive(Debug, Clone)]
pub struct PluginTrackerAdapters {
    host: PluginHost,
}

impl PluginTrackerAdapters {
    pub fn new(host: PluginHost) -> Self {
        Self { host }
    }

    /// The adapter covering this announce URL, with the methods it provides.
    fn adapter_for(&self, tracker: &str) -> Option<(String, Vec<String>)> {
        let host = tracker_host(tracker)?;
        self.host
            .providers(PluginRole::TrackerAdapter)
            .into_iter()
            .find_map(|(id, provided)| {
                let p: Provided = serde_json::from_value(provided).ok()?;
                let covers = p
                    .data
                    .get("trackers")?
                    .as_array()?
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .any(|pattern| pattern_matches(pattern, &host));
                covers.then_some((id, p.methods))
            })
    }

    async fn call(
        &self,
        id: &str,
        method: &str,
        arg: serde_json::Value,
    ) -> Option<serde_json::Value> {
        match self
            .host
            .call_role(
                id,
                PluginRole::TrackerAdapter,
                method,
                serde_json::json!([arg]),
            )
            .await
        {
            Ok(v) => Some(v),
            Err(e) => {
                self.host.slot_logs(id).host(
                    PluginLogLevel::Warn,
                    &format!("tracker-adapter {method}() failed: {e}"),
                );
                None
            }
        }
    }
}

/// What `findPack` may return.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Pack {
    Magnet {
        magnet: String,
    },
    /// Base64 of the `.torrent` file (the host's JavaScript encodes bytes).
    Torrent {
        torrent: String,
    },
}

impl TrackerAdapters for PluginTrackerAdapters {
    fn covers(&self, tracker: &str) -> bool {
        self.adapter_for(tracker).is_some()
    }

    fn unregistered(&self, query: UnregisteredQuery) -> BoxFuture<Option<bool>> {
        let this = self.clone();
        Box::pin(async move {
            let (id, methods) = this.adapter_for(&query.tracker)?;
            if !methods.iter().any(|m| m == "isUnregistered") {
                return None;
            }
            let arg = serde_json::to_value(&query).ok()?;
            match this.call(&id, "isUnregistered", arg).await? {
                serde_json::Value::Bool(b) => Some(b),
                other => {
                    this.host.slot_logs(&id).host(
                        PluginLogLevel::Warn,
                        &format!(
                            "tracker-adapter isUnregistered() returned {other}, not a boolean"
                        ),
                    );
                    None
                }
            }
        })
    }

    fn find_pack(&self, query: PackQuery) -> BoxFuture<Option<AddSource>> {
        let this = self.clone();
        Box::pin(async move {
            let (id, methods) = this.adapter_for(&query.tracker)?;
            if !methods.iter().any(|m| m == "findPack") {
                return None;
            }
            let arg = serde_json::to_value(&query).ok()?;
            let result = this.call(&id, "findPack", arg).await?;
            if result.is_null() {
                return None;
            }
            match serde_json::from_value::<Pack>(result) {
                Ok(Pack::Magnet { magnet }) => Some(AddSource::Magnet { uri: magnet }),
                Ok(Pack::Torrent { torrent }) => Some(AddSource::TorrentFile { data: torrent }),
                Err(_) => {
                    this.host.slot_logs(&id).host(
                        PluginLogLevel::Warn,
                        "tracker-adapter findPack() must return { magnet }, { torrent } or null",
                    );
                    None
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns() {
        for ok in ["tracker.example.org", "*.example.net", "a-b.c1"] {
            assert!(valid_tracker_pattern(ok), "{ok}");
        }
        for bad in [
            "",
            "*.",
            "Tracker.example",
            "a..b",
            "-a.b",
            "a/b",
            "*",
            "x.*.y",
        ] {
            assert!(!valid_tracker_pattern(bad), "{bad}");
        }
        assert!(pattern_matches(
            "tracker.example.org",
            "tracker.example.org"
        ));
        assert!(!pattern_matches(
            "tracker.example.org",
            "x.tracker.example.org"
        ));
        assert!(pattern_matches("*.example.net", "a.example.net"));
        assert!(pattern_matches("*.example.net", "a.b.example.net"));
        assert!(!pattern_matches("*.example.net", "example.net"));
        assert!(!pattern_matches("*.example.net", "badexample.net"));
        assert_eq!(
            tracker_host("udp://Tracker.Example.org:6969/announce").as_deref(),
            Some("tracker.example.org")
        );
        assert_eq!(
            tracker_host("https://t.example.net./a?passkey=x").as_deref(),
            Some("t.example.net")
        );
        assert_eq!(tracker_host("not a url"), None);
    }
}

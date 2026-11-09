//! Types of the plugin management endpoints (`/api/v1/plugins`) and of plugin manifests.
//!
//! A plugin is a single ESM bundle plus a manifest. The manifest declares everything the
//! plugin may do; an admin approves those capabilities before it runs, and again whenever an
//! upgrade asks for more. Settings schemas and values are JSON (`unknown` in TypeScript):
//! the schema is a JSON Schema subset the host validates and the UI renders.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::auth::Role;

/// The plugin API version this server implements. A manifest's `apiVersion` must match.
pub const PLUGIN_API_VERSION: u32 = 1;

/// The file format version of a plugin bundle (see [`PluginBundle`]).
pub const PLUGIN_BUNDLE_FORMAT: u32 = 1;

/// What a plugin declares about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct PluginManifest {
    /// Lowercase letters, digits and `-`, starting with a letter; at most 64 characters.
    /// It names the plugin's routes (`/api/v1/plugins/<id>/…`) and its data.
    pub id: String,
    /// A display name.
    pub name: String,
    /// Any non-empty version string (`1.2.0`, `2026.11.08`). Installing a different version
    /// of an installed plugin is an upgrade.
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    /// Must equal [`PLUGIN_API_VERSION`].
    pub api_version: u32,
    #[serde(default)]
    pub capabilities: PluginCapabilities,
    /// A JSON Schema (subset) for the plugin's settings. The UI renders a form from it.
    #[serde(default)]
    #[ts(type = "Record<string, unknown> | null")]
    pub settings: Option<serde_json::Value>,
}

/// Everything a plugin may do beyond pure computation. Undeclared calls fail.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct PluginCapabilities {
    /// Hosts `fetch` may reach: `example.com`, `*.example.com` (subdomains only),
    /// `example.com:8443`, or with `http://` in front to allow plain HTTP. Without a scheme
    /// only HTTPS on the default port is allowed. A port of `*` allows any port.
    #[serde(default)]
    pub network: Vec<String>,
    /// Settings fields whose values are also network grants, so an admin can point the
    /// plugin at hosts of their choosing (a feed's URL, say) without a new manifest. Paths
    /// name string properties: `feedUrl`, `feeds[].url`, `extraHosts[]`. A URL value
    /// grants its own origin (plain HTTP only when the URL says `http://`); any other value
    /// is read as a grant pattern, as in `network`.
    #[serde(default)]
    pub network_from_settings: Vec<String>,
    /// Calls into apitorrent.
    #[serde(default)]
    pub api: Vec<PluginApiScope>,
    /// HTTP routes the plugin serves under `/api/v1/plugins/<id>`.
    #[serde(default)]
    pub routes: Vec<PluginRoute>,
    /// A private key-value store.
    #[serde(default)]
    pub kv: bool,
    /// Jobs the host runs on an interval.
    #[serde(default)]
    pub schedule: bool,
    /// Roles the plugin fills for the host.
    #[serde(default)]
    pub roles: Vec<PluginRole>,
}

/// A call into apitorrent a plugin may make.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[ts(export)]
pub enum PluginApiScope {
    /// List torrents and read their details.
    #[serde(rename = "torrents:read")]
    TorrentsRead,
    /// Add torrents and magnet links.
    #[serde(rename = "torrents:add")]
    TorrentsAdd,
    /// Pause, resume and label torrents.
    #[serde(rename = "torrents:control")]
    TorrentsControl,
    /// Remove torrents, optionally with their data.
    #[serde(rename = "torrents:remove")]
    TorrentsRemove,
    /// Receive torrent events (added, removed, state changes, finished).
    #[serde(rename = "events:read")]
    EventsRead,
    /// Check apitorrent API tokens that clients present (as a password, say), and learn
    /// whose they are. For compatibility layers whose clients sign in their own way.
    #[serde(rename = "tokens:verify")]
    TokensVerify,
}

/// An HTTP route a plugin serves.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct PluginRoute {
    /// Relative to `/api/v1/plugins/<id>`: `/status` matches exactly, `/feeds/*` matches
    /// `/feeds/` and everything below it. It may not start with `/-`, which the management
    /// API uses.
    pub path: String,
    /// Upper-case HTTP methods, such as `GET` and `POST`.
    pub methods: Vec<String>,
    /// Who authenticates requests to this route. Defaults to `api`.
    #[serde(default)]
    pub auth: PluginRouteAuth,
}

/// Who authenticates the requests to a plugin route.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS,
)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum PluginRouteAuth {
    /// apitorrent does, as for the rest of the API: a session or an API token, and only
    /// viewers' `GET`s are read-only. The plugin is told who called.
    #[default]
    Api,
    /// The plugin does. apitorrent lets requests through without signing in (still telling
    /// the plugin who called, when the request did authenticate), and the plugin checks the
    /// client's credentials itself, with `tokens:verify`. Such a route may set cookies, and
    /// it sees the request's cookies other than apitorrent's own. Only `api-compat` plugins
    /// may declare one: it exists for other clients' APIs (qBittorrent's login, say).
    Plugin,
}

/// A role a plugin fills: it provides an object of handlers the host calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum PluginRole {
    /// Turns some upstream (a feed, a release page) into release candidates.
    Source,
    /// Speaks another client's API (such as qBittorrent's) for existing tools.
    ApiCompat,
    /// Publishes DNS TXT records for ACME DNS-01.
    DnsProvider,
    /// Tracker-specific behaviour: "unregistered" detection, pack lookups.
    TrackerAdapter,
}

/// The install file: a manifest plus the bundled code (`*.plugin.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct PluginBundle {
    /// Must equal [`PLUGIN_BUNDLE_FORMAT`].
    pub format: u32,
    pub manifest: PluginManifest,
    /// One ES module whose default export is the plugin (`definePlugin({ start })`).
    pub code: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum PluginStatus {
    /// Installed, but no version has been approved yet.
    AwaitingApproval,
    Disabled,
    Starting,
    Running,
    /// It stopped on an error, or the host stopped it (see `error`). Enable it again to
    /// restart it.
    Failed,
}

/// An installed plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PluginInfo {
    pub id: String,
    pub status: PluginStatus,
    pub enabled: bool,
    /// The approved version that runs, if any.
    pub active: Option<PluginManifest>,
    /// The capabilities granted to the active version.
    pub granted: PluginCapabilities,
    /// A newer (or older) version that needs approval before it runs.
    pub pending: Option<PendingPlugin>,
    /// Why the plugin last stopped on its own, if it did.
    pub error: Option<String>,
    #[ts(type = "number")]
    pub installed_at: i64,
    #[ts(type = "number")]
    pub updated_at: i64,
}

/// A version waiting for approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PendingPlugin {
    pub manifest: PluginManifest,
    /// What it asks for beyond what is granted now, in words. Everything, for a new plugin.
    pub new_capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct InstallPluginResponse {
    pub plugin: PluginInfo,
    /// The installed version waits for approval: it is new, or asks for more than the
    /// running version was granted.
    pub needs_approval: bool,
}

/// Approve exactly the pending version the admin looked at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ApprovePluginRequest {
    pub version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum PluginLogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// One line of a plugin's log: its `console` output and what the host says about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PluginLogEntry {
    /// Increases by one per entry; pass the last one seen as `after` to poll for more.
    #[ts(type = "number")]
    pub seq: u64,
    #[ts(type = "number")]
    pub time: i64,
    pub level: PluginLogLevel,
    /// The host's own messages (start, stop, watchdog) rather than the plugin's.
    pub host: bool,
    pub message: String,
}

/// A plugin's settings: the schema, and the current values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PluginSettings {
    #[ts(type = "Record<string, unknown> | null")]
    pub schema: Option<serde_json::Value>,
    /// Current values, with defaults filled in. Secrets (`writeOnly` properties) are left
    /// out.
    #[ts(type = "Record<string, unknown>")]
    pub values: serde_json::Value,
    /// The secrets that have a value.
    pub secrets_set: Vec<String>,
}

/// New settings. A secret left out keeps its value; set it to `null` to clear it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PutPluginSettingsRequest {
    #[ts(type = "Record<string, unknown>")]
    pub values: serde_json::Value,
}

/// Who called a plugin's HTTP route, as the plugin sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PluginCaller {
    pub user_id: String,
    pub user_name: String,
    pub role: Role,
    /// The credential may change things: not a viewer, not a read-only token. A plugin
    /// acting for a caller on a route that does its own login must check this before
    /// writes (on ordinary routes apitorrent already refuses read-only writes).
    pub may_write: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_json_shape() {
        let m: PluginManifest = serde_json::from_str(
            r#"{
                "id": "rss", "name": "RSS", "version": "1.0.0", "apiVersion": 1,
                "capabilities": {
                    "network": ["*.example.com"],
                    "api": ["torrents:add", "events:read"],
                    "routes": [{"path": "/status", "methods": ["GET"]}],
                    "kv": true, "schedule": true, "roles": ["source", "dns-provider"]
                },
                "settings": {"type": "object", "properties": {}}
            }"#,
        )
        .unwrap();
        assert_eq!(
            m.capabilities.api,
            [PluginApiScope::TorrentsAdd, PluginApiScope::EventsRead]
        );
        assert_eq!(
            m.capabilities.roles,
            [PluginRole::Source, PluginRole::DnsProvider]
        );
        // Unknown keys are mistakes, not extensions.
        assert!(
            serde_json::from_str::<PluginManifest>(
                r#"{"id":"x","name":"x","version":"1","apiVersion":1,"capabilitys":{}}"#
            )
            .is_err()
        );
    }
}

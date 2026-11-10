//! Manifest rules, checked at install.

use apitorrent_types::plugins::{
    PLUGIN_API_VERSION, PLUGIN_BUNDLE_FORMAT, PluginApiScope, PluginBundle, PluginManifest,
    PluginRole, PluginRoute, PluginRouteAuth,
};

use crate::network::NetworkGrant;
use crate::schema;

/// The largest bundle the host accepts: the code of one ES module.
pub const MAX_CODE_BYTES: usize = 16 * 1024 * 1024;

const MAX_NETWORK: usize = 64;
const MAX_ROUTES: usize = 64;

/// HTTP methods a plugin route may declare.
pub const METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"];

/// Every problem with a bundle, joined into one message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid plugin: {}", .0.join("; "))]
pub struct ValidationError(pub Vec<String>);

pub fn bundle(b: &PluginBundle) -> Result<(), ValidationError> {
    let mut problems = Vec::new();
    if b.format != PLUGIN_BUNDLE_FORMAT {
        problems.push(format!(
            "bundle format {} is not supported (expected {PLUGIN_BUNDLE_FORMAT})",
            b.format
        ));
    }
    if b.code.trim().is_empty() {
        problems.push("the bundle has no code".to_owned());
    }
    if b.code.len() > MAX_CODE_BYTES {
        problems.push(format!(
            "the code is {} bytes; the limit is {MAX_CODE_BYTES}",
            b.code.len()
        ));
    }
    manifest_into(&b.manifest, &mut problems);
    if problems.is_empty() {
        Ok(())
    } else {
        Err(ValidationError(problems))
    }
}

pub fn manifest(m: &PluginManifest) -> Result<(), ValidationError> {
    let mut problems = Vec::new();
    manifest_into(m, &mut problems);
    if problems.is_empty() {
        Ok(())
    } else {
        Err(ValidationError(problems))
    }
}

fn manifest_into(m: &PluginManifest, problems: &mut Vec<String>) {
    if !valid_id(&m.id) {
        problems.push(format!(
            "id `{}` must be 1 to 64 lowercase letters, digits and single dashes, starting \
             with a letter",
            m.id
        ));
    }
    if m.name.trim().is_empty() || m.name.chars().count() > 100 || has_control(&m.name) {
        problems.push("name must be 1 to 100 printable characters".to_owned());
    }
    let version_ok = m.version.starts_with(|c: char| c.is_ascii_alphanumeric())
        && m.version.len() <= 64
        && m.version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'));
    if !version_ok {
        problems.push(format!(
            "version `{}` must be 1 to 64 letters, digits, `.`, `-`, `+` or `_`, starting \
             with a letter or digit",
            m.version
        ));
    }
    if m.description.chars().count() > 2000 {
        problems.push("description is longer than 2000 characters".to_owned());
    }
    if m.api_version != PLUGIN_API_VERSION {
        problems.push(format!(
            "apiVersion {} is not supported (this host implements {PLUGIN_API_VERSION})",
            m.api_version
        ));
    }
    let caps = &m.capabilities;
    if caps.network.len() > MAX_NETWORK {
        problems.push(format!("more than {MAX_NETWORK} network grants"));
    }
    for g in &caps.network {
        if let Err(e) = NetworkGrant::parse(g) {
            problems.push(e.to_string());
        }
    }
    if caps.routes.len() > MAX_ROUTES {
        problems.push(format!("more than {MAX_ROUTES} routes"));
    }
    for r in &caps.routes {
        if let Err(e) = route(r) {
            problems.push(e);
        }
    }
    // Signing clients in its own way is what a compatibility layer does, and nothing else.
    if !caps.roles.contains(&PluginRole::ApiCompat) {
        if caps
            .routes
            .iter()
            .any(|r| r.auth == PluginRouteAuth::Plugin)
        {
            problems
                .push("routes with `\"auth\": \"plugin\"` need the `api-compat` role".to_owned());
        }
        if caps.api.contains(&PluginApiScope::TokensVerify) {
            problems.push("`tokens:verify` needs the `api-compat` role".to_owned());
        }
    }
    if let Some(s) = &m.settings
        && let Err(e) = schema::check_schema(s)
    {
        problems.push(format!("settings schema: {e}"));
    }
    for path in &caps.network_from_settings {
        let leaf = m
            .settings
            .as_ref()
            .and_then(|s| schema::resolve_path(s, path));
        if leaf
            .and_then(|l| l.get("type"))
            .and_then(serde_json::Value::as_str)
            != Some("string")
        {
            problems.push(format!(
                "networkFromSettings `{path}` must name a string setting in the settings schema"
            ));
        }
    }
}

/// `rss`, `qbittorrent-api`: lowercase, digits, single dashes, starting with a letter.
pub fn valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_lowercase()
        && b.last() != Some(&b'-')
        && !id.contains("--")
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

fn has_control(s: &str) -> bool {
    s.chars().any(char::is_control)
}

fn route(r: &PluginRoute) -> Result<(), String> {
    let p = &r.path;
    let body = p.strip_suffix("/*").unwrap_or(p);
    let ok = p.starts_with('/')
        && !p.starts_with("/-")
        && p.len() <= 256
        && !p.contains("//")
        && !body.contains('*')
        && body.split('/').all(|seg| seg != "." && seg != "..")
        && body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.' | '~'));
    if !ok {
        return Err(format!(
            "route `{p}` must start with `/` (not `/-`), use letters, digits, `-`, `_`, `.` \
             and `~`, and may end in `/*`"
        ));
    }
    if r.methods.is_empty() {
        return Err(format!("route `{p}` lists no methods"));
    }
    for m in &r.methods {
        if !METHODS.contains(&m.as_str()) {
            return Err(format!(
                "route `{p}`: method `{m}` is not one of {}",
                METHODS.join(", ")
            ));
        }
    }
    Ok(())
}

/// Whether a declared route path matches a request path (both relative to the plugin's
/// prefix). `/feeds/*` matches `/feeds/` and anything below it, but not `/feeds` or
/// `/feedsx`.
pub fn route_matches(declared: &str, path: &str) -> bool {
    match declared.strip_suffix('*') {
        Some(prefix) => path.starts_with(prefix),
        None => declared == path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apitorrent_types::plugins::PluginCapabilities;

    fn manifest_with(caps: PluginCapabilities) -> PluginManifest {
        PluginManifest {
            id: "rss".into(),
            name: "RSS".into(),
            version: "1.0.0".into(),
            description: String::new(),
            author: None,
            homepage: None,
            api_version: PLUGIN_API_VERSION,
            capabilities: caps,
            settings: None,
            space_aware: false,
        }
    }

    #[test]
    fn ids() {
        for id in ["rss", "qbittorrent-api", "a1", "a-b-c"] {
            assert!(valid_id(id), "{id}");
        }
        for id in [
            "",
            "Rss",
            "1rss",
            "rss-",
            "r--s",
            "r_s",
            "r s",
            &"a".repeat(65),
        ] {
            assert!(!valid_id(id), "{id}");
        }
    }

    #[test]
    fn routes() {
        let route = |path: &str, methods: &[&str]| PluginCapabilities {
            routes: vec![PluginRoute {
                path: path.into(),
                methods: methods.iter().map(|m| (*m).to_owned()).collect(),
                auth: PluginRouteAuth::Api,
            }],
            ..PluginCapabilities::default()
        };
        assert!(manifest(&manifest_with(route("/status", &["GET"]))).is_ok());
        assert!(manifest(&manifest_with(route("/api/v2/*", &["GET", "POST"]))).is_ok());
        for bad in [
            "status", "/-/logs", "/a//b", "/a/*/b", "/../x", "/a b", "/a*",
        ] {
            assert!(
                manifest(&manifest_with(route(bad, &["GET"]))).is_err(),
                "{bad}"
            );
        }
        assert!(manifest(&manifest_with(route("/x", &[]))).is_err());
        assert!(manifest(&manifest_with(route("/x", &["get"]))).is_err());
        assert!(manifest(&manifest_with(route("/x", &["CONNECT"]))).is_err());

        assert!(route_matches("/status", "/status"));
        assert!(!route_matches("/status", "/status/"));
        assert!(route_matches("/feeds/*", "/feeds/"));
        assert!(route_matches("/feeds/*", "/feeds/a/b"));
        assert!(!route_matches("/feeds/*", "/feeds"));
        assert!(!route_matches("/feeds/*", "/feedsx"));
    }

    #[test]
    fn own_login_needs_the_api_compat_role() {
        let caps = |roles: Vec<PluginRole>| PluginCapabilities {
            api: vec![PluginApiScope::TokensVerify],
            routes: vec![PluginRoute {
                path: "/api/v2/*".into(),
                methods: vec!["POST".into()],
                auth: PluginRouteAuth::Plugin,
            }],
            roles,
            ..PluginCapabilities::default()
        };
        let err = manifest(&manifest_with(caps(vec![PluginRole::Source]))).unwrap_err();
        assert_eq!(err.0.len(), 2, "{err}");
        assert!(manifest(&manifest_with(caps(vec![PluginRole::ApiCompat]))).is_ok());
    }

    #[test]
    fn collects_every_problem() {
        let mut m = manifest_with(PluginCapabilities {
            network: vec!["ftp://x".into()],
            ..PluginCapabilities::default()
        });
        m.api_version = 9;
        m.version = String::new();
        let err = manifest(&m).unwrap_err();
        assert_eq!(err.0.len(), 3, "{err}");
    }
}

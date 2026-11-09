//! What a plugin version asks for beyond what was granted, in words an admin can approve.

use apitorrent_types::plugins::{PluginApiScope, PluginCapabilities, PluginRole, PluginRouteAuth};

use crate::network::NetworkGrant;

/// A plain-language line for one API scope.
pub fn describe_scope(s: PluginApiScope) -> &'static str {
    match s {
        PluginApiScope::TorrentsRead => "see all torrents and their details",
        PluginApiScope::TorrentsAdd => "add torrents and magnet links",
        PluginApiScope::TorrentsControl => "pause, resume and label torrents",
        PluginApiScope::TorrentsRemove => "remove torrents, and delete their data",
        PluginApiScope::EventsRead => "be told when torrents are added, change state or finish",
        PluginApiScope::TokensVerify => {
            "check the apitorrent API tokens its clients sign in with, and learn whose they are"
        }
    }
}

pub fn describe_role(r: PluginRole) -> &'static str {
    match r {
        PluginRole::Source => "act as a release source",
        PluginRole::ApiCompat => "act as another client's API (compatibility layer)",
        PluginRole::DnsProvider => "publish DNS records for certificates (DNS provider)",
        PluginRole::TrackerAdapter => "adapt behaviour to specific trackers",
    }
}

/// A plain-language line for serving one method of a route.
pub fn describe_route(method: &str, path: &str, auth: PluginRouteAuth) -> String {
    match auth {
        PluginRouteAuth::Api => format!("serve {method} {path} under its API path"),
        PluginRouteAuth::Plugin => format!(
            "serve {method} {path} under its API path to clients that haven't signed in to \
             apitorrent (it checks their credentials itself, and may set cookies there)"
        ),
    }
}

/// Everything in `wanted` that `granted` doesn't already allow. Empty means an upgrade to
/// `wanted` needs no new approval. Against the empty set it describes every capability.
pub fn expansion(wanted: &PluginCapabilities, granted: &PluginCapabilities) -> Vec<String> {
    let mut out = Vec::new();
    let granted_net: Vec<NetworkGrant> = granted
        .network
        .iter()
        .filter_map(|g| NetworkGrant::parse(g).ok())
        .collect();
    for g in &wanted.network {
        match NetworkGrant::parse(g) {
            Ok(w) if granted_net.iter().any(|h| w.covered_by(h)) => {}
            Ok(w) => out.push(format!("connect to {w}")),
            // Invalid grants never pass validation; describe them verbatim regardless.
            Err(_) => out.push(format!("connect to {g}")),
        }
    }
    for path in &wanted.network_from_settings {
        if !granted.network_from_settings.contains(path) {
            out.push(format!(
                "connect to the hosts you enter in its `{path}` setting"
            ));
        }
    }
    let mut scopes = wanted.api.clone();
    scopes.sort_unstable();
    scopes.dedup();
    for s in scopes {
        if !granted.api.contains(&s) {
            out.push(describe_scope(s).to_owned());
        }
    }
    for r in &wanted.routes {
        for m in &r.methods {
            let covered = granted
                .routes
                .iter()
                .any(|g| g.path == r.path && g.auth == r.auth && g.methods.contains(m));
            if !covered {
                out.push(describe_route(m, &r.path, r.auth));
            }
        }
    }
    if wanted.kv && !granted.kv {
        out.push("store its own data".to_owned());
    }
    if wanted.schedule && !granted.schedule {
        out.push("run jobs on a schedule".to_owned());
    }
    let mut roles = wanted.roles.clone();
    roles.sort_unstable();
    roles.dedup();
    for r in roles {
        if !granted.roles.contains(&r) {
            out.push(describe_role(r).to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use apitorrent_types::plugins::PluginRoute;

    #[test]
    fn upgrade_within_grants_needs_nothing() {
        let granted = PluginCapabilities {
            network: vec!["*.example.com".into(), "http://feeds.net".into()],
            api: vec![PluginApiScope::TorrentsAdd, PluginApiScope::TorrentsRead],
            routes: vec![PluginRoute {
                path: "/status".into(),
                methods: vec!["GET".into(), "POST".into()],
                auth: PluginRouteAuth::Api,
            }],
            kv: true,
            schedule: true,
            roles: vec![PluginRole::Source],
            ..PluginCapabilities::default()
        };
        let narrower = PluginCapabilities {
            network: vec!["a.example.com".into(), "feeds.net".into()],
            api: vec![PluginApiScope::TorrentsAdd],
            routes: vec![PluginRoute {
                path: "/status".into(),
                methods: vec!["GET".into()],
                auth: PluginRouteAuth::Api,
            }],
            kv: true,
            schedule: false,
            roles: vec![PluginRole::Source],
            ..PluginCapabilities::default()
        };
        assert!(expansion(&narrower, &granted).is_empty());
        assert!(expansion(&granted, &granted).is_empty());
    }

    #[test]
    fn a_plugin_authenticated_route_is_not_covered_by_an_ordinary_one() {
        let route = |auth| PluginRoute {
            path: "/api/v2/*".into(),
            methods: vec!["POST".into()],
            auth,
        };
        let granted = PluginCapabilities {
            routes: vec![route(PluginRouteAuth::Api)],
            ..PluginCapabilities::default()
        };
        let wanted = PluginCapabilities {
            routes: vec![route(PluginRouteAuth::Plugin)],
            ..PluginCapabilities::default()
        };
        assert_eq!(expansion(&wanted, &granted).len(), 1);
    }

    #[test]
    fn every_kind_of_expansion_is_listed() {
        let granted = PluginCapabilities {
            network: vec!["example.com".into()],
            ..PluginCapabilities::default()
        };
        let wanted = PluginCapabilities {
            network: vec!["example.com".into(), "http://example.com".into()],
            api: vec![PluginApiScope::TorrentsRemove, PluginApiScope::TokensVerify],
            routes: vec![
                PluginRoute {
                    path: "/x".into(),
                    methods: vec!["GET".into()],
                    auth: PluginRouteAuth::Api,
                },
                PluginRoute {
                    path: "/api/v2/*".into(),
                    methods: vec!["POST".into()],
                    auth: PluginRouteAuth::Plugin,
                },
            ],
            kv: true,
            schedule: true,
            roles: vec![PluginRole::DnsProvider],
            network_from_settings: vec!["feeds[].url".into()],
        };
        let e = expansion(&wanted, &granted);
        assert_eq!(
            e,
            [
                "connect to http://example.com",
                "connect to the hosts you enter in its `feeds[].url` setting",
                "remove torrents, and delete their data",
                "check the apitorrent API tokens its clients sign in with, and learn whose they are",
                "serve GET /x under its API path",
                "serve POST /api/v2/* under its API path to clients that haven't signed in to \
                 apitorrent (it checks their credentials itself, and may set cookies there)",
                "store its own data",
                "run jobs on a schedule",
                "publish DNS records for certificates (DNS provider)",
            ]
        );
    }
}

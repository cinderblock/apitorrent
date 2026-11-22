//! Instance settings, and the pure decisions built on them: which group a torrent belongs
//! to, and which torrents the queue lets run.

use std::collections::HashSet;
use std::sync::Arc;

use apitorrent_engine::SessionConfig;
use apitorrent_engine::identity::{
    ClientProfile, HostPattern, IdentityError, IdentityRule, IdentityRules,
};
use apitorrent_engine::limits::GroupLimits;
use apitorrent_types::{
    ClientIdentity, GroupSettings, IdentitySettings, QueueSettings, Settings, TrackerIdentityRule,
};

/// The settings a fresh instance starts with, taken from the session configuration (the
/// daemon's command line). Stored settings replace them after the first change.
pub fn defaults(session: &SessionConfig) -> Settings {
    Settings {
        download_rate: session.download_rate,
        upload_rate: session.upload_rate,
        max_connections: clamp_u32(session.max_connections),
        max_half_open: clamp_u32(session.max_half_open),
        connect_rate: u32::try_from(session.connect_rate).unwrap_or(u32::MAX),
        queue: QueueSettings::default(),
        groups: Vec::new(),
        naming: apitorrent_types::NamingPolicy::Portable,
        library: apitorrent_types::LibrarySettings::default(),
        encryption: encryption_setting(session.encryption),
        seed_limits: apitorrent_types::SeedLimits::default(),
        peer_port: session.listen.port(),
        dht_port: session
            .dht
            .as_ref()
            .and_then(|d| d.bind_v4.or(d.bind_v6))
            .map_or(0, |a| a.port()),
        identity: IdentitySettings::default(),
        file_order: apitorrent_types::files::FileOrder::default(),
        move_completed: apitorrent_types::files::MoveCompleted::default(),
    }
}

/// The engine's profile for an identity; `None` is apitorrent itself.
pub fn client_profile(id: &ClientIdentity) -> Result<Option<Arc<ClientProfile>>, IdentityError> {
    let profile = match id {
        ClientIdentity::Apitorrent => return Ok(None),
        ClientIdentity::Deluge {
            version,
            libtorrent,
        } => ClientProfile::deluge(version, libtorrent)?,
        ClientIdentity::QBittorrent { version } => ClientProfile::qbittorrent(version)?,
        ClientIdentity::Transmission { version } => ClientProfile::transmission(version)?,
        ClientIdentity::Custom {
            peer_id_prefix,
            user_agent,
            client_name,
        } => ClientProfile::custom(peer_id_prefix, user_agent, client_name)?,
    };
    Ok(Some(Arc::new(profile)))
}

/// The engine's rules for the identity settings. Fails on a bad version, prefix or host,
/// or two rules for the same host.
pub fn identity_rules(s: &IdentitySettings) -> Result<IdentityRules, String> {
    let default = client_profile(&s.default).map_err(|e| format!("default identity: {e}"))?;
    let mut rules: Vec<IdentityRule> = Vec::with_capacity(s.rules.len());
    for r in &s.rules {
        let host = HostPattern::parse(&r.host).map_err(|e| e.to_string())?;
        if rules.iter().any(|x| x.host == host) {
            return Err(format!("two identity rules are for {host}"));
        }
        let profile = client_profile(&r.identity).map_err(|e| format!("{host}: {e}"))?;
        rules.push(IdentityRule { host, profile });
    }
    Ok(IdentityRules { default, rules })
}

/// Add a rule giving `identity` to each of `hosts` that no rule covers yet (an exact rule
/// for it, or a `*.domain` rule that matches it). Returns the hosts that got one, in order.
/// Bad host names are skipped.
pub fn ensure_tracker_identities(
    s: &mut IdentitySettings,
    hosts: &[String],
    identity: &ClientIdentity,
) -> Vec<String> {
    let mut added = Vec::new();
    for host in hosts {
        let Ok(pattern) = HostPattern::parse(host) else {
            continue;
        };
        let name = pattern.to_string();
        let covered = s
            .rules
            .iter()
            .any(|r| HostPattern::parse(&r.host).is_ok_and(|p| p == pattern || p.matches(&name)));
        if !covered {
            s.rules.push(TrackerIdentityRule {
                host: name.clone(),
                identity: identity.clone(),
            });
            added.push(name);
        }
    }
    added
}

/// The engine's policy for an encryption setting.
pub fn encryption_policy(e: apitorrent_types::EncryptionPolicy) -> apitorrent_mse::Policy {
    use apitorrent_mse::{Policy, Preference};
    use apitorrent_types::EncryptionPolicy as E;
    match e {
        E::Disabled => Policy::Disabled,
        E::Enabled => Policy::Enabled(Preference::Rc4),
        E::PreferPlaintext => Policy::Enabled(Preference::Plaintext),
        E::Required => Policy::Required,
    }
}

/// The setting for an engine policy.
pub fn encryption_setting(p: apitorrent_mse::Policy) -> apitorrent_types::EncryptionPolicy {
    use apitorrent_mse::{Policy, Preference};
    use apitorrent_types::EncryptionPolicy as E;
    match p {
        Policy::Disabled => E::Disabled,
        Policy::Enabled(Preference::Rc4) => E::Enabled,
        Policy::Enabled(Preference::Plaintext) => E::PreferPlaintext,
        Policy::Required => E::Required,
    }
}

fn clamp_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

/// Reject settings that can't be applied.
pub fn validate(s: &Settings) -> Result<(), String> {
    let ratio_ok = |l: &apitorrent_types::SeedLimits| l.ratio.is_finite() && l.ratio >= 0.0;
    if !ratio_ok(&s.seed_limits)
        || s.groups
            .iter()
            .filter_map(|g| g.seed_limits.as_ref())
            .any(|l| !ratio_ok(l))
    {
        return Err("a seed ratio must be a number, 0 or more".into());
    }
    crate::files::validate_move(&s.move_completed)?;
    for g in &s.groups {
        if let Some(m) = &g.move_completed {
            crate::files::validate_move(m).map_err(|e| format!("group {}: {e}", g.name))?;
        }
    }
    if s.peer_port != 0 && s.peer_port == s.dht_port {
        return Err("the peer port and the DHT port must differ".into());
    }
    identity_rules(&s.identity)?;
    let mut names = HashSet::new();
    let mut labels = HashSet::new();
    for g in &s.groups {
        let name = g.name.trim();
        if name.is_empty() {
            return Err("a group needs a name".into());
        }
        if !names.insert(name.to_owned()) {
            return Err(format!("two groups are called {name:?}"));
        }
        for l in &g.labels {
            if !labels.insert(l.clone()) {
                return Err(format!("label {l:?} is in more than one group"));
            }
        }
    }
    Ok(())
}

pub fn group_limits(g: &GroupSettings) -> GroupLimits {
    GroupLimits {
        download_rate: g.download_rate,
        upload_rate: g.upload_rate,
        max_connections: g.max_connections as usize,
    }
}

/// The group a torrent with `label` belongs to.
pub fn group_for<'a>(settings: &'a Settings, label: Option<&str>) -> Option<&'a str> {
    let label = label?;
    settings
        .groups
        .iter()
        .find(|g| g.labels.iter().any(|l| l == label))
        .map(|g| g.name.as_str())
}

/// One torrent as the queue sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueItem {
    pub id: String,
    pub position: u32,
    /// Every wanted piece is downloaded.
    pub finished: bool,
    /// Running, but below the slow rate for long enough (and the queue ignores slow
    /// torrents).
    pub slow: bool,
}

/// Which of `items` may run, in queue order: the first downloads up to the download limit,
/// the first finished torrents up to the seed limit, and no more than the total limit. 0
/// means unlimited. `items` are the torrents not paused by the user and not in error.
///
/// A slow torrent (see [`QueueItem::slow`]) skips the download and seed limits and takes
/// no slot there, but still counts toward the total, as in libtorrent. Slow torrents are
/// placed first, so a fast torrent can't push an already running slow one off the total,
/// only for it to restart, lose its slowness and come back, over and over.
pub fn plan(items: &[QueueItem], q: &QueueSettings) -> HashSet<String> {
    let mut order: Vec<&QueueItem> = items.iter().collect();
    order.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
    let under = |count: u32, limit: u32| limit == 0 || count < limit;
    let (mut downloads, mut seeds, mut total) = (0, 0, 0);
    let mut active = HashSet::new();
    for item in order.iter().filter(|i| i.slow) {
        if under(total, q.max_active) {
            total += 1;
            active.insert(item.id.clone());
        }
    }
    for item in order.iter().filter(|i| !i.slow) {
        let fits = if item.finished {
            under(seeds, q.max_active_seeds)
        } else {
            under(downloads, q.max_active_downloads)
        } && under(total, q.max_active);
        if fits {
            if item.finished {
                seeds += 1;
            } else {
                downloads += 1;
            }
            total += 1;
            active.insert(item.id.clone());
        }
    }
    active
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, position: u32, finished: bool) -> QueueItem {
        QueueItem {
            id: id.into(),
            position,
            finished,
            slow: false,
        }
    }

    fn sorted(active: &HashSet<String>) -> Vec<&str> {
        let mut got: Vec<&str> = active.iter().map(String::as_str).collect();
        got.sort_unstable();
        got
    }

    #[test]
    fn slow_torrents_free_their_slots() {
        let slow = |id: &str, position: u32, finished: bool| QueueItem {
            slow: true,
            ..item(id, position, finished)
        };
        let items = [
            slow("d0", 0, false),
            item("d1", 1, false),
            item("d2", 2, false),
            slow("s3", 3, true),
            item("s4", 4, true),
        ];
        let q = QueueSettings {
            max_active_downloads: 1,
            max_active_seeds: 1,
            ..QueueSettings::default()
        };
        // d0 and s3 are slow, so d1 and s4 get the slots.
        assert_eq!(sorted(&plan(&items, &q)), ["d0", "d1", "s3", "s4"]);

        // The total limit still binds slow torrents, and they keep their place in it.
        let q = QueueSettings { max_active: 3, ..q };
        assert_eq!(sorted(&plan(&items, &q)), ["d0", "d1", "s3"]);
    }

    #[test]
    fn unlimited_runs_everything() {
        let items = [item("a", 0, false), item("b", 1, true)];
        assert_eq!(plan(&items, &QueueSettings::default()).len(), 2);
    }

    #[test]
    fn limits_apply_in_queue_order() {
        let items = [
            item("d3", 3, false),
            item("d1", 1, false),
            item("s0", 0, true),
            item("d2", 2, false),
            item("s4", 4, true),
            item("s5", 5, true),
        ];
        let q = QueueSettings {
            max_active_downloads: 2,
            max_active_seeds: 1,
            ..QueueSettings::default()
        };
        let active = plan(&items, &q);
        let mut got: Vec<&str> = active.iter().map(String::as_str).collect();
        got.sort_unstable();
        assert_eq!(got, ["d1", "d2", "s0"]);

        let q = QueueSettings {
            max_active: 2,
            ..QueueSettings::default()
        };
        let active = plan(&items, &q);
        let mut got: Vec<&str> = active.iter().map(String::as_str).collect();
        got.sort_unstable();
        assert_eq!(got, ["d1", "s0"]);
    }

    #[test]
    fn identity_settings_become_engine_rules() {
        let mut s = defaults(&SessionConfig::default());
        assert_eq!(
            identity_rules(&s.identity).unwrap(),
            IdentityRules::default()
        );
        s.identity.rules.push(TrackerIdentityRule {
            host: "Tracker.Example.org".into(),
            identity: ClientIdentity::deluge_default(),
        });
        s.identity.rules.push(TrackerIdentityRule {
            host: "*.other.example".into(),
            identity: ClientIdentity::Transmission {
                version: "4.0.6".into(),
            },
        });
        assert!(validate(&s).is_ok());
        let rules = identity_rules(&s.identity).unwrap();
        let p = rules
            .resolve(["https://tracker.example.org/pk/announce"])
            .unwrap();
        assert_eq!(p.peer_id_prefix, b"-DE211s-");
        assert_eq!(p.user_agent, "Deluge/2.1.1 libtorrent/2.0.10.0");
        let t = rules.resolve(["udp://a.other.example:6969"]).unwrap();
        assert_eq!(t.peer_id_prefix, b"-TR4060-");
        assert!(rules.resolve(["udp://open.example:1"]).is_none());

        // Bad rules are refused with a reason.
        let mut bad = s.clone();
        bad.identity.rules.push(TrackerIdentityRule {
            host: "tracker.example.org.".into(),
            identity: ClientIdentity::Apitorrent,
        });
        assert!(validate(&bad).unwrap_err().contains("two identity rules"));
        let mut bad = s.clone();
        bad.identity.default = ClientIdentity::QBittorrent {
            version: "four".into(),
        };
        assert!(validate(&bad).unwrap_err().contains("default identity"));
        let mut bad = s.clone();
        bad.identity.rules[0].host = "http://x/".into();
        assert!(validate(&bad).is_err());
    }

    #[test]
    fn ensuring_rules_leaves_covered_hosts_alone() {
        let mut s = IdentitySettings::default();
        s.rules.push(TrackerIdentityRule {
            host: "*.covered.example".into(),
            identity: ClientIdentity::Apitorrent,
        });
        let hosts = [
            "T.Example.org".to_owned(),
            "t.example.org".to_owned(),
            "x.covered.example".to_owned(),
            "not a host".to_owned(),
            "u.example.org".to_owned(),
        ];
        let deluge = ClientIdentity::deluge_default();
        let added = ensure_tracker_identities(&mut s, &hosts, &deluge);
        assert_eq!(added, ["t.example.org", "u.example.org"]);
        assert_eq!(s.rules.len(), 3);
        assert_eq!(s.rules[1].identity, deluge);
        // A second import adds nothing.
        assert!(ensure_tracker_identities(&mut s, &hosts, &deluge).is_empty());
    }

    #[test]
    fn settings_round_trip_through_json() {
        let mut s = defaults(&SessionConfig::default());
        s.identity.default = ClientIdentity::QBittorrent {
            version: "4.6.5".into(),
        };
        s.identity.rules.push(TrackerIdentityRule {
            host: "t.example".into(),
            identity: ClientIdentity::Custom {
                peer_id_prefix: "-XX1000-".into(),
                user_agent: "X/1.0".into(),
                client_name: "X 1.0".into(),
            },
        });
        let json = serde_json::to_string(&s).unwrap();
        assert!(
            json.contains(r#""default":{"client":"qBittorrent","version":"4.6.5"}"#),
            "{json}"
        );
        assert!(json.contains(r#""peerIdPrefix":"-XX1000-""#), "{json}");
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);

        // Settings stored before identities existed still load, as apitorrent.
        let mut old: serde_json::Value =
            serde_json::to_value(defaults(&SessionConfig::default())).unwrap();
        old.as_object_mut().unwrap().remove("identity");
        let loaded: Settings = serde_json::from_value(old).unwrap();
        assert_eq!(loaded.identity, IdentitySettings::default());
    }

    #[test]
    fn groups_are_found_by_label_and_validated() {
        let mut s = defaults(&SessionConfig::default());
        s.groups.push(GroupSettings {
            name: "linux".into(),
            labels: vec!["iso".into(), "distro".into()],
            download_rate: 0,
            upload_rate: 0,
            max_connections: 0,
            seed_limits: None,
            file_order: None,
            move_completed: None,
        });
        assert_eq!(group_for(&s, Some("iso")), Some("linux"));
        assert_eq!(group_for(&s, Some("other")), None);
        assert_eq!(group_for(&s, None), None);
        assert!(validate(&s).is_ok());
        s.groups.push(GroupSettings {
            name: "dupe".into(),
            labels: vec!["iso".into()],
            download_rate: 0,
            upload_rate: 0,
            max_connections: 0,
            seed_limits: None,
            file_order: None,
            move_completed: None,
        });
        assert!(validate(&s).is_err());
    }
}

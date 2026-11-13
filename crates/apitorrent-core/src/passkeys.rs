//! A space's tracker passkeys, listed and replaced in bulk.
//!
//! The finding and replacing is `apitorrent_tracker::passkey`'s; this groups the space's
//! announce URLs by passkey and works out each torrent's new announce list. A passkey is
//! named by [`id`] (a hash), so the API never sends one back.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write;

use apitorrent_tracker::passkey;
use apitorrent_types::TrackerPasskey;
use sha2::{Digest, Sha256};

/// The name of a passkey in the API: the first 16 hex digits of its SHA-256.
#[must_use]
pub fn id(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    digest[..8].iter().fold(String::with_capacity(16), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// The passkeys in `torrents`' announce lists (one list of tiers per torrent), with the
/// hosts each is used with and how many torrents use it, ordered by host.
#[must_use]
pub fn list<'a>(torrents: impl IntoIterator<Item = &'a [Vec<String>]>) -> Vec<TrackerPasskey> {
    struct Group {
        masked: String,
        hosts: BTreeSet<String>,
        torrents: u32,
    }
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for tiers in torrents {
        let mut here = HashSet::new();
        for url in tiers.iter().flatten() {
            let Some(found) = passkey::find(url) else {
                continue;
            };
            let id = id(found.key);
            let g = groups.entry(id.clone()).or_insert_with(|| Group {
                masked: passkey::mask(found.key),
                hosts: BTreeSet::new(),
                torrents: 0,
            });
            g.hosts.insert(found.host);
            if here.insert(id) {
                g.torrents += 1;
            }
        }
    }
    let mut out: Vec<TrackerPasskey> = groups
        .into_iter()
        .map(|(id, g)| TrackerPasskey {
            id,
            masked: g.masked,
            hosts: g.hosts.into_iter().collect(),
            torrents: g.torrents,
        })
        .collect();
    out.sort_by(|a, b| a.hosts.cmp(&b.hosts).then_with(|| a.id.cmp(&b.id)));
    out
}

/// The passkey named `id` in `tiers`, if any URL there has it.
#[must_use]
pub fn key_of<'a>(tiers: &'a [Vec<String>], wanted: &str) -> Option<&'a str> {
    tiers
        .iter()
        .flatten()
        .filter_map(|u| passkey::find(u))
        .find(|f| id(f.key) == wanted)
        .map(|f| f.key)
}

/// Why `new` can't replace the passkey in `tiers` (it isn't shaped like one where it
/// goes), or `None` when it can.
#[must_use]
pub fn unacceptable(tiers: &[Vec<String>], old: &str, new: &str) -> Option<&'static str> {
    if new == old {
        return Some("that is the passkey they already have");
    }
    let bad = tiers
        .iter()
        .flatten()
        .filter(|u| passkey::find(u).is_some_and(|f| f.key == old))
        .any(|u| !passkey::acceptable(u, new));
    bad.then_some(
        "that doesn't look like a passkey: use the letters and digits the tracker gave you, \
         at least 8 (16 where the passkey is part of the path)",
    )
}

/// `tiers` with the passkey `old` changed to `new`. A URL that becomes one already in the
/// list is dropped, so the list stays free of repeats.
#[must_use]
pub fn replaced(tiers: &[Vec<String>], old: &str, new: &str) -> Vec<Vec<String>> {
    let mut seen = HashSet::new();
    tiers
        .iter()
        .map(|tier| {
            tier.iter()
                .map(|u| passkey::replace(u, old, new).unwrap_or_else(|| u.clone()))
                .filter(|u| seen.insert(u.clone()))
                .collect::<Vec<_>>()
        })
        .filter(|t| !t.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "0123456789abcdef0123456789abcdef";
    const NEW: &str = "fedcba9876543210fedcba9876543210";

    fn tiers(urls: &[&str]) -> Vec<Vec<String>> {
        urls.iter().map(|u| vec![(*u).to_owned()]).collect()
    }

    #[test]
    fn groups_by_passkey_across_hosts() {
        let a = tiers(&[
            &format!("https://t1.example.org/{OLD}/announce"),
            &format!("https://t2.example.org/{OLD}/announce"),
        ]);
        let b = tiers(&[&format!("https://t1.example.org/{OLD}/announce")]);
        let c = tiers(&["http://other.org/announce.php?passkey=abcdefgh1234"]);
        let public = tiers(&["udp://open.example.org:1337/announce"]);
        let got = list([&a[..], &b[..], &c[..], &public[..]]);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].hosts, ["other.org"]);
        assert_eq!(got[1].hosts, ["t1.example.org", "t2.example.org"]);
        assert_eq!(got[1].torrents, 2);
        assert_eq!(got[1].masked, "0123…cdef");
        assert_eq!(got[1].id, id(OLD));
        assert!(!got[1].id.contains(OLD));
        assert_eq!(key_of(&a, &id(OLD)), Some(OLD));
        assert_eq!(key_of(&c, &id(OLD)), None);
    }

    #[test]
    fn replaces_and_drops_repeats() {
        let t = vec![
            vec![
                format!("https://t.example.org/{OLD}/announce"),
                format!("https://t.example.org/{NEW}/announce"),
            ],
            vec!["udp://open.example.org:1337/announce".to_owned()],
        ];
        assert_eq!(
            replaced(&t, OLD, NEW),
            vec![
                vec![format!("https://t.example.org/{NEW}/announce")],
                vec!["udp://open.example.org:1337/announce".to_owned()],
            ]
        );
        assert_eq!(unacceptable(&t, OLD, NEW), None);
        assert!(unacceptable(&t, OLD, OLD).is_some());
        assert!(unacceptable(&t, OLD, "short").is_some());
        assert!(unacceptable(&t, OLD, "has/slash/0123456789").is_some());
    }
}

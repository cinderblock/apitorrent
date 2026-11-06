//! Magnet links (BEP 9, with BEP 52 `btmh` hashes and BEP 53 file selection).

use std::fmt::Write as _;
use std::ops::RangeInclusive;

use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};

use crate::hash::{InfoHashes, Sha1Hash, Sha256Hash};

/// A parsed magnet link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Magnet {
    pub info_hashes: InfoHashes,
    /// `dn`: a suggested name, for display until the metadata arrives.
    pub display_name: Option<String>,
    /// `tr`: trackers, in the order given. Magnet links have no tiers.
    pub trackers: Vec<String>,
    /// `ws`: web seeds.
    pub web_seeds: Vec<String>,
    /// `x.pe`: peers to try, as `host:port`.
    pub peers: Vec<String>,
    /// `so` (BEP 53): the file indices to download. `None` means all of them.
    pub select_only: Option<Vec<RangeInclusive<usize>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MagnetError {
    #[error("not a magnet link")]
    NotMagnet,
    #[error("no BitTorrent info-hash (`xt=urn:btih:` or `xt=urn:btmh:`)")]
    NoInfoHash,
    #[error("malformed info-hash `{0}`")]
    BadInfoHash(String),
    #[error("conflicting info-hashes")]
    ConflictingHashes,
    #[error("malformed file selection `{0}`")]
    BadSelection(String),
}

/// Characters that must be escaped in a magnet parameter value.
const VALUE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'&')
    .add(b'+')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?');

impl Magnet {
    pub fn new(info_hashes: InfoHashes) -> Self {
        Self {
            info_hashes,
            display_name: None,
            trackers: Vec::new(),
            web_seeds: Vec::new(),
            peers: Vec::new(),
            select_only: None,
        }
    }

    pub fn parse(uri: &str) -> Result<Self, MagnetError> {
        let query = uri
            .trim()
            .strip_prefix("magnet:?")
            .or_else(|| uri.trim().strip_prefix("MAGNET:?"))
            .ok_or(MagnetError::NotMagnet)?;

        let mut v1: Option<Sha1Hash> = None;
        let mut v2: Option<Sha256Hash> = None;
        let mut out = Magnet::new(InfoHashes::v1(Sha1Hash([0; 20])));

        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            // Parameters may be numbered (`tr.1`, `xt.2`) to allow repeats.
            let key = key.split_once('.').map_or(key, |(k, rest)| {
                if rest.chars().all(|c| c.is_ascii_digit()) {
                    k
                } else {
                    key
                }
            });
            let value = decode(value);
            match key {
                "xt" => {
                    if let Some(h) = value.strip_prefix("urn:btih:") {
                        let hash = parse_btih(h)?;
                        if v1.is_some_and(|old| old != hash) {
                            return Err(MagnetError::ConflictingHashes);
                        }
                        v1 = Some(hash);
                    } else if let Some(h) = value.strip_prefix("urn:btmh:") {
                        let hash = parse_btmh(h)?;
                        if v2.is_some_and(|old| old != hash) {
                            return Err(MagnetError::ConflictingHashes);
                        }
                        v2 = Some(hash);
                    }
                    // Other URNs (ed2k, sha1, …) are for other networks and are ignored.
                }
                "dn" => out.display_name = Some(value.replace('+', " ")),
                "tr" => push_unique(&mut out.trackers, &value),
                "ws" => push_unique(&mut out.web_seeds, &value),
                "x.pe" => push_unique(&mut out.peers, &value),
                "so" => out.select_only = Some(parse_selection(&value)?),
                _ => {}
            }
        }
        out.info_hashes = InfoHashes::new(v1, v2).ok_or(MagnetError::NoInfoHash)?;
        Ok(out)
    }

    /// Whether file `index` is selected (BEP 53).
    pub fn is_selected(&self, index: usize) -> bool {
        self.select_only
            .as_ref()
            .is_none_or(|ranges| ranges.iter().any(|r| r.contains(&index)))
    }

    pub fn to_uri(&self) -> String {
        let mut s = String::from("magnet:?");
        let mut first = true;
        let mut param = |s: &mut String, key: &str, value: &str| {
            if !first {
                s.push('&');
            }
            first = false;
            let _ = write!(s, "{key}={}", utf8_percent_encode(value, VALUE));
        };
        if let Some(v1) = self.info_hashes.get_v1() {
            param(&mut s, "xt", &format!("urn:btih:{v1}"));
        }
        if let Some(v2) = self.info_hashes.get_v2() {
            param(&mut s, "xt", &format!("urn:btmh:1220{v2}"));
        }
        if let Some(dn) = &self.display_name {
            param(&mut s, "dn", dn);
        }
        for tr in &self.trackers {
            param(&mut s, "tr", tr);
        }
        for ws in &self.web_seeds {
            param(&mut s, "ws", ws);
        }
        for pe in &self.peers {
            param(&mut s, "x.pe", pe);
        }
        if let Some(ranges) = &self.select_only {
            let text: Vec<String> = ranges
                .iter()
                .map(|r| {
                    if r.start() == r.end() {
                        r.start().to_string()
                    } else {
                        format!("{}-{}", r.start(), r.end())
                    }
                })
                .collect();
            param(&mut s, "so", &text.join(","));
        }
        s
    }
}

fn decode(value: &str) -> String {
    percent_decode_str(value).decode_utf8_lossy().into_owned()
}

fn push_unique(list: &mut Vec<String>, value: &str) {
    let value = value.trim().to_owned();
    if !value.is_empty() && !list.contains(&value) {
        list.push(value);
    }
}

fn parse_btih(h: &str) -> Result<Sha1Hash, MagnetError> {
    let bad = || MagnetError::BadInfoHash(h.to_owned());
    match h.len() {
        40 => h.parse().map_err(|_| bad()),
        32 => {
            let bytes = data_encoding::BASE32
                .decode(h.to_ascii_uppercase().as_bytes())
                .map_err(|_| bad())?;
            Sha1Hash::from_slice(&bytes).ok_or_else(bad)
        }
        _ => Err(bad()),
    }
}

fn parse_btmh(h: &str) -> Result<Sha256Hash, MagnetError> {
    // A multihash: 0x12 (SHA-256), 0x20 (32 bytes), then the digest, all in hex.
    let digest = h
        .strip_prefix("1220")
        .ok_or_else(|| MagnetError::BadInfoHash(h.to_owned()))?;
    digest
        .parse()
        .map_err(|_| MagnetError::BadInfoHash(h.to_owned()))
}

fn parse_selection(value: &str) -> Result<Vec<RangeInclusive<usize>>, MagnetError> {
    let bad = || MagnetError::BadSelection(value.to_owned());
    value
        .split(',')
        .filter(|p| !p.is_empty())
        .map(|part| {
            let (a, b) = part.split_once('-').unwrap_or((part, part));
            let a: usize = a.trim().parse().map_err(|_| bad())?;
            let b: usize = b.trim().parse().map_err(|_| bad())?;
            if a > b {
                return Err(bad());
            }
            Ok(a..=b)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hybrid_magnet() {
        let uri = "magnet:?xt=urn:btih:631a31dd0a46257d5078c0dee4e66e26f73e42ac\
                   &xt=urn:btmh:1220d8dd32ac93357c368556af3ac1d95c9d76bd0dff6fa9833ecdac3d53134efabb\
                   &dn=bittorrent-v1-v2-hybrid-test&tr=udp%3A%2F%2Ftracker.example%3A6969";
        let m = Magnet::parse(uri).unwrap();
        assert!(m.info_hashes.is_hybrid());
        assert_eq!(
            m.display_name.as_deref(),
            Some("bittorrent-v1-v2-hybrid-test")
        );
        assert_eq!(m.trackers, ["udp://tracker.example:6969"]);
        assert_eq!(Magnet::parse(&m.to_uri()).unwrap(), m);
    }

    #[test]
    fn base32_hash() {
        let m = Magnet::parse("magnet:?xt=urn:btih:MMNDDXIKIYSX2UDYYDPOJZTOE33T4QVM").unwrap();
        assert_eq!(
            m.info_hashes.get_v1().unwrap().to_string(),
            "631a31dd0a46257d5078c0dee4e66e26f73e42ac"
        );
    }

    #[test]
    fn select_only() {
        let m = Magnet::parse(
            "magnet:?xt=urn:btih:631a31dd0a46257d5078c0dee4e66e26f73e42ac&so=0,2,4-6",
        )
        .unwrap();
        assert!(m.is_selected(0));
        assert!(!m.is_selected(1));
        assert!(m.is_selected(5));
        assert!(!m.is_selected(7));
        assert!(
            Magnet::parse("magnet:?xt=urn:btih:631a31dd0a46257d5078c0dee4e66e26f73e42ac&so=3-1")
                .is_err()
        );
    }

    #[test]
    fn errors() {
        assert_eq!(
            Magnet::parse("http://x").unwrap_err(),
            MagnetError::NotMagnet
        );
        assert_eq!(
            Magnet::parse("magnet:?dn=x").unwrap_err(),
            MagnetError::NoInfoHash
        );
        assert!(matches!(
            Magnet::parse("magnet:?xt=urn:btih:abc").unwrap_err(),
            MagnetError::BadInfoHash(_)
        ));
    }
}

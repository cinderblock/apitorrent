//! Client identity: which client a torrent presents itself as.
//!
//! Some private trackers accept only clients on a list, recognised by the peer id prefix
//! and the HTTP `User-Agent` of announces. A [`ClientProfile`] holds everything that
//! names a client:
//!
//! - the peer id prefix, and how the rest of the 20 bytes is filled;
//! - the `User-Agent` of HTTP announces and scrapes;
//! - the client name (`v`) in the extension handshake (BEP 10);
//! - the shape of the HTTP announce ([`AnnounceStyle`]): its parameters, their order,
//!   `numwant`, and a per-torrent `key`.
//!
//! [`IdentityRules`] pick a profile per torrent: the first rule whose host pattern matches
//! the host of any of the torrent's trackers, or else the default. A torrent with no
//! profile is apitorrent itself and uses the session's own peer id and key, so nothing
//! changes for it (unless it switched back from another profile: then it gets its own). The
//! DHT always speaks as apitorrent.
//!
//! A switch looks to a tracker like one client quitting and another starting: `stopped`
//! under the old identity, answered before the new one's `started`, and nothing shared
//! between them (peer id, key, `trackerid`, UDP connection id, HTTP connection, stats).
//!
//! The formats come from the clients' sources: Deluge 2.1.1 (`deluge/core/core.py`,
//! `_create_peer_id` and the `user_agent` setting), libtorrent 2.0.10
//! (`generate_peer_id.cpp`, `string_util.cpp`, `http_tracker_connection.cpp`,
//! `bt_peer_connection.cpp`), qBittorrent 4.6.5 (`sessionimpl.cpp`) and Transmission
//! 4.0.6 (`session.cc` `tr_peerIdInit`, `announcer-http.cc`, `peer-msgs.cc`). All three
//! keep a peer id per torrent, as we do here.
//!
//! # What still tells a profile apart from the real client
//!
//! Trackers see the announce. The peer id, `User-Agent`, query parameters, their order and
//! escaping, `numwant` and `key` match. What doesn't:
//!
//! - HTTP headers: we send `accept: */*` and no `Accept-Encoding`. libtorrent sends
//!   `Accept-Encoding: gzip` and `Connection: close` and no `Accept`; Transmission's curl
//!   sends `Accept: */*` and every encoding it supports.
//! - libtorrent adds `ipv4=`/`ipv6=` for private torrents when it listens on a public
//!   address, and sends `event=paused` for a finished partial download. Transmission
//!   announces twice, once over IPv4 and once over IPv6. We do neither.
//! - TLS: the `ClientHello` of HTTPS announces is rustls's, not OpenSSL's or curl's.
//!
//! UDP trackers see only the peer id, key and numwant.
//!
//! Peers see more. A profile's [`WireStyle`] matches the client's reserved bits (DHT and
//! v2), its `reqq` (2000 for libtorrent, 512 for Transmission), and whether libtorrent's
//! extensions are offered (`upload_only`, `lt_donthave`, `ut_holepunch`); custom identities
//! set these at runtime. What doesn't match: the `m` ids, which keys the extension
//! handshake carries (Transmission sends `e` and always `upload_only`; libtorrent sends
//! `complete_ago`, and `p` only on outgoing connections), the holepunch error code
//! (always sent, as BEP 55 says; libtorrent sends it only with errors), uTP and encryption
//! behaviour, and request patterns. Trackers can't see any of that, and peers don't
//! enforce client lists, so it doesn't matter for what this is for.

use std::sync::Arc;

use apitorrent_tracker::AnnounceEvent;
pub use apitorrent_tracker::AnnounceStyle;
use apitorrent_wire::PeerId;

/// Why a profile or rule can't be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    #[error("{0:?} is not a version like 2.1.1")]
    BadVersion(String),
    #[error("version part {0} is too large for a peer id")]
    VersionPartTooLarge(u32),
    #[error("a peer id prefix must be 1 to 20 printable ASCII characters")]
    BadPrefix,
    #[error("{0} must be 1 to 200 printable ASCII characters")]
    BadText(&'static str),
    #[error("{0:?} is not a host name or *.domain pattern")]
    BadHost(String),
    #[error("a request queue of {0} is not between 1 and {max}", max = WireStyle::MAX_REQUEST_QUEUE)]
    BadRequestQueue(u32),
}

/// How the bytes after the peer id prefix are filled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PeerIdSuffix {
    /// Random bytes, any value (apitorrent's own).
    Bytes,
    /// Random characters from this alphabet.
    Alphabet(&'static [u8]),
    /// Transmission's: lower-case base 36, the last character a check that makes the
    /// characters' values add up to a multiple of 36.
    Transmission,
}

/// libtorrent's `url_random`: characters an announce URL needs no escaping for.
pub const LIBTORRENT_ALPHABET: &[u8] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz-_.!~*()";
/// Transmission's `tr_peerIdInit` alphabet: lower-case base 36.
pub const TRANSMISSION_ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// When the handshake's DHT bit is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DhtBit {
    /// Always, even with the DHT off (libtorrent, built with DHT support).
    Always,
    /// While our DHT runs (apitorrent).
    WhenRunning,
    /// While our DHT runs, and only for public torrents (Transmission's `allows_dht`).
    PublicWhenRunning,
}

/// How a client fills the parts of the peer handshakes that peers see beyond its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WireStyle {
    pub dht_bit: DhtBit,
    /// Set the v2 bit for hybrid torrents. A v2-only torrent always sets it: it can't be
    /// shared without.
    pub v2_bit: bool,
    /// `reqq`: how many requests a peer may have queued with us at once.
    pub request_queue: u32,
    /// Offer libtorrent's extensions (`upload_only`, `lt_donthave`, `ut_holepunch`).
    pub libtorrent_extensions: bool,
}

impl WireStyle {
    /// The largest `reqq` a profile may give.
    pub const MAX_REQUEST_QUEUE: u32 = 10_000;

    /// apitorrent's own.
    pub const APITORRENT: Self = Self {
        dht_bit: DhtBit::WhenRunning,
        v2_bit: true,
        request_queue: 2000,
        libtorrent_extensions: true,
    };

    /// libtorrent 2.0 (Deluge, qBittorrent): `write_handshake`, and
    /// `max_allowed_in_request_queue` for `reqq`.
    pub const LIBTORRENT: Self = Self {
        dht_bit: DhtBit::Always,
        v2_bit: true,
        request_queue: 2000,
        libtorrent_extensions: true,
    };

    /// Transmission 4: no v2, `reqq` 512, and none of libtorrent's extensions.
    pub const TRANSMISSION: Self = Self {
        dht_bit: DhtBit::PublicWhenRunning,
        v2_bit: false,
        request_queue: 512,
        libtorrent_extensions: false,
    };

    /// Checked: `reqq` 1 to [`Self::MAX_REQUEST_QUEUE`].
    pub fn checked(self) -> Result<Self, IdentityError> {
        if (1..=Self::MAX_REQUEST_QUEUE).contains(&self.request_queue) {
            Ok(self)
        } else {
            Err(IdentityError::BadRequestQueue(self.request_queue))
        }
    }

    /// Whether the DHT bit goes in a handshake for a torrent.
    pub fn dht(self, dht_running: bool, private: bool) -> bool {
        match self.dht_bit {
            DhtBit::Always => true,
            DhtBit::WhenRunning => dht_running,
            DhtBit::PublicWhenRunning => dht_running && !private,
        }
    }

    /// Whether the v2 bit goes in a handshake for a torrent with v2 hashes (`has_v2`), and
    /// v1 ones too (`has_v1`).
    pub fn v2(self, has_v2: bool, has_v1: bool) -> bool {
        has_v2 && (self.v2_bit || !has_v1)
    }
}

/// Everything that names a client to trackers and peers.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientProfile {
    /// The start of the peer id, at most 20 bytes: `-DE211s-`.
    pub peer_id_prefix: Vec<u8>,
    pub suffix: PeerIdSuffix,
    /// The `User-Agent` of HTTP announces and scrapes.
    pub user_agent: String,
    /// The `v` of the extension handshake.
    pub client_name: String,
    pub announce: AnnounceStyle,
    pub wire: WireStyle,
}

impl ClientProfile {
    /// Deluge on libtorrent. Deluge builds its peer id with libtorrent's
    /// `generate_fingerprint("DE", major, minor, patch, 0)` and then puts its release
    /// character in the seventh place: `s` for a stable release (`-DE211s-` for 2.1.1),
    /// `D` for a development build, or the first letter of a pre-release suffix. Its user
    /// agent is `Deluge/<version> libtorrent/<libtorrent version>`, and libtorrent sends
    /// the user agent as `v` too.
    pub fn deluge(version: &str, libtorrent: &str) -> Result<Self, IdentityError> {
        let (parts, suffix) = split_version(version)?;
        split_version(libtorrent)?;
        let mut prefix = azureus_prefix(*b"DE", &parts)?;
        let suffix = suffix.to_ascii_lowercase();
        prefix[6] = if suffix.contains("dev") {
            b'D'
        } else {
            suffix
                .trim_start_matches(['.', '-', '_'])
                .bytes()
                .next()
                .filter(|c| matches!(c, b'a' | b'b' | b'r'))
                .unwrap_or(b's')
        };
        let user_agent = format!("Deluge/{} libtorrent/{}", version.trim(), libtorrent.trim());
        Ok(Self {
            peer_id_prefix: prefix.to_vec(),
            suffix: PeerIdSuffix::Alphabet(LIBTORRENT_ALPHABET),
            client_name: user_agent.clone(),
            user_agent,
            announce: AnnounceStyle::Libtorrent,
            wire: WireStyle::LIBTORRENT,
        })
    }

    /// qBittorrent (on libtorrent): `-qB4650-` and `qBittorrent/4.6.5`, which is also its
    /// `v`.
    pub fn qbittorrent(version: &str) -> Result<Self, IdentityError> {
        let (parts, _) = split_version(version)?;
        let prefix = azureus_prefix(*b"qB", &parts)?;
        let user_agent = format!("qBittorrent/{}", version.trim());
        Ok(Self {
            peer_id_prefix: prefix.to_vec(),
            suffix: PeerIdSuffix::Alphabet(LIBTORRENT_ALPHABET),
            client_name: user_agent.clone(),
            user_agent,
            announce: AnnounceStyle::Libtorrent,
            wire: WireStyle::LIBTORRENT,
        })
    }

    /// Transmission: `-TR4060-` and `Transmission/4.0.6`; its `v` is
    /// `Transmission 4.0.6`. Its prefix is major, minor and patch in base 62, then `0`
    /// for a release, `B` for a beta, `Z` for a development build.
    pub fn transmission(version: &str) -> Result<Self, IdentityError> {
        let (parts, suffix) = split_version(version)?;
        let mut prefix = *b"-TR0000-";
        for (slot, &n) in prefix[3..6].iter_mut().zip(&parts) {
            *slot = base62_char(n)?;
        }
        let suffix = suffix.to_ascii_lowercase();
        prefix[6] = if suffix.contains("dev") {
            b'Z'
        } else if suffix.contains("beta") {
            b'B'
        } else {
            b'0'
        };
        Ok(Self {
            peer_id_prefix: prefix.to_vec(),
            suffix: PeerIdSuffix::Transmission,
            user_agent: format!("Transmission/{}", version.trim()),
            client_name: format!("Transmission {}", version.trim()),
            announce: AnnounceStyle::Transmission,
            wire: WireStyle::TRANSMISSION,
        })
    }

    /// Any client: the prefix is used as is, and the rest filled with libtorrent's
    /// alphabet. The announce looks like libtorrent's, as most clients use it.
    pub fn custom(
        peer_id_prefix: &str,
        user_agent: &str,
        client_name: &str,
        wire: WireStyle,
    ) -> Result<Self, IdentityError> {
        let prefix = peer_id_prefix.as_bytes();
        if prefix.is_empty() || prefix.len() > 20 || !prefix.iter().all(u8::is_ascii_graphic) {
            return Err(IdentityError::BadPrefix);
        }
        Ok(Self {
            peer_id_prefix: prefix.to_vec(),
            suffix: PeerIdSuffix::Alphabet(LIBTORRENT_ALPHABET),
            user_agent: checked_text(user_agent, "the user agent")?,
            client_name: checked_text(client_name, "the client name")?,
            announce: AnnounceStyle::Libtorrent,
            wire: wire.checked()?,
        })
    }

    /// A new peer id: the prefix, then random characters.
    pub fn peer_id(&self) -> PeerId {
        use rand::RngExt as _;
        let mut rng = rand::rng();
        let mut id = [0u8; 20];
        let n = self.peer_id_prefix.len().min(20);
        id[..n].copy_from_slice(&self.peer_id_prefix[..n]);
        let rest = &mut id[n..];
        match self.suffix {
            PeerIdSuffix::Bytes => rng.fill(rest),
            PeerIdSuffix::Alphabet(a) => {
                for b in rest {
                    *b = a[rng.random_range(0..a.len())];
                }
            }
            PeerIdSuffix::Transmission => {
                let pool = TRANSMISSION_ALPHABET;
                if let Some((check, random)) = rest.split_last_mut() {
                    let mut total = 0;
                    for b in random {
                        let v = rng.random_range(0..pool.len());
                        total += v;
                        *b = pool[v];
                    }
                    *check = pool[(pool.len() - total % pool.len()) % pool.len()];
                }
            }
        }
        PeerId(id)
    }
}

/// Text that goes into an HTTP header or a handshake: printable ASCII and spaces only, so
/// it can't break a header line.
fn checked_text(s: &str, what: &'static str) -> Result<String, IdentityError> {
    let s = s.trim();
    if s.is_empty() || s.len() > 200 || !s.bytes().all(|b| b == b' ' || b.is_ascii_graphic()) {
        return Err(IdentityError::BadText(what));
    }
    Ok(s.to_owned())
}

/// `2.1.1` → `([2, 1, 1], "")`; `2.1.2.dev0` → `([2, 1, 2], ".dev0")`;
/// `4.1.0-beta.2` → `([4, 1, 0], "-beta.2")`. One to four numeric parts, then an optional
/// suffix of printable characters.
fn split_version(version: &str) -> Result<(Vec<u32>, &str), IdentityError> {
    let bad = || IdentityError::BadVersion(version.to_owned());
    let v = version.trim();
    let end = v
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(v.len());
    let numeric = &v[..end];
    // `2.1.2.dev0`: the dot before the suffix belongs to it.
    let numeric = numeric.trim_end_matches('.');
    let suffix = &v[numeric.len()..];
    if suffix.len() > 40 || !suffix.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(bad());
    }
    let parts = numeric
        .split('.')
        .map(|p| p.parse::<u32>().map_err(|_| bad()))
        .collect::<Result<Vec<_>, _>>()?;
    if !(1..=4).contains(&parts.len()) {
        return Err(bad());
    }
    Ok((parts, suffix))
}

/// libtorrent's `generate_fingerprint`: `-XY` and four version characters (`0`–`9`, then
/// `A`–`Z` for 10–35), missing parts as `0`.
fn azureus_prefix(code: [u8; 2], parts: &[u32]) -> Result<[u8; 8], IdentityError> {
    let mut out = *b"-XY0000-";
    out[1..3].copy_from_slice(&code);
    for (slot, &n) in out[3..7].iter_mut().zip(parts) {
        *slot = match n {
            0..=9 => b'0' + u8::try_from(n).expect("small"),
            10..=35 => b'A' + u8::try_from(n - 10).expect("small"),
            _ => return Err(IdentityError::VersionPartTooLarge(n)),
        };
    }
    Ok(out)
}

/// Transmission's version characters: `0`–`9`, `A`–`Z`, `a`–`z`.
fn base62_char(n: u32) -> Result<u8, IdentityError> {
    const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    usize::try_from(n)
        .ok()
        .and_then(|i| BASE62.get(i).copied())
        .ok_or(IdentityError::VersionPartTooLarge(n))
}

/// A tracker host pattern: `tracker.example.org` matches that host only;
/// `*.example.org` matches `example.org` and every name under it. Case-insensitive; a
/// trailing dot is ignored.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HostPattern {
    host: String,
    wildcard: bool,
}

impl HostPattern {
    pub fn parse(pattern: &str) -> Result<Self, IdentityError> {
        let p = normalize_host(pattern);
        let (wildcard, host) = match p.strip_prefix("*.") {
            Some(rest) => (true, rest.to_owned()),
            None => (false, p),
        };
        let name = !host.is_empty()
            && host.len() <= 253
            && host.split('.').all(|label| {
                !label.is_empty()
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            });
        // An IP address is a host too (IPv6 in brackets, as URLs write it).
        let ip = !wildcard
            && host
                .strip_prefix('[')
                .and_then(|h| h.strip_suffix(']'))
                .unwrap_or(&host)
                .parse::<std::net::IpAddr>()
                .is_ok();
        if !name && !ip {
            return Err(IdentityError::BadHost(pattern.to_owned()));
        }
        Ok(Self { host, wildcard })
    }

    pub fn matches(&self, host: &str) -> bool {
        let host = normalize_host(host);
        if host == self.host {
            return true;
        }
        self.wildcard
            && host
                .strip_suffix(self.host.as_str())
                .is_some_and(|front| front.ends_with('.'))
    }

    /// Whether both patterns are the same once normalised.
    pub fn same_as(&self, other: &str) -> bool {
        Self::parse(other).is_ok_and(|o| o == *self)
    }
}

impl std::fmt::Display for HostPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.wildcard {
            write!(f, "*.{}", self.host)
        } else {
            f.write_str(&self.host)
        }
    }
}

/// Lower case, no surrounding space, no trailing dot.
pub fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// The host of a tracker URL, normalised, if it has one.
pub fn tracker_host(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url.trim()).ok()?;
    parsed.host_str().map(normalize_host)
}

/// One rule: torrents with a tracker on `host` present as `profile` (`None`: apitorrent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityRule {
    pub host: HostPattern,
    pub profile: Option<Arc<ClientProfile>>,
}

/// Which profile each torrent uses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdentityRules {
    /// For torrents no rule matches. `None` is apitorrent.
    pub default: Option<Arc<ClientProfile>>,
    /// In order; the first one that matches any of a torrent's trackers wins.
    pub rules: Vec<IdentityRule>,
}

impl IdentityRules {
    /// The profile for a torrent with these tracker URLs.
    pub fn resolve<'a>(
        &self,
        tracker_urls: impl IntoIterator<Item = &'a str>,
    ) -> Option<Arc<ClientProfile>> {
        let hosts: Vec<String> = tracker_urls.into_iter().filter_map(tracker_host).collect();
        self.rules
            .iter()
            .find(|r| hosts.iter().any(|h| r.host.matches(h)))
            .map_or_else(|| self.default.clone(), |r| r.profile.clone())
    }
}

/// The identity a torrent is using right now.
#[derive(Debug, Clone)]
pub(crate) struct ActiveIdentity {
    /// `None` is apitorrent itself.
    pub profile: Option<Arc<ClientProfile>>,
    pub peer_id: PeerId,
    /// The announce `key`. The clients we present as keep one per torrent; apitorrent
    /// keeps one for the session.
    pub key: u32,
}

impl ActiveIdentity {
    /// The identity `profile` gives: apitorrent's shared peer id and key, or fresh ones.
    pub(crate) fn new(
        profile: Option<Arc<ClientProfile>>,
        native: PeerId,
        native_key: u32,
    ) -> Self {
        let (peer_id, key) = match &profile {
            Some(p) => (p.peer_id(), rand::random()),
            None => (native, native_key),
        };
        Self {
            profile,
            peer_id,
            key,
        }
    }

    /// The identity for a torrent with these trackers under the session's rules, and the
    /// rules' generation.
    pub(crate) fn resolve<'a>(
        shared: &crate::session::Shared,
        tracker_urls: impl IntoIterator<Item = &'a str>,
    ) -> (Self, u64) {
        let (rules, generation) = shared.identity_rules();
        let profile = rules.resolve(tracker_urls);
        (
            Self::new(profile, shared.peer_id, shared.announce_key),
            generation,
        )
    }

    /// For a torrent switching back to apitorrent from another client: its own peer id and
    /// key, in place of the session's. The session's are the ones the trackers saw before
    /// the earlier switch, and a tracker that saw the other client stop must not see them
    /// start again. Other profiles always get fresh ones.
    pub(crate) fn renew_native(&mut self) {
        if self.profile.is_none() {
            // Keep `-ATxxxx-`; replace the random part, as the session made it.
            self.peer_id.0[8..].copy_from_slice(&rand::random::<[u8; 12]>());
            self.key = rand::random();
        }
    }

    /// The `v` of our extension handshake.
    pub(crate) fn client_name(&self, native: &str) -> String {
        self.profile
            .as_ref()
            .map_or_else(|| native.to_owned(), |p| p.client_name.clone())
    }

    pub(crate) fn user_agent(&self) -> Option<String> {
        self.profile.as_ref().map(|p| p.user_agent.clone())
    }

    /// What peers see of our handshakes besides the name.
    pub(crate) fn wire(&self) -> WireStyle {
        self.profile
            .as_ref()
            .map_or(WireStyle::APITORRENT, |p| p.wire)
    }

    pub(crate) fn announce_style(&self) -> AnnounceStyle {
        self.profile
            .as_ref()
            .map_or(AnnounceStyle::Apitorrent, |p| p.announce)
    }

    /// How many peers an announce asks for: what the client we present as asks for
    /// (libtorrent 200, Transmission 80, both 0 when stopping), or apitorrent's 50 (0 once
    /// finished).
    pub(crate) fn num_want(&self, event: AnnounceEvent, finished: bool) -> u32 {
        match self.announce_style() {
            AnnounceStyle::Apitorrent => {
                if finished {
                    0
                } else {
                    50
                }
            }
            _ if event == AnnounceEvent::Stopped => 0,
            AnnounceStyle::Libtorrent => 200,
            AnnounceStyle::Transmission => 80,
        }
    }

    /// A short name for logs: `apitorrent`, or the user agent.
    pub(crate) fn describe(&self) -> String {
        self.profile
            .as_ref()
            .map_or_else(|| "apitorrent".to_owned(), |p| p.user_agent.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefix(p: &ClientProfile) -> &str {
        std::str::from_utf8(&p.peer_id_prefix).unwrap()
    }

    #[test]
    fn deluge_matches_the_real_client() {
        let p = ClientProfile::deluge("2.1.1", "2.0.10.0").unwrap();
        assert_eq!(prefix(&p), "-DE211s-");
        assert_eq!(p.user_agent, "Deluge/2.1.1 libtorrent/2.0.10.0");
        assert_eq!(
            p.client_name, p.user_agent,
            "libtorrent's v is the user agent"
        );
        assert_eq!(p.announce, AnnounceStyle::Libtorrent);
        let pre = |v: &str| {
            ClientProfile::deluge(v, "2.0.10.0")
                .unwrap()
                .peer_id_prefix
                .clone()
        };
        assert_eq!(pre("2.0.3"), b"-DE203s-");
        assert_eq!(pre("2.1.2.dev0"), b"-DE212D-");
        assert_eq!(pre("2.0.0b2"), b"-DE200b-");
        assert_eq!(pre("2.0.0rc1"), b"-DE200r-");
        assert_eq!(pre("2.12.1"), b"-DE2C1s-");
        assert!(ClientProfile::deluge("two", "2.0.10.0").is_err());
        assert!(ClientProfile::deluge("2.1.1", "").is_err());
        assert!(ClientProfile::deluge("1.2.3.4.5", "2.0.10.0").is_err());
    }

    #[test]
    fn qbittorrent_and_transmission() {
        let q = ClientProfile::qbittorrent("4.6.5").unwrap();
        assert_eq!(prefix(&q), "-qB4650-");
        assert_eq!(q.user_agent, "qBittorrent/4.6.5");
        assert_eq!(q.client_name, "qBittorrent/4.6.5");
        assert_eq!(
            prefix(&ClientProfile::qbittorrent("4.6.10").unwrap()),
            "-qB46A0-"
        );
        assert_eq!(
            ClientProfile::qbittorrent("4.6.36").unwrap_err(),
            IdentityError::VersionPartTooLarge(36)
        );
        let t = ClientProfile::transmission("4.0.6").unwrap();
        assert_eq!(prefix(&t), "-TR4060-");
        assert_eq!(t.user_agent, "Transmission/4.0.6");
        assert_eq!(t.client_name, "Transmission 4.0.6");
        assert_eq!(t.announce, AnnounceStyle::Transmission);
        assert_eq!(
            prefix(&ClientProfile::transmission("4.1.0-beta.2").unwrap()),
            "-TR410B-"
        );
        assert_eq!(
            prefix(&ClientProfile::transmission("4.0.40").unwrap()),
            "-TR40e0-"
        );
        assert_eq!(
            prefix(&ClientProfile::transmission("4.1.0-dev").unwrap()),
            "-TR410Z-"
        );
    }

    #[test]
    fn peer_ids_use_the_clients_alphabet() {
        for p in [
            ClientProfile::deluge("2.1.1", "2.0.10.0").unwrap(),
            ClientProfile::qbittorrent("4.6.5").unwrap(),
        ] {
            let a = p.peer_id();
            assert_ne!(a, p.peer_id(), "random per call");
            assert_eq!(&a.0[..8], p.peer_id_prefix.as_slice());
            assert!(
                a.0[8..].iter().all(|c| LIBTORRENT_ALPHABET.contains(c)),
                "{a:?}"
            );
        }
        let t = ClientProfile::transmission("4.0.6").unwrap();
        for _ in 0..50 {
            let id = t.peer_id();
            assert_eq!(&id.0[..8], b"-TR4060-");
            let values: Vec<usize> = id.0[8..]
                .iter()
                .map(|c| TRANSMISSION_ALPHABET.iter().position(|a| a == c).unwrap())
                .collect();
            assert_eq!(values.iter().sum::<usize>() % 36, 0, "{id:?}");
        }
        let w = WireStyle::LIBTORRENT;
        let c = ClientProfile::custom("-XX1234-", "X/1.2", "X 1.2", w).unwrap();
        assert_eq!(&c.peer_id().0[..8], b"-XX1234-");
        assert!(ClientProfile::custom("", "X", "X", w).is_err());
        assert!(ClientProfile::custom("-XX1234-", "X\r\nEvil: 1", "X", w).is_err());
        assert!(ClientProfile::custom(&"x".repeat(21), "X", "X", w).is_err());
        for bad in [0, WireStyle::MAX_REQUEST_QUEUE + 1] {
            let wire = WireStyle {
                request_queue: bad,
                ..w
            };
            assert_eq!(
                ClientProfile::custom("-XX1234-", "X", "X", wire).unwrap_err(),
                IdentityError::BadRequestQueue(bad)
            );
        }
    }

    #[test]
    fn wire_styles_match_the_clients() {
        let (lt, tr, at) = (
            WireStyle::LIBTORRENT,
            WireStyle::TRANSMISSION,
            WireStyle::APITORRENT,
        );
        // DHT bit: (running, private)
        for (dht, private) in [(false, false), (true, false), (true, true), (false, true)] {
            assert!(lt.dht(dht, private), "libtorrent always sets it");
            assert_eq!(at.dht(dht, private), dht);
            assert_eq!(tr.dht(dht, private), dht && !private);
        }
        // v2 bit: hybrid, v2-only, v1-only
        assert!(lt.v2(true, true) && lt.v2(true, false) && !lt.v2(false, true));
        assert!(!tr.v2(true, true), "Transmission has no v2");
        assert!(tr.v2(true, false), "a v2-only torrent needs the bit");
        assert_eq!((lt.request_queue, tr.request_queue), (2000, 512));
        assert_eq!(
            ClientProfile::transmission("4.0.6").unwrap().wire,
            WireStyle::TRANSMISSION
        );
        assert_eq!(
            ClientProfile::deluge("2.1.1", "2.0.10.0").unwrap().wire,
            WireStyle::LIBTORRENT
        );
    }

    #[test]
    fn host_patterns() {
        let exact = HostPattern::parse("Tracker.Example.org.").unwrap();
        assert!(exact.matches("tracker.example.org"));
        assert!(!exact.matches("a.tracker.example.org"));
        assert!(!exact.matches("example.org"));
        assert!(exact.same_as("TRACKER.example.org"));
        let wild = HostPattern::parse("*.example.org").unwrap();
        assert!(wild.matches("example.org"));
        assert!(wild.matches("t.example.org"));
        assert!(wild.matches("a.b.EXAMPLE.org"));
        assert!(!wild.matches("badexample.org"));
        assert_eq!(wild.to_string(), "*.example.org");
        assert!(!wild.same_as("example.org"));
        assert!(HostPattern::parse("10.0.0.1").is_ok());
        assert!(HostPattern::parse("[::1]").is_ok());
        for bad in ["", "*.", "*", "a..b", "http://x/", "a b", "x:80"] {
            assert!(HostPattern::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn rules_pick_the_first_match() {
        let deluge = Arc::new(ClientProfile::deluge("2.1.1", "2.0.10.0").unwrap());
        let tr = Arc::new(ClientProfile::transmission("4.0.6").unwrap());
        let rules = IdentityRules {
            default: None,
            rules: vec![
                IdentityRule {
                    host: HostPattern::parse("*.private.example").unwrap(),
                    profile: Some(deluge.clone()),
                },
                IdentityRule {
                    host: HostPattern::parse("other.example").unwrap(),
                    profile: Some(tr.clone()),
                },
            ],
        };
        assert_eq!(rules.resolve(["udp://open.example:1337"]), None);
        assert_eq!(
            rules.resolve([
                "http://other.example/announce",
                "https://t.private.example/pk/announce"
            ]),
            Some(deluge),
            "rule order wins, not tracker order"
        );
        assert_eq!(
            rules.resolve(["udp://OTHER.example:80/announce"]),
            Some(tr.clone())
        );
        let with_default = IdentityRules {
            default: Some(tr.clone()),
            ..rules
        };
        assert_eq!(
            with_default.resolve(["http://x/announce"]),
            Some(tr.clone())
        );
        assert_eq!(with_default.resolve(Vec::<&str>::new()), Some(tr));
    }

    #[test]
    fn active_identity_keeps_apitorrents_for_no_profile() {
        let native = PeerId(*b"-AT0120-abcdefghijkl");
        let a = ActiveIdentity::new(None, native, 7);
        assert_eq!((a.peer_id, a.key), (native, 7));
        assert_eq!(a.client_name("apitorrent 0.1.2"), "apitorrent 0.1.2");
        assert_eq!(a.user_agent(), None);
        assert_eq!(a.num_want(AnnounceEvent::None, false), 50);
        assert_eq!(a.num_want(AnnounceEvent::None, true), 0);
        let mut renewed = a.clone();
        renewed.renew_native();
        assert_eq!(&renewed.peer_id.0[..8], b"-AT0120-");
        assert_ne!(renewed.peer_id, native);
        let d = ActiveIdentity::new(
            Some(Arc::new(
                ClientProfile::deluge("2.1.1", "2.0.10.0").unwrap(),
            )),
            native,
            7,
        );
        assert_eq!(&d.peer_id.0[..8], b"-DE211s-");
        assert_eq!(d.num_want(AnnounceEvent::None, true), 200);
        assert_eq!(d.num_want(AnnounceEvent::Stopped, false), 0);
        assert_eq!(
            d.client_name("apitorrent 0.1.2"),
            "Deluge/2.1.1 libtorrent/2.0.10.0"
        );
    }
}

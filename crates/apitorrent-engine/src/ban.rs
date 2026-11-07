//! Keeping misbehaving and unwanted peers out: bans earned by sending bad data, and IP
//! blocklists loaded by the user or a plugin.
//!
//! A ban is per IP address and session-wide, since data poisoning tends to span swarms. It
//! lasts the base duration, doubling with each repeat offence up to a maximum. How peers
//! earn one is decided by the torrent actor (see `docs/architecture.md`, "Misbehaving
//! peers"); this module only keeps the list.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

/// How long bans last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BanPolicy {
    /// The first ban's length. Each repeat doubles it.
    pub base: Duration,
    pub max: Duration,
}

impl Default for BanPolicy {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(60 * 60),
            max: Duration::from_secs(7 * 24 * 60 * 60),
        }
    }
}

/// Why an address was banned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BanReason {
    /// Sent blocks that failed verification, proven block by block or by a piece that came
    /// wholly from it.
    BadData,
    /// Too many failed pieces it took part in, without proof.
    Untrusted,
    /// Banned by hand (or by a plugin).
    Manual,
}

/// One entry in the ban list, for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BanInfo {
    pub ip: IpAddr,
    pub reason: BanReason,
    /// How many times this address has been banned this session.
    pub count: u32,
    /// When the ban ends, in wall-clock time.
    pub until: SystemTime,
}

#[derive(Debug, Clone)]
struct Entry {
    reason: BanReason,
    count: u32,
    until: Instant,
}

/// The session's bans and blocklist.
#[derive(Debug, Default)]
pub struct IpFilter {
    policy: BanPolicy,
    bans: Mutex<HashMap<IpAddr, Entry>>,
    blocklist: Mutex<Blocklist>,
}

impl IpFilter {
    pub fn new(policy: BanPolicy) -> Self {
        Self {
            policy,
            ..Self::default()
        }
    }

    /// Whether peers at `ip` should be refused: banned now, or on the blocklist.
    pub fn is_blocked(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        let banned = self
            .bans
            .lock()
            .expect("ban list poisoned")
            .get(&ip)
            .is_some_and(|e| Instant::now() < e.until);
        banned
            || self
                .blocklist
                .lock()
                .expect("blocklist poisoned")
                .contains(ip)
    }

    /// Ban `ip`. Returns how long the ban lasts. Banning an address that is already banned
    /// counts as a repeat and extends it.
    pub fn ban(&self, ip: IpAddr, reason: BanReason) -> Duration {
        let ip = canonical(ip);
        let mut bans = self.bans.lock().expect("ban list poisoned");
        let now = Instant::now();
        let count = bans.get(&ip).map_or(0, |e| e.count) + 1;
        let factor = 1u32.checked_shl(count - 1).unwrap_or(u32::MAX);
        let length = self.policy.base.saturating_mul(factor).min(self.policy.max);
        bans.insert(
            ip,
            Entry {
                reason,
                count,
                until: now + length,
            },
        );
        length
    }

    /// Lift a ban. Its repeat count is forgotten too.
    pub fn unban(&self, ip: IpAddr) -> bool {
        self.bans
            .lock()
            .expect("ban list poisoned")
            .remove(&canonical(ip))
            .is_some()
    }

    /// Current bans. Expired entries are kept internally (they still count as previous
    /// offences) but not listed.
    pub fn bans(&self) -> Vec<BanInfo> {
        let now = Instant::now();
        let wall = SystemTime::now();
        let mut out: Vec<BanInfo> = self
            .bans
            .lock()
            .expect("ban list poisoned")
            .iter()
            .filter(|(_, e)| now < e.until)
            .map(|(ip, e)| BanInfo {
                ip: *ip,
                reason: e.reason.clone(),
                count: e.count,
                until: wall + (e.until - now),
            })
            .collect();
        out.sort_by_key(|b| b.ip);
        out
    }

    /// Replace the blocklist.
    pub fn set_blocklist(&self, list: Blocklist) {
        *self.blocklist.lock().expect("blocklist poisoned") = list;
    }

    pub fn blocklist_len(&self) -> usize {
        self.blocklist.lock().expect("blocklist poisoned").len()
    }
}

/// IPv4-mapped IPv6 addresses (`::ffff:1.2.3.4`) are the same host as the IPv4 address.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 @ IpAddr::V4(_) => v4,
    }
}

/// A set of IP ranges, IPv4 and IPv6, with fast lookup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Blocklist {
    v4: Vec<(u32, u32)>,
    v6: Vec<(u128, u128)>,
}

/// A line of a blocklist file that couldn't be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: {message}")]
pub struct BlocklistError {
    pub line: usize,
    pub message: String,
}

impl Blocklist {
    /// Add an inclusive range. Both ends must be the same family.
    pub fn insert(&mut self, first: IpAddr, last: IpAddr) -> Result<(), String> {
        match (canonical(first), canonical(last)) {
            (IpAddr::V4(a), IpAddr::V4(b)) => {
                let (a, b) = (u32::from(a), u32::from(b));
                self.v4.push((a.min(b), a.max(b)));
            }
            (IpAddr::V6(a), IpAddr::V6(b)) => {
                let (a, b) = (u128::from(a), u128::from(b));
                self.v6.push((a.min(b), a.max(b)));
            }
            _ => return Err("range mixes IPv4 and IPv6".into()),
        }
        Ok(())
    }

    /// Sort and merge the ranges. Lookups need this; the parsers call it.
    pub fn finish(&mut self) {
        merge(&mut self.v4);
        merge(&mut self.v6);
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match canonical(ip) {
            IpAddr::V4(a) => lookup(&self.v4, u32::from(a)),
            IpAddr::V6(a) => lookup(&self.v6, u128::from(a)),
        }
    }

    /// The number of ranges after merging.
    pub fn len(&self) -> usize {
        self.v4.len() + self.v6.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Parse a blocklist in any of the common formats, line by line:
    ///
    /// - P2P (`PeerGuardian`): `description:1.2.3.0-1.2.3.255`
    /// - DAT / eMule: `001.002.003.000 - 001.002.003.255 , 100 , description` (entries
    ///   with an access level above 127 are allowed, as eMule does)
    /// - CIDR (`1.2.3.0/24`, `2001:db8::/32`), a single address, or `first-last`
    ///
    /// Blank lines and `#` or `//` comments are skipped. Bad lines are collected rather
    /// than failing the whole list, since published lists often carry a few.
    pub fn parse(text: &str) -> (Self, Vec<BlocklistError>) {
        let mut list = Self::default();
        let mut errors = Vec::new();
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
                continue;
            }
            match parse_line(line) {
                Ok(Some((a, b))) => {
                    if let Err(message) = list.insert(a, b) {
                        errors.push(BlocklistError {
                            line: i + 1,
                            message,
                        });
                    }
                }
                Ok(None) => {}
                Err(message) => errors.push(BlocklistError {
                    line: i + 1,
                    message,
                }),
            }
        }
        list.finish();
        (list, errors)
    }
}

fn merge<T: Ord + Copy + num_step::Step>(ranges: &mut Vec<(T, T)>) {
    ranges.sort_unstable();
    let mut out: Vec<(T, T)> = Vec::with_capacity(ranges.len());
    for &(a, b) in ranges.iter() {
        match out.last_mut() {
            Some(last) if a <= last.1.next_or_max() => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    *ranges = out;
}

fn lookup<T: Ord + Copy>(ranges: &[(T, T)], v: T) -> bool {
    let i = ranges.partition_point(|r| r.1 < v);
    ranges.get(i).is_some_and(|r| r.0 <= v)
}

/// `n + 1`, saturating, for merging adjacent ranges.
mod num_step {
    pub trait Step {
        fn next_or_max(self) -> Self;
    }
    impl Step for u32 {
        fn next_or_max(self) -> Self {
            self.saturating_add(1)
        }
    }
    impl Step for u128 {
        fn next_or_max(self) -> Self {
            self.saturating_add(1)
        }
    }
}

/// One line: `Ok(None)` for an entry that allows rather than blocks (DAT access level).
fn parse_line(line: &str) -> Result<Option<(IpAddr, IpAddr)>, String> {
    // DAT: "first - last , level , description"
    if line.contains(',') {
        let mut fields = line.splitn(3, ',');
        let range = fields.next().unwrap_or_default();
        if let Some(level) = fields.next() {
            let level: u32 = level.trim().parse().map_err(|_| "bad access level")?;
            if level > 127 {
                return Ok(None);
            }
        }
        return parse_range(range).map(Some);
    }
    // A bare range (IPv6 ranges contain colons, so try the whole line first), then P2P:
    // "description:first-last", where the description may contain colons too. P2P lists
    // are IPv4-only in practice.
    if let Ok(r) = parse_range(line) {
        return Ok(Some(r));
    }
    if let Some((_, range)) = line.rsplit_once(':')
        && let Ok(r) = parse_range(range)
    {
        return Ok(Some(r));
    }
    Err(format!("not a recognised blocklist entry: {line}"))
}

fn parse_range(s: &str) -> Result<(IpAddr, IpAddr), String> {
    let s = s.trim();
    if let Some((addr, bits)) = s.split_once('/') {
        let addr = parse_ip(addr)?;
        let bits: u32 = bits.trim().parse().map_err(|_| "bad prefix length")?;
        return cidr(addr, bits);
    }
    if let Some((a, b)) = s.split_once('-') {
        return Ok((parse_ip(a)?, parse_ip(b)?));
    }
    let ip = parse_ip(s)?;
    Ok((ip, ip))
}

/// Parse an address, accepting zero-padded IPv4 (`001.002.003.004`) as DAT files use.
fn parse_ip(s: &str) -> Result<IpAddr, String> {
    let s = s.trim();
    if let Ok(ip) = s.parse::<IpAddr>() {
        return Ok(ip);
    }
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() == 4 {
        let mut octets = [0u8; 4];
        for (o, p) in octets.iter_mut().zip(&parts) {
            *o = p.parse().map_err(|_| format!("bad address {s}"))?;
        }
        return Ok(IpAddr::V4(Ipv4Addr::from(octets)));
    }
    Err(format!("bad address {s}"))
}

fn cidr(addr: IpAddr, bits: u32) -> Result<(IpAddr, IpAddr), String> {
    match addr {
        IpAddr::V4(a) if bits <= 32 => {
            let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
            let first = u32::from(a) & mask;
            Ok((
                IpAddr::V4(Ipv4Addr::from(first)),
                IpAddr::V4(Ipv4Addr::from(first | !mask)),
            ))
        }
        IpAddr::V6(a) if bits <= 128 => {
            let mask = u128::MAX.checked_shl(128 - bits).unwrap_or(0);
            let first = u128::from(a) & mask;
            Ok((
                IpAddr::V6(Ipv6Addr::from(first)),
                IpAddr::V6(Ipv6Addr::from(first | !mask)),
            ))
        }
        _ => Err("prefix length too long".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn bans_escalate_and_cap() {
        let f = IpFilter::new(BanPolicy {
            base: Duration::from_secs(10),
            max: Duration::from_secs(35),
        });
        assert!(!f.is_blocked(ip("10.0.0.1")));
        assert_eq!(
            f.ban(ip("10.0.0.1"), BanReason::BadData),
            Duration::from_secs(10)
        );
        assert!(f.is_blocked(ip("10.0.0.1")));
        assert!(
            f.is_blocked(ip("::ffff:10.0.0.1")),
            "mapped addresses are the same host"
        );
        assert_eq!(
            f.ban(ip("10.0.0.1"), BanReason::BadData),
            Duration::from_secs(20)
        );
        assert_eq!(
            f.ban(ip("10.0.0.1"), BanReason::BadData),
            Duration::from_secs(35)
        );
        let list = f.bans();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].count, 3);
        assert!(f.unban(ip("10.0.0.1")));
        assert!(!f.is_blocked(ip("10.0.0.1")));
        assert_eq!(
            f.ban(ip("10.0.0.1"), BanReason::Manual),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn parses_common_formats() {
        let text = "\
# comment
// another
Some ISP:1.2.3.0-1.2.3.255
Name: with: colons:5.6.7.8-5.6.7.9
010.000.000.000 - 010.255.255.255 , 000 , private
011.000.000.000 - 011.000.000.255 , 200 , allowed by level
192.168.0.0/16
2001:db8::/32
8.8.8.8
garbage here
";
        let (list, errors) = Blocklist::parse(text);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].line, 10);
        for yes in [
            "1.2.3.0",
            "1.2.3.255",
            "5.6.7.9",
            "10.20.30.40",
            "192.168.1.1",
            "2001:db8::1",
            "8.8.8.8",
            "::ffff:8.8.8.8",
        ] {
            assert!(list.contains(ip(yes)), "{yes} should be blocked");
        }
        for no in [
            "1.2.4.0",
            "5.6.7.10",
            "11.0.0.1",
            "172.16.0.1",
            "2001:db9::1",
            "8.8.4.4",
        ] {
            assert!(!list.contains(ip(no)), "{no} should not be blocked");
        }
    }

    #[test]
    fn ranges_merge() {
        let (list, errors) =
            Blocklist::parse("1.0.0.0-1.0.0.10\n1.0.0.11-1.0.0.20\n1.0.0.5-1.0.0.6\n0.0.0.0/0");
        assert!(errors.is_empty());
        assert_eq!(list.len(), 1);
        assert!(list.contains(ip("255.255.255.255")));
    }
}

//! Version ordering for rules: lenient SemVer, dates, and a natural sort.
//!
//! Each scheme turns a text into a [`Key`] that sorts the way people expect. A text that
//! doesn't parse under the scheme has no key; rules leave such candidates out rather than
//! guess where they go.

use std::cmp::Ordering;

use apitorrent_types::automation::VersionScheme;

/// A comparable version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Semver(Semver),
    /// Seconds since the Unix epoch (UTC).
    Date(i64),
    Natural(Vec<Chunk>),
}

impl Ord for Key {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Key::Semver(a), Key::Semver(b)) => a.cmp(b),
            (Key::Date(a), Key::Date(b)) => a.cmp(b),
            (Key::Natural(a), Key::Natural(b)) => a.cmp(b),
            // One rule uses one scheme, so mixed keys don't meet; order them stably anyway.
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Key {
    fn rank(&self) -> u8 {
        match self {
            Key::Semver(_) => 0,
            Key::Date(_) => 1,
            Key::Natural(_) => 2,
        }
    }
}

/// The key of `text` under `scheme`, or why it has none.
pub fn key(scheme: VersionScheme, text: &str) -> Result<Key, String> {
    match scheme {
        VersionScheme::Semver => parse_semver(text).map(Key::Semver).ok_or_else(|| {
            format!("`{text}` isn't a version (dotted numbers, like 24.04.1 or 2.1.0-rc.1)")
        }),
        VersionScheme::Date => parse_date(text)
            .map(Key::Date)
            .ok_or_else(|| format!("`{text}` isn't a date (like 2026-11-08 or 2026.11.08)")),
        VersionScheme::Natural => {
            if text.trim().is_empty() {
                Err("there is no text to order by".to_owned())
            } else {
                Ok(Key::Natural(natural(text)))
            }
        }
    }
}

// --- SemVer, leniently --------------------------------------------------------------------

/// Dotted numbers (any count, leading zeros allowed), then an optional pre-release. Build
/// metadata (`+…`) is ignored, as SemVer says. Missing parts count as 0, so `24.04` equals
/// `24.4.0`.
#[derive(Debug, Clone)]
pub struct Semver {
    numbers: Vec<u64>,
    pre: Vec<PreId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PreId {
    Num(u64),
    Text(String),
}

impl Ord for PreId {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (PreId::Num(a), PreId::Num(b)) => a.cmp(b),
            // Numeric identifiers sort before alphanumeric ones (SemVer 11.4.3).
            (PreId::Num(_), PreId::Text(_)) => Ordering::Less,
            (PreId::Text(_), PreId::Num(_)) => Ordering::Greater,
            (PreId::Text(a), PreId::Text(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for PreId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Semver {
    fn cmp(&self, other: &Self) -> Ordering {
        let n = self.numbers.len().max(other.numbers.len());
        for i in 0..n {
            let a = self.numbers.get(i).copied().unwrap_or(0);
            let b = other.numbers.get(i).copied().unwrap_or(0);
            match a.cmp(&b) {
                Ordering::Equal => {}
                o => return o,
            }
        }
        // A pre-release sorts before its release.
        match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => self.pre.cmp(&other.pre),
        }
    }
}

/// Equal as ordered: `24.04` and `24.4.0` are the same version.
impl PartialEq for Semver {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Semver {}

impl PartialOrd for Semver {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn parse_semver(text: &str) -> Option<Semver> {
    let t = text.trim();
    let t = t.strip_prefix(['v', 'V']).unwrap_or(t);
    let t = t.split('+').next()?;
    let (core, pre) = match t.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (t, None),
    };
    let numbers = core
        .split('.')
        .map(|p| {
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                None
            } else {
                p.parse::<u64>().ok()
            }
        })
        .collect::<Option<Vec<u64>>>()?;
    if numbers.is_empty() {
        return None;
    }
    let pre = match pre {
        None => Vec::new(),
        Some(p) => p
            .split('.')
            .map(|id| {
                if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                    None
                } else if id.bytes().all(|b| b.is_ascii_digit()) {
                    id.parse().ok().map(PreId::Num)
                } else {
                    Some(PreId::Text(id.to_ascii_lowercase()))
                }
            })
            .collect::<Option<Vec<_>>>()?,
    };
    Some(Semver { numbers, pre })
}

// --- Dates ----------------------------------------------------------------------------

/// Days from 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

fn digits(s: &str, n: usize) -> Option<u32> {
    (s.len() == n && s.bytes().all(|b| b.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

/// `YYYY-MM-DD`, `YYYY.MM.DD`, `YYYY/MM/DD` or `YYYYMMDD`, optionally followed by a time
/// (`T` or a space, `HH:MM[:SS[.fff]]`) and a zone (`Z` or `±HH:MM`; none means UTC).
/// Returns seconds since the epoch, UTC.
pub fn parse_date(text: &str) -> Option<i64> {
    let text = text.trim();
    let (date, rest) = match text.find(['T', 't', ' ']) {
        Some(i) => (&text[..i], Some(&text[i + 1..])),
        None => (text, None),
    };
    let (year, month, day) = if date.len() == 8 {
        (
            digits(&date[..4], 4)?,
            digits(&date[4..6], 2)?,
            digits(&date[6..], 2)?,
        )
    } else {
        let sep = date.chars().find(|c| matches!(c, '-' | '.' | '/'))?;
        let mut parts = date.split(sep);
        let year = digits(parts.next()?, 4)?;
        let month = small(parts.next()?)?;
        let day = small(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        (year, month, day)
    };
    let year = i64::from(year);
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    let mut secs = days_from_civil(year, month, day) * 86_400;
    if let Some(rest) = rest.map(str::trim).filter(|r| !r.is_empty()) {
        let (time, offset) = split_zone(rest)?;
        let mut parts = time.split(':');
        let hour = digits(parts.next()?, 2)?;
        let minute = digits(parts.next()?, 2)?;
        let second = match parts.next() {
            Some(sec) => digits(sec.split(['.', ',']).next()?, 2)?,
            None => 0,
        };
        if parts.next().is_some() || hour > 23 || minute > 59 || second > 60 {
            return None;
        }
        secs += i64::from(hour * 3600 + minute * 60 + second) - offset;
    }
    Some(secs)
}

/// One or two digits.
fn small(s: &str) -> Option<u32> {
    if s.is_empty() || s.len() > 2 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// A time and its zone offset in seconds.
fn split_zone(rest: &str) -> Option<(&str, i64)> {
    if let Some(time) = rest.strip_suffix(['Z', 'z']) {
        return Some((time, 0));
    }
    if let Some(i) = rest.rfind(['+', '-']) {
        let (time, zone) = rest.split_at(i);
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        let z = &zone[1..];
        let (zh, zm) = match z.split_once(':') {
            Some((h, m)) => (digits(h, 2)?, digits(m, 2)?),
            None if z.len() == 4 => (digits(&z[..2], 2)?, digits(&z[2..], 2)?),
            None => (digits(z, 2)?, 0),
        };
        return Some((time, sign * i64::from(zh * 3600 + zm * 60)));
    }
    Some((rest, 0))
}

/// A proleptic Gregorian date from days since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).expect("1 to 31");
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).expect("1 to 12");
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Milliseconds since the epoch as RFC 3339 in UTC, to the second.
pub fn format_rfc3339(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        t / 3600,
        t % 3600 / 60,
        t % 60
    )
}

// --- Natural sort ---------------------------------------------------------------------

/// A run of digits (compared as a number) or of other characters (compared ignoring case).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Chunk {
    /// Digits sort before text at the same position. Leading zeros are dropped for the
    /// value; the digit count breaks ties so `07` and `7` stay distinct.
    Num(u128, usize),
    Text(String),
}

pub fn natural(text: &str) -> Vec<Chunk> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_digits = false;
    let flush = |cur: &mut String, in_digits: bool, out: &mut Vec<Chunk>| {
        if cur.is_empty() {
            return;
        }
        if in_digits {
            let trimmed = cur.trim_start_matches('0');
            let value =
                trimmed
                    .parse::<u128>()
                    .unwrap_or(if trimmed.is_empty() { 0 } else { u128::MAX });
            out.push(Chunk::Num(value, cur.len()));
        } else {
            out.push(Chunk::Text(cur.to_lowercase()));
        }
        cur.clear();
    };
    for c in text.chars() {
        let d = c.is_ascii_digit();
        if d != in_digits {
            flush(&mut cur, in_digits, &mut out);
            in_digits = d;
        }
        cur.push(c);
    }
    flush(&mut cur, in_digits, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(s: &str) -> Key {
        key(VersionScheme::Semver, s).unwrap()
    }

    #[test]
    fn semver_is_lenient_and_ordered() {
        assert!(sv("24.04.1") > sv("24.04"));
        assert_eq!(sv("24.04"), sv("24.4.0"));
        assert!(sv("26.04") > sv("24.10"));
        assert!(sv("v2.1.0") > sv("2.1.0-rc.2"));
        assert!(sv("2.1.0-rc.10") > sv("2.1.0-rc.2"));
        assert!(sv("2.1.0-alpha") < sv("2.1.0-alpha.1"));
        assert!(sv("2.1.0-1") < sv("2.1.0-alpha"));
        assert_eq!(sv("1.0.0+build.5"), sv("1.0.0"));
        assert!(sv("10.0") > sv("9.99.99"));
        for bad in ["", "x", "1..2", "1.a", "24.04 LTS", "1.0-", "1.0-a b"] {
            assert!(key(VersionScheme::Semver, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn dates() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("2026-11-08"), parse_date("2026.11.08"));
        assert_eq!(parse_date("20261108"), parse_date("2026/11/08"));
        assert_eq!(
            parse_date("2026-11-08T12:00:00Z"),
            parse_date("2026-11-08T04:00:00-08:00")
        );
        assert_eq!(
            parse_date("2026-11-08T12:00:00.123+0100").unwrap(),
            parse_date("2026-11-08").unwrap() + 11 * 3600
        );
        assert_eq!(
            parse_date("2000-03-01").unwrap() - parse_date("2000-02-28").unwrap(),
            2 * 86_400,
            "2000 is a leap year"
        );
        assert!(parse_date("2026-02-29").is_none());
        assert!(parse_date("2026-13-01").is_none());
        assert!(parse_date("2026-11-08T25:00").is_none());
        assert!(parse_date("Sun, 08 Nov 2026").is_none());
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        let t = parse_date("2026-11-08T12:34:56Z").unwrap();
        assert_eq!(format_rfc3339(t * 1000 + 999), "2026-11-08T12:34:56Z");
        assert_eq!(
            format_rfc3339(parse_date("2000-02-29").unwrap() * 1000),
            "2000-02-29T00:00:00Z"
        );
        let d = |s| key(VersionScheme::Date, s).unwrap();
        assert!(d("2026-11-09") > d("2026-11-08T23:59:59Z"));
    }

    #[test]
    fn natural_sort() {
        let n = |s| key(VersionScheme::Natural, s).unwrap();
        assert!(n("build-10") > n("build-9"));
        assert!(n("Release 2") > n("release 1"));
        assert!(n("a2b10") > n("a2b9"));
        assert_ne!(n("img-007"), n("img-7"));
        assert!(key(VersionScheme::Natural, "  ").is_err());
    }
}

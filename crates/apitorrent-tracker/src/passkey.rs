//! Passkeys: the per-user secret many private trackers put in their announce URLs.
//!
//! A tracker knows its users by a token in the URL, either as a path segment
//! (`https://tracker.example.org/0123456789abcdef0123456789abcdef/announce`) or as a query
//! parameter (`…/announce.php?passkey=…`, `?authkey=…`, `?torrent_pass=…`, `?pid=…`). When
//! a user rolls their token, every torrent from that tracker needs the new one.
//!
//! [`find`] locates the token without parsing and re-serialising the URL, so [`replace`]
//! changes those characters and nothing else. Passkeys are secrets: [`redact`] and
//! [`redact_text`] hide them before a URL reaches a log, and [`mask`] shows enough of one to
//! tell two apart.

use std::borrow::Cow;
use std::ops::Range;

/// Query parameters that carry a passkey, compared without regard to case.
const QUERY_KEYS: &[&str] = &[
    "passkey",
    "pass_key",
    "authkey",
    "auth_key",
    "torrent_pass",
    "pid",
    "apikey",
    "api_key",
    "token",
];

/// What [`redact`] puts where a passkey was.
pub const REDACTED: &str = "<passkey>";

/// A passkey found in a tracker URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found<'a> {
    /// The tracker's host, lowercased, without port or user info.
    pub host: String,
    /// The passkey itself.
    pub key: &'a str,
    /// Where `key` is in the URL.
    pub span: Range<usize>,
}

/// A passkey's characters: letters, digits, `-` and `_`.
fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// A query value that looks like a token, not a word or a number: at least 8 token
/// characters.
fn query_token(v: &str) -> bool {
    v.len() >= 8 && v.len() <= 256 && v.chars().all(token_char)
}

/// A path segment that looks like a token: 16 to 256 letters and digits, with at least
/// one of each (so `announce` and long numbers are not mistaken for one).
fn path_token(s: &str) -> bool {
    (16..=256).contains(&s.len())
        && s.chars().all(|c| c.is_ascii_alphanumeric())
        && s.chars().any(|c| c.is_ascii_digit())
        && s.chars().any(|c| c.is_ascii_alphabetic())
}

/// The passkey in a tracker URL, if it has one. A query parameter wins over a path
/// segment; within each, the first one.
#[must_use]
pub fn find(url: &str) -> Option<Found<'_>> {
    let scheme_end = url.find("://")? + 3;
    let rest = &url[scheme_end..];
    let authority_len = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_len];
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(v6) = host_port.strip_prefix('[') {
        v6.split_once(']').map_or(v6, |(h, _)| h)
    } else {
        host_port.split_once(':').map_or(host_port, |(h, _)| h)
    };
    if host.is_empty() {
        return None;
    }
    let host = host.to_ascii_lowercase();
    let path_start = scheme_end + authority_len;
    let after = &url[path_start..];
    let end = after.find('#').map_or(url.len(), |i| path_start + i);
    let (path_end, query) = match url[path_start..end].find('?') {
        Some(i) => (path_start + i, Some(path_start + i + 1)),
        None => (end, None),
    };
    if let Some(q) = query {
        let mut at = q;
        for pair in url[q..end].split('&') {
            if let Some((name, value)) = pair.split_once('=')
                && QUERY_KEYS.iter().any(|k| k.eq_ignore_ascii_case(name))
                && query_token(value)
            {
                let start = at + name.len() + 1;
                return Some(Found {
                    host,
                    key: value,
                    span: start..start + value.len(),
                });
            }
            at += pair.len() + 1;
        }
    }
    let mut at = path_start;
    for seg in url[path_start..path_end].split('/') {
        if path_token(seg) {
            return Some(Found {
                host,
                key: seg,
                span: at..at + seg.len(),
            });
        }
        at += seg.len() + 1;
    }
    None
}

/// Whether `key` may stand in for a passkey: what [`find`] would recognise, so the new
/// URL is found again by the next search.
#[must_use]
pub fn acceptable(old_in: &str, key: &str) -> bool {
    match find(old_in) {
        Some(f) if f.span.start > 0 && old_in[..f.span.start].ends_with('=') => query_token(key),
        Some(_) => path_token(key),
        None => false,
    }
}

/// `url` with its passkey `old` changed to `new`; `None` when its passkey isn't `old`.
#[must_use]
pub fn replace(url: &str, old: &str, new: &str) -> Option<String> {
    let f = find(url)?;
    (f.key == old).then(|| format!("{}{new}{}", &url[..f.span.start], &url[f.span.end..]))
}

/// `url` with its passkey, if any, replaced by [`REDACTED`], for logs.
#[must_use]
pub fn redact(url: &str) -> Cow<'_, str> {
    match find(url) {
        Some(f) => Cow::Owned(format!(
            "{}{REDACTED}{}",
            &url[..f.span.start],
            &url[f.span.end..]
        )),
        None => Cow::Borrowed(url),
    }
}

/// `text` with the passkey of every URL in it redacted (an error message that quotes the
/// URL, say).
#[must_use]
pub fn redact_text(text: &str) -> Cow<'_, str> {
    if !text.contains("://") {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        // The URL starts at its scheme: the letters before `://`.
        let scheme = rest[..i]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.'))
            .map_or(0, |j| j + 1);
        let len = rest[i..]
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')'))
            .unwrap_or(rest.len() - i);
        let end = i + len;
        out.push_str(&rest[..scheme]);
        out.push_str(&redact(&rest[scheme..end]));
        rest = &rest[end..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// Enough of a passkey to tell it from another: its first and last four characters.
#[must_use]
pub fn mask(key: &str) -> String {
    if key.len() <= 10 {
        return "…".to_owned();
    }
    format!("{}…{}", &key[..4], &key[key.len() - 4..])
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn finds_path_and_query_passkeys() {
        let url = format!("https://Tracker.Example.org:443/{HEX}/announce");
        let f = find(&url).unwrap();
        assert_eq!(f.host, "tracker.example.org");
        assert_eq!(f.key, HEX);
        assert_eq!(&url[f.span.clone()], HEX);

        let url = "http://t.example.org/announce.php?uid=5&PassKey=Ab12Cd34Ef56&x=1";
        assert_eq!(find(url).unwrap().key, "Ab12Cd34Ef56");
        let url = "udp://u@t.example.org:6969/announce?torrent_pass=abcdefgh12345678#frag";
        assert_eq!(find(url).unwrap().key, "abcdefgh12345678");
        assert_eq!(find("http://[::1]:80/abcdefgh12345678/announce").unwrap().host, "::1");
    }

    #[test]
    fn public_trackers_have_none() {
        for url in [
            "udp://tracker.opentrackr.org:1337/announce",
            "http://tracker.example.org/announce?info=12345678901234567890",
            "https://example.org/announce?passkey=short",
            "https://example.org/12345678901234567890/announce",
            "not a url",
        ] {
            assert_eq!(find(url), None, "{url}");
        }
    }

    #[test]
    fn replaces_only_the_passkey() {
        let url = format!("https://t.example.org/{HEX}/announce?{HEX}");
        let new = "fedcba9876543210fedcba9876543210";
        assert_eq!(
            replace(&url, HEX, new).unwrap(),
            format!("https://t.example.org/{new}/announce?{HEX}")
        );
        assert_eq!(replace(&url, "other", new), None);
        assert!(acceptable(&url, new));
        assert!(!acceptable(&url, "short"));
        assert!(acceptable("http://t/a?passkey=12345678", "abcdefgh"));
    }

    #[test]
    fn redacts_for_logs() {
        let url = format!("https://t.example.org/{HEX}/announce");
        assert_eq!(redact(&url), "https://t.example.org/<passkey>/announce");
        let text = format!("error sending request for url ({url}): refused, and http://x/a");
        assert_eq!(
            redact_text(&text),
            "error sending request for url (https://t.example.org/<passkey>/announce): refused, and http://x/a"
        );
        assert_eq!(mask(HEX), "0123…cdef");
        assert_eq!(mask("short"), "…");
    }
}

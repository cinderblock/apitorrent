//! The `network` capability: which URLs a plugin's `fetch` may reach.
//!
//! A grant is a host pattern with an optional scheme and port:
//!
//! | Grant | Allows |
//! |---|---|
//! | `example.com` | `https://example.com/…` on port 443 |
//! | `*.example.com` | HTTPS to any subdomain (`a.example.com`, `a.b.example.com`), not `example.com` itself |
//! | `example.com:8443` | HTTPS on port 8443 |
//! | `example.com:*` | HTTPS on any port |
//! | `http://example.com` | HTTP on port 80 and HTTPS on port 443 |
//! | `http://127.0.0.1:8080` | an IP address; plain HTTP on 8080 (or HTTPS on 8080) |
//!
//! Plain HTTP must be asked for explicitly, so an approver sees it. Host names are compared
//! after IDNA normalisation, so a look-alike Unicode name can't match an ASCII grant.
//!
//! Granting a host name says nothing about where it resolves. [`is_public`] is the check
//! the host applies to resolved addresses: a granted name that resolves to a loopback,
//! private or link-local address is refused (DNS rebinding would otherwise let a plugin
//! reach the daemon itself or the LAN). Only a grant that names an IP address, or
//! `localhost`, reaches such addresses, because those URLs never go through DNS.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostPattern {
    /// One host name, lowercase ASCII (IDNA applied).
    Name(String),
    /// Any proper subdomain of this name.
    Subdomains(String),
    Ip(IpAddr),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortPattern {
    /// 443 for HTTPS, 80 for plain HTTP.
    Default,
    Any,
    Port(u16),
}

/// One parsed `network` grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkGrant {
    /// Plain `http://` is allowed as well as HTTPS.
    pub plain_http: bool,
    pub host: HostPattern,
    pub port: PortPattern,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid network grant `{grant}`: {reason}")]
pub struct GrantError {
    pub grant: String,
    pub reason: String,
}

impl NetworkGrant {
    pub fn parse(text: &str) -> Result<Self, GrantError> {
        let fail = |reason: &str| GrantError {
            grant: text.to_owned(),
            reason: reason.to_owned(),
        };
        let trimmed = text.trim();
        let (plain_http, rest) = if let Some(rest) = trimmed.strip_prefix("http://") {
            (true, rest)
        } else if let Some(rest) = trimmed.strip_prefix("https://") {
            (false, rest)
        } else if trimmed.contains("://") {
            return Err(fail("only http:// and https:// are supported"));
        } else {
            (false, trimmed)
        };
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        if rest.is_empty() || rest.contains(['/', '?', '#', '@']) {
            return Err(fail(
                "expected a host, optionally with a port: no path or user",
            ));
        }
        // Split off the port, minding IPv6 brackets.
        let (host_text, port_text) = if let Some(after) = rest.strip_prefix('[') {
            let (inside, tail) = after.split_once(']').ok_or_else(|| fail("unclosed `[`"))?;
            let port = match tail {
                "" => None,
                t => Some(t.strip_prefix(':').ok_or_else(|| fail("bad port"))?),
            };
            (format!("[{inside}]"), port)
        } else {
            match rest.rsplit_once(':') {
                Some((h, p)) => (h.to_owned(), Some(p)),
                None => (rest.to_owned(), None),
            }
        };
        let port = match port_text {
            None => PortPattern::Default,
            Some("*") => PortPattern::Any,
            Some(p) => PortPattern::Port(
                p.parse::<u16>()
                    .ok()
                    .filter(|p| *p != 0)
                    .ok_or_else(|| fail("bad port"))?,
            ),
        };
        let host = if let Some(domain) = host_text.strip_prefix("*.") {
            match Host::parse(domain).map_err(|e| fail(&e.to_string()))? {
                Host::Domain(d) if d.contains('.') => HostPattern::Subdomains(d),
                Host::Domain(_) => {
                    return Err(fail(
                        "a wildcard needs at least two labels, like *.example.com",
                    ));
                }
                _ => return Err(fail("a wildcard can't apply to an IP address")),
            }
        } else if host_text.contains('*') {
            return Err(fail(
                "`*` is only allowed as the whole first label (*.example.com)",
            ));
        } else {
            match Host::parse(&host_text).map_err(|e| fail(&e.to_string()))? {
                Host::Domain(d) => HostPattern::Name(d),
                Host::Ipv4(ip) => HostPattern::Ip(IpAddr::V4(ip)),
                Host::Ipv6(ip) => HostPattern::Ip(IpAddr::V6(ip)),
            }
        };
        Ok(Self {
            plain_http,
            host,
            port,
        })
    }

    /// Whether this grant allows a request to `url`. Only `http` and `https` URLs can match.
    pub fn allows(&self, url: &Url) -> bool {
        let https = match url.scheme() {
            "https" => true,
            "http" if self.plain_http => false,
            _ => return false,
        };
        let Some(port) = url.port_or_known_default() else {
            return false;
        };
        let port_ok = match self.port {
            PortPattern::Any => true,
            PortPattern::Port(p) => p == port,
            PortPattern::Default => port == if https { 443 } else { 80 },
        };
        port_ok && self.host_matches(url.host())
    }

    fn host_matches(&self, host: Option<Host<&str>>) -> bool {
        match (&self.host, host) {
            (HostPattern::Name(n), Some(Host::Domain(d))) => n == &d.to_ascii_lowercase(),
            (HostPattern::Subdomains(n), Some(Host::Domain(d))) => {
                let d = d.to_ascii_lowercase();
                d.len() > n.len() + 1 && d.ends_with(n.as_str()) && {
                    d.as_bytes()[d.len() - n.len() - 1] == b'.'
                }
            }
            (HostPattern::Ip(ip), Some(Host::Ipv4(v4))) => *ip == IpAddr::V4(v4),
            (HostPattern::Ip(ip), Some(Host::Ipv6(v6))) => *ip == IpAddr::V6(v6),
            _ => false,
        }
    }

    /// Whether everything this grant allows, `other` allows too.
    pub fn covered_by(&self, other: &Self) -> bool {
        let scheme_ok = !self.plain_http || other.plain_http;
        let port_ok = match (self.port, other.port) {
            (_, PortPattern::Any) => true,
            (a, b) => a == b,
        };
        let host_ok = match (&self.host, &other.host) {
            (a, b) if a == b => true,
            (HostPattern::Name(n) | HostPattern::Subdomains(n), HostPattern::Subdomains(d)) => {
                n.len() > d.len() + 1
                    && n.ends_with(d.as_str())
                    && n.as_bytes()[n.len() - d.len() - 1] == b'.'
            }
            _ => false,
        };
        scheme_ok && port_ok && host_ok
    }
}

impl fmt::Display for NetworkGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.plain_http {
            "http://"
        } else {
            "https://"
        })?;
        match &self.host {
            HostPattern::Name(n) => f.write_str(n)?,
            HostPattern::Subdomains(n) => write!(f, "*.{n}")?,
            HostPattern::Ip(IpAddr::V4(ip)) => write!(f, "{ip}")?,
            HostPattern::Ip(IpAddr::V6(ip)) => write!(f, "[{ip}]")?,
        }
        match self.port {
            PortPattern::Default => Ok(()),
            PortPattern::Any => f.write_str(":*"),
            PortPattern::Port(p) => write!(f, ":{p}"),
        }
    }
}

/// Parsed grants, checked together.
#[derive(Debug, Clone, Default)]
pub struct NetworkGrants(Vec<NetworkGrant>);

/// The grant a settings value stands for (see `networkFromSettings`): a URL grants its
/// origin, anything else is read as a grant pattern. `None` for values that are neither.
pub fn grant_from_setting(value: &str) -> Option<NetworkGrant> {
    if let Ok(url) = Url::parse(value.trim())
        && matches!(url.scheme(), "http" | "https")
        && let Some(host) = url.host()
    {
        let host = match host {
            Host::Domain(d) => HostPattern::Name(d.to_ascii_lowercase()),
            Host::Ipv4(ip) => HostPattern::Ip(IpAddr::V4(ip)),
            Host::Ipv6(ip) => HostPattern::Ip(IpAddr::V6(ip)),
        };
        return Some(NetworkGrant {
            plain_http: url.scheme() == "http",
            host,
            port: url.port().map_or(PortPattern::Default, PortPattern::Port),
        });
    }
    NetworkGrant::parse(value).ok()
}

impl NetworkGrants {
    /// The manifest's grants plus those from settings values. Unparseable entries are
    /// skipped: manifests are validated at install, settings values are the admin's.
    pub fn combine(manifest: &[String], from_settings: &[String]) -> Self {
        Self(
            manifest
                .iter()
                .filter_map(|g| NetworkGrant::parse(g).ok())
                .chain(from_settings.iter().filter_map(|v| grant_from_setting(v)))
                .collect(),
        )
    }

    pub fn parse(grants: &[String]) -> Result<Self, GrantError> {
        grants
            .iter()
            .map(|g| NetworkGrant::parse(g))
            .collect::<Result<_, _>>()
            .map(Self)
    }

    pub fn allows(&self, url: &Url) -> bool {
        self.0.iter().any(|g| g.allows(url))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Whether a resolved address may be reached through a granted host *name*: not loopback,
/// private, link-local, shared (CGNAT), documentation, benchmarking, multicast or reserved.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            is_public_v6(v6)
        }
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (64..128).contains(&b)) // shared address space (CGNAT)
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 198 && (18..20).contains(&b)) // benchmarking
        || a >= 240) // reserved
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00 // unique local
        || (s[0] & 0xffc0) == 0xfe80 // link-local
        || (s[0] & 0xffc0) == 0xfec0 // site-local (deprecated)
        || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
        || (s[0] == 0x0064 && s[1] == 0xff9b) // NAT64, which embeds IPv4 addresses
        || (s[0] == 0x2002) // 6to4, which embeds IPv4 addresses
        || ip.to_ipv4().is_some_and(|v4| s[5] == 0 && !is_public_v4(v4))) // IPv4-compatible
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(s: &str) -> NetworkGrant {
        NetworkGrant::parse(s).unwrap()
    }

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn plain_names_allow_https_on_443_only() {
        let grant = g("example.com");
        assert!(grant.allows(&u("https://example.com/feed.xml")));
        assert!(grant.allows(&u("https://EXAMPLE.com:443/")));
        assert!(!grant.allows(&u("http://example.com/")));
        assert!(!grant.allows(&u("https://example.com:8443/")));
        assert!(!grant.allows(&u("https://www.example.com/")));
        assert!(!grant.allows(&u("https://example.com.evil.net/")));
        assert!(!grant.allows(&u("https://evilexample.com/")));
        assert!(!grant.allows(&u("ftp://example.com/")));
        assert!(!grant.allows(&u("file:///etc/passwd")));
    }

    #[test]
    fn wildcards_are_subdomains_only() {
        let grant = g("*.ubuntu.com");
        assert!(grant.allows(&u("https://releases.ubuntu.com/")));
        assert!(grant.allows(&u("https://a.b.ubuntu.com/")));
        assert!(!grant.allows(&u("https://ubuntu.com/")));
        assert!(!grant.allows(&u("https://notubuntu.com/")));
        assert!(NetworkGrant::parse("*.com").is_err());
        assert!(NetworkGrant::parse("a.*.com").is_err());
        assert!(NetworkGrant::parse("*.1.2.3.4").is_err());
    }

    #[test]
    fn ports_schemes_and_ips() {
        let grant = g("http://127.0.0.1:8080");
        assert!(grant.allows(&u("http://127.0.0.1:8080/x")));
        assert!(grant.allows(&u("https://127.0.0.1:8080/x")));
        assert!(!grant.allows(&u("http://127.0.0.1:8081/x")));
        assert!(!grant.allows(&u("http://localhost:8080/x")));
        let grant = g("[::1]:*");
        assert!(grant.allows(&u("https://[::1]:1234/")));
        assert!(!grant.allows(&u("http://[::1]:1234/")));
        let grant = g("http://example.com");
        assert!(grant.allows(&u("http://example.com/")));
        assert!(grant.allows(&u("https://example.com/")));
        assert!(!grant.allows(&u("http://example.com:8080/")));
        for bad in [
            "",
            "example.com/path",
            "user@example.com",
            "ftp://x.com",
            "x.com:0",
            "x.com:99999",
            "[::1",
        ] {
            assert!(NetworkGrant::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn idna_is_applied_before_matching() {
        // A Cyrillic "а" makes a different (punycode) name, so it can't pass for the
        // ASCII grant.
        let grant = g("example.com");
        assert!(!grant.allows(&u("https://exаmple.com/")));
        assert_eq!(
            g("BÜCHER.example").host,
            HostPattern::Name("xn--bcher-kva.example".into())
        );
    }

    #[test]
    fn coverage() {
        assert!(g("a.example.com").covered_by(&g("*.example.com")));
        assert!(g("*.a.example.com").covered_by(&g("*.example.com")));
        assert!(!g("example.com").covered_by(&g("*.example.com")));
        assert!(g("example.com").covered_by(&g("http://example.com")));
        assert!(!g("http://example.com").covered_by(&g("example.com")));
        assert!(g("example.com:8443").covered_by(&g("example.com:*")));
        assert!(!g("example.com:*").covered_by(&g("example.com:8443")));
    }

    #[test]
    fn display_round_trips() {
        for s in [
            "https://example.com",
            "http://*.example.com:*",
            "https://[::1]:80",
            "https://10.0.0.1:443",
        ] {
            assert_eq!(g(s).to_string(), s);
            assert_eq!(g(&g(s).to_string()), g(s));
        }
    }

    #[test]
    fn settings_values_become_grants() {
        let g = grant_from_setting("http://127.0.0.1:8080/feed.xml?x=1").unwrap();
        assert!(g.allows(&u("http://127.0.0.1:8080/other")));
        assert!(!g.allows(&u("http://127.0.0.1:8081/")));
        let g = grant_from_setting("https://Feeds.Example.com/rss").unwrap();
        assert!(g.allows(&u("https://feeds.example.com/x")));
        assert!(!g.allows(&u("http://feeds.example.com/x")));
        assert!(grant_from_setting("*.tracker.example").is_some());
        assert!(grant_from_setting("not a host").is_none());
        let all = NetworkGrants::combine(&["a.example".into()], &["https://b.example/".into()]);
        assert!(all.allows(&u("https://a.example/")) && all.allows(&u("https://b.example/")));
    }

    #[test]
    fn public_addresses() {
        for ip in [
            "93.184.215.14",
            "2606:2800:21f:cb07:6820:80da:af6b:8b2c",
            "1.1.1.1",
        ] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:192.168.0.1",
            "2001:db8::1",
            "64:ff9b::7f00:1",
            "2002:7f00:1::",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
    }
}

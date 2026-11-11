//! Share locators: where other machines reach a storage root over the network.
//!
//! An admin attaches them to a root (`\\nas\torrents`, `smb://nas/torrents`,
//! `nfs://nas/export/torrents`), and a desktop client turns them into local paths to
//! suggest for its path mappings. They are hints, never trusted on their own: a mapping is
//! used only once the root's marker file reads correctly through it.

use std::fmt;

/// At most this many locators per root.
pub const MAX_LOCATORS: usize = 8;
/// The longest locator accepted, in bytes.
pub const MAX_LOCATOR_LEN: usize = 1024;

/// How a share is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareProtocol {
    /// SMB, written as a UNC path (`\\host\share\…`) or an `smb://` URL. Both name the same
    /// thing; the written form is kept.
    Smb,
    /// NFS, as an `nfs://host/export/…` URL.
    Nfs,
}

/// A parsed, checked share locator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareLocator {
    pub protocol: ShareProtocol,
    /// Written as a UNC path rather than a URL.
    pub unc: bool,
    /// The server, as written (a name, an IPv4 address, or an IPv6 address without
    /// brackets). Compared case-insensitively.
    pub host: String,
    /// A port other than the protocol's own, from a URL.
    pub port: Option<u16>,
    /// The path on the server: for SMB the share name first, then folders inside it; for
    /// NFS the export path's components. Never empty, never `.` or `..`.
    pub path: Vec<String>,
}

/// Characters that can't appear in a host name or path component here: separators,
/// Windows-reserved characters and controls.
fn bad_char(c: char) -> bool {
    c.is_control() || matches!(c, '/' | '\\' | '"' | '<' | '>' | '|' | '?' | '*')
}

fn check_component(c: &str) -> Result<(), String> {
    if c.is_empty() || c == "." || c == ".." {
        return Err("the path can't have empty, `.` or `..` parts".into());
    }
    if c.chars().any(|ch| bad_char(ch) || ch == ':') {
        return Err(format!("`{c}` has a character paths can't contain"));
    }
    Ok(())
}

fn check_host(h: &str) -> Result<(), String> {
    if h.is_empty() {
        return Err("the locator has no host".into());
    }
    if h.len() > 253
        || h.chars()
            .any(|c| bad_char(c) || c.is_whitespace() || c == '@')
    {
        return Err(format!("`{h}` isn't a host name or address"));
    }
    Ok(())
}

/// Undo `%XX` escapes in a URL path component. Invalid escapes and non-UTF-8 results are
/// refused rather than guessed at.
fn percent_decode(s: &str) -> Result<String, String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| format!("`{s}` has a broken %-escape"))?;
            out.push(hex);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| format!("`{s}` doesn't decode to text"))
}

/// Escape what a URL path component can't hold literally.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~!$&'()+,;=@".contains(&b) {
            out.push(char::from(b));
        } else {
            use fmt::Write as _;
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

impl ShareLocator {
    /// Parse a locator: `\\host\share[\folder…]` (or with `/`), `smb://host[:port]/share[/…]`
    /// or `nfs://host[:port]/export[/…]`. Credentials, queries and fragments are refused.
    pub fn parse(input: &str) -> Result<Self, String> {
        let s = input.trim();
        if s.len() > MAX_LOCATOR_LEN {
            return Err("the locator is too long".into());
        }
        if s.starts_with("\\\\") || s.starts_with("//") {
            let mut parts = s[2..].split(['\\', '/']).filter(|p| !p.is_empty());
            let host = parts.next().unwrap_or_default().to_owned();
            check_host(&host)?;
            if host.contains(':') {
                return Err("a UNC path has no port; use an smb:// URL for that".into());
            }
            let path: Vec<String> = parts.map(str::to_owned).collect();
            if path.is_empty() {
                return Err(format!("{s} names a server but no share (\\\\host\\share)"));
            }
            for c in &path {
                check_component(c)?;
            }
            return Ok(Self {
                protocol: ShareProtocol::Smb,
                unc: true,
                host,
                port: None,
                path,
            });
        }
        let (scheme, rest) = s
            .split_once("://")
            .ok_or_else(|| format!("{s} isn't a UNC path or an smb:// or nfs:// URL"))?;
        let protocol = match scheme.to_ascii_lowercase().as_str() {
            "smb" | "cifs" => ShareProtocol::Smb,
            "nfs" => ShareProtocol::Nfs,
            other => {
                return Err(format!(
                    "`{other}://` isn't supported; use a UNC path, smb:// or nfs://"
                ));
            }
        };
        if rest.contains(['?', '#']) {
            return Err("a locator can't have a query or fragment".into());
        }
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        if authority.contains('@') {
            return Err("don't put credentials in a locator; each client signs in itself".into());
        }
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (h, after) = v6
                .split_once(']')
                .ok_or_else(|| format!("{authority}: unclosed `[`"))?;
            let port = match after {
                "" => None,
                p => Some(p.strip_prefix(':').ok_or("junk after the address")?),
            };
            (h.to_owned(), port)
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h.to_owned(), Some(p)),
                None => (authority.to_owned(), None),
            }
        };
        if authority.starts_with('[') {
            if host.parse::<std::net::Ipv6Addr>().is_err() {
                return Err(format!("`{host}` isn't an IPv6 address"));
            }
        } else {
            if host.contains(':') {
                return Err(format!("{authority}: put an IPv6 address in brackets"));
            }
            check_host(&host)?;
        }
        let port = port
            .map(|p| {
                p.parse::<u16>()
                    .ok()
                    .filter(|p| *p != 0)
                    .ok_or_else(|| format!("`{p}` isn't a port"))
            })
            .transpose()?;
        let path = path
            .split('/')
            .filter(|c| !c.is_empty())
            .map(percent_decode)
            .collect::<Result<Vec<_>, _>>()?;
        if path.is_empty() {
            return Err(match protocol {
                ShareProtocol::Smb => format!("{s} names no share (smb://host/share)"),
                ShareProtocol::Nfs => format!("{s} names no export (nfs://host/export)"),
            });
        }
        for c in &path {
            check_component(c)?;
        }
        Ok(Self {
            protocol,
            unc: false,
            host,
            port,
            path,
        })
    }

    /// The same share, written as a UNC path (`\\host\share\…`), when it is SMB on the
    /// standard port: what Windows opens directly.
    pub fn unc_path(&self) -> Option<String> {
        if self.protocol != ShareProtocol::Smb || self.port.is_some() {
            return None;
        }
        Some(format!("\\\\{}\\{}", self.host, self.path.join("\\")))
    }
}

impl fmt::Display for ShareLocator {
    /// The normalised form: UNC with backslashes, URLs with a lowercase scheme, brackets
    /// around IPv6 hosts, and escaped path parts.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.unc {
            return write!(f, "\\\\{}\\{}", self.host, self.path.join("\\"));
        }
        let scheme = match self.protocol {
            ShareProtocol::Smb => "smb",
            ShareProtocol::Nfs => "nfs",
        };
        write!(f, "{scheme}://")?;
        if self.host.contains(':') {
            write!(f, "[{}]", self.host)?;
        } else {
            f.write_str(&self.host)?;
        }
        if let Some(p) = self.port {
            write!(f, ":{p}")?;
        }
        for c in &self.path {
            write!(f, "/{}", percent_encode(c))?;
        }
        Ok(())
    }
}

/// Check and normalise a root's locators: each parsed and written in its normal form,
/// duplicates dropped, at most [`MAX_LOCATORS`].
pub fn normalise_locators(input: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for l in input.iter().map(|l| l.trim()).filter(|l| !l.is_empty()) {
        let parsed = ShareLocator::parse(l)?.to_string();
        if !out.iter().any(|o| o.eq_ignore_ascii_case(&parsed)) {
            out.push(parsed);
        }
    }
    if out.len() > MAX_LOCATORS {
        return Err(format!(
            "a storage root takes at most {MAX_LOCATORS} locators"
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> ShareLocator {
        ShareLocator::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn unc_paths() {
        let l = ok(r"\\NAS\torrents\tv");
        assert_eq!(l.protocol, ShareProtocol::Smb);
        assert!(l.unc);
        assert_eq!(l.host, "NAS");
        assert_eq!(l.path, ["torrents", "tv"]);
        assert_eq!(l.to_string(), r"\\NAS\torrents\tv");
        // Forward slashes and doubled separators are the same path.
        assert_eq!(ok("//nas/torrents//tv/").to_string(), r"\\nas\torrents\tv");
        assert_eq!(l.unc_path().as_deref(), Some(r"\\NAS\torrents\tv"));
    }

    #[test]
    fn urls() {
        let l = ok("SMB://nas.lan/torrents/My%20Shows");
        assert_eq!((l.protocol, l.unc), (ShareProtocol::Smb, false));
        assert_eq!(l.path, ["torrents", "My Shows"]);
        assert_eq!(l.to_string(), "smb://nas.lan/torrents/My%20Shows");
        assert_eq!(
            l.unc_path().as_deref(),
            Some(r"\\nas.lan\torrents\My Shows")
        );

        let l = ok("nfs://10.0.0.5:2049/srv/torrents");
        assert_eq!(l.protocol, ShareProtocol::Nfs);
        assert_eq!(l.port, Some(2049));
        assert_eq!(l.path, ["srv", "torrents"]);
        assert_eq!(l.unc_path(), None);

        let l = ok("smb://[fd00::5]:445/t");
        assert_eq!(l.host, "fd00::5");
        assert_eq!(l.to_string(), "smb://[fd00::5]:445/t");
        assert_eq!(l.unc_path(), None, "a port can't be written in UNC");
        assert_eq!(ok("cifs://nas/t").to_string(), "smb://nas/t");
    }

    #[test]
    fn refusals() {
        for bad in [
            "",
            r"\\nas",
            r"\\nas\",
            r"\\nas:445\t",
            r"\\nas\t\..\etc",
            "smb://user:pw@nas/t",
            "smb://user@nas/t",
            "smb://nas",
            "smb://nas/",
            "nfs://nas/a/../b",
            "nfs://nas:0/a",
            "nfs://nas:99999/a",
            "smb://nas/t?x=1",
            "smb://nas/t#f",
            "smb://nas/a%2Fb",
            "smb://nas/a%zz",
            "sftp://nas/t",
            "/srv/torrents",
            r"C:\torrents",
            "smb://[fd00::5/t",
            "smb://fd00::5/t",
            "smb://[nas]/t",
        ] {
            assert!(
                ShareLocator::parse(bad).is_err(),
                "{bad:?} should be refused"
            );
        }
    }

    #[test]
    fn normalising_a_list() {
        let got = normalise_locators(&[
            r"//nas/t".into(),
            "  ".into(),
            r"\\NAS\T".into(),
            "smb://nas/t".into(),
        ])
        .unwrap();
        assert_eq!(got, [r"\\nas\t", "smb://nas/t"]);
        let many: Vec<String> = (0..=MAX_LOCATORS).map(|i| format!(r"\\nas\t{i}")).collect();
        assert!(normalise_locators(&many).is_err());
        assert!(normalise_locators(&["nope".into()]).is_err());
    }
}

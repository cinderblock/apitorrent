//! Share locators: where other machines reach a storage root over the network.
//!
//! An admin attaches them to a root (`\\nas\torrents`, `smb://nas/torrents`,
//! `nfs://nas/export/torrents`), and a desktop client turns them into local paths to
//! suggest for its path mappings. They are hints, never trusted on their own: a mapping is
//! used only once the root's marker file reads correctly through it.
//!
//! A root can also be served over the web (`https://files.example.com/downloads/`, an nginx
//! `autoindex` listing, say). The web UI links files to their URLs there; the first web
//! locator of a root is the one it uses.

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
    /// A web server listing the root's files, as an `http://` URL.
    Http,
    /// The same over TLS, as an `https://` URL.
    Https,
}

impl ShareProtocol {
    /// Served over HTTP(S): opened in a browser rather than mounted.
    pub fn is_web(self) -> bool {
        matches!(self, Self::Http | Self::Https)
    }
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
    /// NFS the export path's components; for the web, the URL path's (decoded). Empty only
    /// for the web (a root served at the top of a host); never `.` or `..`.
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
    /// `nfs://host[:port]/export[/…]` or `http(s)://host[:port][/folder…]`. Credentials,
    /// queries and fragments are refused: every signed-in user can read a root's locators.
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
            .ok_or_else(|| format!("{s} isn't a UNC path or an smb://, nfs:// or https:// URL"))?;
        let protocol = match scheme.to_ascii_lowercase().as_str() {
            "smb" | "cifs" => ShareProtocol::Smb,
            "nfs" => ShareProtocol::Nfs,
            "http" => ShareProtocol::Http,
            "https" => ShareProtocol::Https,
            other => {
                return Err(format!(
                    "`{other}://` isn't supported; use a UNC path, smb://, nfs:// or https://"
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
        match protocol {
            _ if !path.is_empty() => {}
            ShareProtocol::Smb => return Err(format!("{s} names no share (smb://host/share)")),
            ShareProtocol::Nfs => return Err(format!("{s} names no export (nfs://host/export)")),
            // A root served at the top of its host.
            ShareProtocol::Http | ShareProtocol::Https => {}
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

    /// The root's URL, ending in `/`, when it is served over the web: what a path below the
    /// root (each part escaped) is appended to.
    pub fn web_base(&self) -> Option<String> {
        self.protocol.is_web().then(|| self.to_string())
    }

    /// Where the root's marker file is served, when the root is served over the web.
    pub fn web_marker_url(&self) -> Option<String> {
        self.web_base()
            .map(|base| format!("{base}{}", percent_encode(crate::RootMarker::FILE_NAME)))
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
            ShareProtocol::Http => "http",
            ShareProtocol::Https => "https",
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
        if self.protocol.is_web() {
            // A directory: paths below it are appended, and a web server would redirect
            // the bare form to this one anyway.
            f.write_str("/")?;
        }
        Ok(())
    }
}

/// Check and normalise a root's locators: each parsed and written in its normal form,
/// duplicates dropped, at most [`MAX_LOCATORS`].
pub fn normalise_locators(input: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for l in input.iter().map(|l| l.trim()).filter(|l| !l.is_empty()) {
        let locator = ShareLocator::parse(l)?;
        let parsed = locator.to_string();
        // Web paths are case-sensitive; SMB and NFS hosts and SMB paths aren't.
        let web = locator.protocol.is_web();
        if !out.iter().any(|o| {
            if web {
                *o == parsed
            } else {
                o.eq_ignore_ascii_case(&parsed)
            }
        }) {
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
    fn web_urls() {
        let l = ok("HTTPS://files.example.com/path/to/downloads");
        assert_eq!(l.protocol, ShareProtocol::Https);
        assert_eq!(l.path, ["path", "to", "downloads"]);
        assert_eq!(
            l.to_string(),
            "https://files.example.com/path/to/downloads/"
        );
        assert_eq!(
            l.web_base().as_deref(),
            Some("https://files.example.com/path/to/downloads/")
        );
        assert_eq!(
            l.web_marker_url().as_deref(),
            Some("https://files.example.com/path/to/downloads/.apitorrent-root")
        );
        assert_eq!(l.unc_path(), None);

        // The top of a host, a port, escapes.
        let l = ok("http://10.0.0.5:8080");
        assert_eq!((l.protocol, l.path.len()), (ShareProtocol::Http, 0));
        assert_eq!(l.to_string(), "http://10.0.0.5:8080/");
        assert_eq!(ok("https://f.example/").to_string(), "https://f.example/");
        assert_eq!(
            ok("https://f.example/My%20Files/").to_string(),
            "https://f.example/My%20Files/"
        );
        assert_eq!(ok("smb://nas/t").web_base(), None);
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
            "ftp://nas/t",
            "https://user:pw@files.example.com/t/",
            "https://files.example.com/t/?token=1",
            "https://files.example.com/a/../b/",
            "https://",
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
        // Web paths differ by case.
        let got = normalise_locators(&[
            "https://f.example/T".into(),
            "https://f.example/t/".into(),
            "https://f.example/t".into(),
        ])
        .unwrap();
        assert_eq!(got, ["https://f.example/T/", "https://f.example/t/"]);
    }
}

//! Outgoing connections through a proxy: SOCKS4 (and SOCKS4a), SOCKS5 and HTTP `CONNECT`.
//!
//! A [`Proxy`] describes the server and how to use it. [`Proxy::connect`] opens a TCP
//! connection to a target through it and returns the plain stream once the proxy has
//! connected it, so whatever runs over it (BitTorrent, MSE, TLS) never knows a proxy is
//! there. [`Proxy::udp_associate`] opens a SOCKS5 UDP association, the only kind of proxy
//! that carries datagrams. [`Proxy::url`] gives the URL form HTTP clients take.
//!
//! The SOCKS handshakes are `tokio-socks`'s. HTTP `CONNECT` and the SOCKS5 UDP association
//! (which `tokio-socks` doesn't offer) are implemented here, each a few dozen lines of
//! RFC 7231 §4.3.6 and RFC 1928 §7.
//!
//! Names are resolved by the proxy when [`Proxy::remote_dns`] is set (SOCKS4a, SOCKS5 with
//! a domain address, or the name in the `CONNECT` line), and locally otherwise, so a
//! name never leaks to the local resolver when the user asked for it not to.
//!
//! With the `testing` feature, [`testing`] has small proxy servers for tests.

use std::fmt;
use std::io;
use std::net::SocketAddr;

use tokio::net::TcpStream;

mod http;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
mod udp;

pub use udp::UdpAssociation;

/// The proxy protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyKind {
    /// SOCKS4, or SOCKS4a when names are resolved by the proxy. IPv4 targets only, unless
    /// the target is a name the proxy resolves. The username, if any, is sent as the
    /// SOCKS4 user id; SOCKS4 has no passwords.
    Socks4,
    /// SOCKS5, with or without a username and password (RFC 1928, RFC 1929).
    Socks5,
    /// An HTTP proxy that supports `CONNECT`, with or without Basic authentication.
    Http,
}

impl ProxyKind {
    /// Whether the proxy can carry UDP: only SOCKS5 can.
    pub fn carries_udp(self) -> bool {
        matches!(self, ProxyKind::Socks5)
    }
}

/// A username and password for the proxy. `Debug` never shows the password.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// A proxy server and how to use it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proxy {
    pub kind: ProxyKind,
    /// The proxy's host name or IP address. Resolved locally: the proxy is how the rest
    /// of the traffic leaves, so it can't resolve its own name.
    pub host: String,
    pub port: u16,
    pub credentials: Option<Credentials>,
    /// Let the proxy resolve target names. Off resolves them locally and hands the proxy
    /// an address.
    pub remote_dns: bool,
}

/// Where to connect through the proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Addr(SocketAddr),
    /// A host name and port. A name that is an IP literal is treated as that address.
    Name(String, u16),
}

impl Target {
    /// `host:port`, with brackets around an IPv6 address.
    pub fn authority(&self) -> String {
        match self {
            Target::Addr(a) => a.to_string(),
            Target::Name(host, port) => format!("{host}:{port}"),
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.authority())
    }
}

impl From<SocketAddr> for Target {
    fn from(a: SocketAddr) -> Self {
        Target::Addr(a)
    }
}

impl Proxy {
    /// Open a TCP connection to `target` through the proxy. The returned stream is
    /// connected to the target: the proxy's handshake is over and nothing of it is left
    /// to read.
    pub async fn connect(&self, target: &Target) -> io::Result<TcpStream> {
        let target = self.prepare(target).await?;
        let proxy = (self.host.as_str(), self.port);
        match self.kind {
            ProxyKind::Socks4 => {
                let stream = match &self.credentials {
                    Some(c) => {
                        tokio_socks::tcp::Socks4Stream::connect_with_userid(
                            proxy,
                            socks_target(&target),
                            &c.username,
                        )
                        .await
                    }
                    None => {
                        tokio_socks::tcp::Socks4Stream::connect(proxy, socks_target(&target)).await
                    }
                }
                .map_err(socks_error)?;
                Ok(stream.into_inner())
            }
            ProxyKind::Socks5 => {
                let stream = match &self.credentials {
                    Some(c) => {
                        tokio_socks::tcp::Socks5Stream::connect_with_password(
                            proxy,
                            socks_target(&target),
                            &c.username,
                            &c.password,
                        )
                        .await
                    }
                    None => {
                        tokio_socks::tcp::Socks5Stream::connect(proxy, socks_target(&target)).await
                    }
                }
                .map_err(socks_error)?;
                Ok(stream.into_inner())
            }
            ProxyKind::Http => http::connect(self, &target).await,
        }
    }

    /// The target as the proxy should get it: a name the proxy may resolve stays a name,
    /// any other name is resolved here.
    async fn prepare(&self, target: &Target) -> io::Result<Target> {
        match target {
            Target::Addr(_) => Ok(target.clone()),
            Target::Name(host, port) => {
                if let Ok(ip) = host.trim_matches(['[', ']']).parse::<std::net::IpAddr>() {
                    return Ok(Target::Addr(SocketAddr::new(ip, *port)));
                }
                if self.remote_dns {
                    check_name(host)?;
                    return Ok(target.clone());
                }
                let addr = tokio::net::lookup_host((host.as_str(), *port))
                    .await?
                    .next()
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, format!("{host} has no address"))
                    })?;
                Ok(Target::Addr(addr))
            }
        }
    }

    /// The proxy as a URL, for HTTP clients that take one (reqwest): `socks4`, `socks4a`,
    /// `socks5`, `socks5h` (the `a` and `h` forms let the proxy resolve names) or `http`,
    /// with the credentials in the user info, percent-encoded.
    ///
    /// The URL holds the password; never log it.
    pub fn url(&self) -> Result<url::Url, url::ParseError> {
        let scheme = match (self.kind, self.remote_dns) {
            (ProxyKind::Socks4, false) => "socks4",
            (ProxyKind::Socks4, true) => "socks4a",
            (ProxyKind::Socks5, false) => "socks5",
            (ProxyKind::Socks5, true) => "socks5h",
            (ProxyKind::Http, _) => "http",
        };
        let host = if self.host.parse::<std::net::Ipv6Addr>().is_ok() {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let mut url = url::Url::parse(&format!("{scheme}://{host}:{}", self.port))?;
        if let Some(c) = &self.credentials {
            // Both only fail on URLs without a host, and this one has one.
            let _ = url.set_username(&c.username);
            if !c.password.is_empty() {
                let _ = url.set_password(Some(&c.password));
            }
        }
        Ok(url)
    }
}

fn socks_target(target: &Target) -> tokio_socks::TargetAddr<'_> {
    match target {
        Target::Addr(a) => tokio_socks::TargetAddr::Ip(*a),
        Target::Name(host, port) => tokio_socks::TargetAddr::Domain(host.as_str().into(), *port),
    }
}

/// A host name the proxy may be given: something a resolver could look up, and nothing
/// that could break out of a protocol line.
fn check_name(host: &str) -> io::Result<()> {
    let ok = !host.is_empty()
        && host.len() <= 255
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'));
    if ok {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{host:?} is not a host name"),
        ))
    }
}

/// A SOCKS failure as an I/O error, keeping the kind of an I/O failure underneath.
fn socks_error(e: tokio_socks::Error) -> io::Error {
    match e {
        tokio_socks::Error::Io(e) => e,
        e @ (tokio_socks::Error::PasswordAuthFailure(_)
        | tokio_socks::Error::NoAcceptableAuthMethods
        | tokio_socks::Error::AuthorizationRequired) => {
            io::Error::new(io::ErrorKind::PermissionDenied, format!("proxy: {e}"))
        }
        e @ tokio_socks::Error::ConnectionRefused => {
            io::Error::new(io::ErrorKind::ConnectionRefused, format!("proxy: {e}"))
        }
        e => io::Error::other(format!("proxy: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy(kind: ProxyKind, remote_dns: bool) -> Proxy {
        Proxy {
            kind,
            host: "proxy.example".into(),
            port: 1080,
            credentials: None,
            remote_dns,
        }
    }

    #[test]
    fn urls() {
        let cases = [
            (ProxyKind::Socks4, false, "socks4://proxy.example:1080"),
            (ProxyKind::Socks4, true, "socks4a://proxy.example:1080"),
            (ProxyKind::Socks5, false, "socks5://proxy.example:1080"),
            (ProxyKind::Socks5, true, "socks5h://proxy.example:1080"),
            (ProxyKind::Http, true, "http://proxy.example:1080/"),
        ];
        for (kind, dns, want) in cases {
            assert_eq!(proxy(kind, dns).url().unwrap().as_str(), want);
        }
    }

    #[test]
    fn url_encodes_credentials_and_brackets_ipv6() {
        let mut p = proxy(ProxyKind::Socks5, true);
        p.host = "::1".into();
        p.credentials = Some(Credentials {
            username: "me@home".into(),
            password: "p:ss/w rd".into(),
        });
        let url = p.url().unwrap();
        assert_eq!(url.host_str(), Some("[::1]"));
        assert_eq!(url.username(), "me%40home");
        assert_eq!(url.password(), Some("p%3Ass%2Fw%20rd"));
    }

    #[test]
    fn debug_hides_the_password() {
        let c = Credentials {
            username: "me".into(),
            password: "hunter2".into(),
        };
        let shown = format!("{c:?}");
        assert!(shown.contains("me") && !shown.contains("hunter2"));
    }

    #[tokio::test]
    async fn prepare_keeps_names_only_for_remote_dns() {
        let name = Target::Name("tracker.example".into(), 80);
        assert_eq!(
            proxy(ProxyKind::Socks5, true).prepare(&name).await.unwrap(),
            name
        );
        // An IP literal is an address either way, without a lookup.
        let literal = Target::Name("[::1]".into(), 80);
        assert_eq!(
            proxy(ProxyKind::Socks5, true)
                .prepare(&literal)
                .await
                .unwrap(),
            Target::Addr("[::1]:80".parse().unwrap())
        );
        let local = Target::Name("localhost".into(), 80);
        assert!(matches!(
            proxy(ProxyKind::Socks5, false).prepare(&local).await.unwrap(),
            Target::Addr(a) if a.ip().is_loopback()
        ));
    }

    #[tokio::test]
    async fn refuses_names_that_could_break_a_request_line() {
        let bad = Target::Name("evil\r\nX-Injected: 1".into(), 80);
        let err = proxy(ProxyKind::Http, true)
            .prepare(&bad)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}

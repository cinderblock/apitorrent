//! Small proxy servers for tests: SOCKS4/4a, SOCKS5 (`CONNECT` and UDP associations) and
//! HTTP `CONNECT`, on loopback. They log what they were asked to do, so a test can check
//! that traffic really went through the proxy. No limits, no hardening: tests only.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::task::JoinHandle;

use crate::udp::{decode_datagram, encode_addr, read_addr};
use crate::{Credentials, Proxy, ProxyKind, Target};

type Log = Arc<Mutex<Vec<String>>>;

/// A running test proxy. Stops when dropped.
#[derive(Debug)]
pub struct TestProxy {
    kind: ProxyKind,
    addr: SocketAddr,
    credentials: Option<Credentials>,
    log: Log,
    task: JoinHandle<()>,
}

impl Drop for TestProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl TestProxy {
    /// A SOCKS5 proxy; with `credentials`, it insists on them.
    pub async fn socks5(credentials: Option<(&str, &str)>) -> io::Result<Self> {
        Self::start(ProxyKind::Socks5, credentials).await
    }

    /// A SOCKS4 proxy (with SOCKS4a names). SOCKS4 has no passwords.
    pub async fn socks4() -> io::Result<Self> {
        Self::start(ProxyKind::Socks4, None).await
    }

    /// An HTTP `CONNECT` proxy; with `credentials`, it insists on Basic authentication.
    pub async fn http(credentials: Option<(&str, &str)>) -> io::Result<Self> {
        Self::start(ProxyKind::Http, credentials).await
    }

    async fn start(kind: ProxyKind, credentials: Option<(&str, &str)>) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = listener.local_addr()?;
        let credentials = credentials.map(|(u, p)| Credentials {
            username: u.to_owned(),
            password: p.to_owned(),
        });
        let log = Log::default();
        let task = {
            let (log, credentials) = (log.clone(), credentials.clone());
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let (log, credentials) = (log.clone(), credentials.clone());
                    tokio::spawn(async move {
                        let _ = match kind {
                            ProxyKind::Socks5 => socks5(stream, credentials, log).await,
                            ProxyKind::Socks4 => socks4(stream, log).await,
                            ProxyKind::Http => http(stream, credentials, log).await,
                        };
                    });
                }
            })
        };
        Ok(Self {
            kind,
            addr,
            credentials,
            log,
            task,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// A [`Proxy`] for this server, with its credentials, letting it resolve names.
    pub fn proxy(&self) -> Proxy {
        Proxy {
            kind: self.kind,
            host: self.addr.ip().to_string(),
            port: self.addr.port(),
            credentials: self.credentials.clone(),
            remote_dns: true,
        }
    }

    /// What the proxy was asked to do so far: `CONNECT <target>` for each tunnel and
    /// `UDP <target>` for each datagram relayed to a target.
    pub fn requests(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

fn record(log: &Log, entry: String) {
    log.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(entry);
}

async fn resolve(target: &Target) -> io::Result<SocketAddr> {
    resolve_for(target, false).await
}

/// Resolve a name to its first address, or its first IPv4 one with `v4_only` (the UDP
/// relay is IPv4).
async fn resolve_for(target: &Target, v4_only: bool) -> io::Result<SocketAddr> {
    match target {
        Target::Addr(a) => Ok(*a),
        Target::Name(host, port) => tokio::net::lookup_host((host.as_str(), *port))
            .await?
            .find(|a| !v4_only || a.is_ipv4())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no address")),
    }
}

async fn tunnel(mut client: TcpStream, target: &Target) -> io::Result<()> {
    let mut upstream = TcpStream::connect(resolve(target).await?).await?;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

async fn socks5(mut s: TcpStream, credentials: Option<Credentials>, log: Log) -> io::Result<()> {
    let mut head = [0u8; 2];
    s.read_exact(&mut head).await?;
    let mut methods = vec![0u8; usize::from(head[1])];
    s.read_exact(&mut methods).await?;
    if let Some(want) = &credentials {
        if !methods.contains(&2) {
            s.write_all(&[5, 0xff]).await?;
            return Ok(());
        }
        s.write_all(&[5, 2]).await?;
        let mut ver_len = [0u8; 2];
        s.read_exact(&mut ver_len).await?;
        let mut user = vec![0u8; usize::from(ver_len[1])];
        s.read_exact(&mut user).await?;
        let mut pass = vec![0u8; usize::from(s.read_u8().await?)];
        s.read_exact(&mut pass).await?;
        let ok = user == want.username.as_bytes() && pass == want.password.as_bytes();
        s.write_all(&[1, u8::from(!ok)]).await?;
        if !ok {
            return Ok(());
        }
    } else {
        s.write_all(&[5, 0]).await?;
    }
    let mut req = [0u8; 3];
    s.read_exact(&mut req).await?;
    let target = read_addr(&mut s).await?;
    let mut reply = vec![5, 0, 0];
    match req[1] {
        1 => {
            record(&log, format!("CONNECT {target}"));
            let upstream = match resolve(&target).await {
                Ok(a) => TcpStream::connect(a).await,
                Err(e) => Err(e),
            };
            let Ok(mut upstream) = upstream else {
                s.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
                return Ok(());
            };
            encode_addr(&mut reply, &Target::Addr(upstream.local_addr()?))?;
            s.write_all(&reply).await?;
            let _ = tokio::io::copy_bidirectional(&mut s, &mut upstream).await;
            Ok(())
        }
        3 => {
            let relay = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
            encode_addr(&mut reply, &Target::Addr(relay.local_addr()?))?;
            s.write_all(&reply).await?;
            let mut client: Option<SocketAddr> = None;
            let mut buf = vec![0u8; 65_535];
            let mut probe = [0u8; 1];
            loop {
                tokio::select! {
                    // The association ends when the control connection closes.
                    r = s.read(&mut probe) => if !matches!(r, Ok(n) if n > 0) { return Ok(()) },
                    r = relay.recv_from(&mut buf) => {
                        let (n, from) = r?;
                        if client.is_none_or(|c| c == from) {
                            client = Some(from);
                            let Some((to, payload)) = decode_datagram(&buf[..n]) else {
                                continue;
                            };
                            record(&log, format!("UDP {to}"));
                            if let Ok(to) = resolve_for(&to, true).await {
                                let _ = relay.send_to(payload, to).await;
                            }
                        } else if let Some(c) = client {
                            let mut out = vec![0, 0, 0];
                            encode_addr(&mut out, &Target::Addr(from))?;
                            out.extend_from_slice(&buf[..n]);
                            let _ = relay.send_to(&out, c).await;
                        }
                    }
                }
            }
        }
        _ => {
            s.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
            Ok(())
        }
    }
}

async fn read_until_nul(s: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        match s.read_u8().await? {
            0 => return Ok(out),
            b => out.push(b),
        }
    }
}

async fn socks4(mut s: TcpStream, log: Log) -> io::Result<()> {
    let mut head = [0u8; 8];
    s.read_exact(&mut head).await?;
    let port = u16::from_be_bytes([head[2], head[3]]);
    let ip = Ipv4Addr::new(head[4], head[5], head[6], head[7]);
    let user = String::from_utf8_lossy(&read_until_nul(&mut s).await?).into_owned();
    // SOCKS4a: 0.0.0.x (x != 0) means a name follows.
    let target = if ip.octets()[..3] == [0, 0, 0] && ip.octets()[3] != 0 {
        let name = String::from_utf8_lossy(&read_until_nul(&mut s).await?).into_owned();
        Target::Name(name, port)
    } else {
        Target::Addr(SocketAddr::from((ip, port)))
    };
    record(&log, format!("CONNECT {target} user={user}"));
    let upstream = match resolve(&target).await {
        Ok(a) => TcpStream::connect(a).await,
        Err(e) => Err(e),
    };
    let Ok(mut upstream) = upstream else {
        s.write_all(&[0, 0x5b, 0, 0, 0, 0, 0, 0]).await?;
        return Ok(());
    };
    s.write_all(&[0, 0x5a, 0, 0, 0, 0, 0, 0]).await?;
    let _ = tokio::io::copy_bidirectional(&mut s, &mut upstream).await;
    Ok(())
}

async fn http(mut s: TcpStream, credentials: Option<Credentials>, log: Log) -> io::Result<()> {
    use base64::Engine as _;
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(s.read_u8().await?);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let mut lines = head.split("\r\n");
    let request = lines.next().unwrap_or_default();
    let mut parts = request.split(' ');
    let (Some("CONNECT"), Some(authority)) = (parts.next(), parts.next()) else {
        s.write_all(b"HTTP/1.1 405 Method Not Allowed\r\n\r\n")
            .await?;
        return Ok(());
    };
    if let Some(c) = &credentials {
        let want = format!(
            "Proxy-Authorization: Basic {}",
            base64::engine::general_purpose::STANDARD
                .encode(format!("{}:{}", c.username, c.password))
        );
        if !lines.any(|l| l == want) {
            s.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
                .await?;
            return Ok(());
        }
    }
    let target = if let Ok(a) = authority.parse::<SocketAddr>() {
        Target::Addr(a)
    } else {
        let (host, port) = authority.rsplit_once(':').unwrap_or((authority, "80"));
        Target::Name(host.to_owned(), port.parse().unwrap_or(80))
    };
    record(&log, format!("CONNECT {target}"));
    s.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await?;
    tunnel(s, &target).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProxyKind;

    /// The address `localhost` resolves to first, which is where the proxies (and the
    /// local resolution in `Proxy::connect`) will go.
    async fn localhost() -> std::net::IpAddr {
        tokio::net::lookup_host(("localhost", 0))
            .await
            .unwrap()
            .next()
            .unwrap()
            .ip()
    }

    /// A TCP server that answers each connection's first line with `echo: <line>`.
    async fn echo() -> SocketAddr {
        let listener = TcpListener::bind((localhost().await, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 64];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let mut out = b"echo: ".to_vec();
                    out.extend_from_slice(&buf[..n]);
                    let _ = s.write_all(&out).await;
                });
            }
        });
        addr
    }

    async fn round_trip(proxy: &Proxy, target: &Target) -> io::Result<String> {
        let mut s = proxy.connect(target).await?;
        s.write_all(b"hi").await?;
        let mut out = String::new();
        s.read_to_string(&mut out).await?;
        Ok(out)
    }

    #[tokio::test]
    async fn every_kind_connects() {
        let target = echo().await;
        let servers = [
            TestProxy::socks5(None).await.unwrap(),
            TestProxy::socks5(Some(("me", "secret"))).await.unwrap(),
            TestProxy::socks4().await.unwrap(),
            TestProxy::http(None).await.unwrap(),
            TestProxy::http(Some(("me", "secret"))).await.unwrap(),
        ];
        for server in &servers {
            let proxy = server.proxy();
            let mut targets = vec![Target::Name("localhost".into(), target.port())];
            // SOCKS4 carries only IPv4 addresses (names go as SOCKS4a).
            if proxy.kind != ProxyKind::Socks4 || target.is_ipv4() {
                targets.insert(0, Target::Addr(target));
            } else {
                let err = proxy.connect(&Target::Addr(target)).await.unwrap_err();
                assert!(err.to_string().contains("not supported"), "{err}");
            }
            for t in &targets {
                assert_eq!(
                    round_trip(&proxy, t).await.unwrap(),
                    "echo: hi",
                    "{:?} to {t}",
                    proxy.kind
                );
            }
            let log = server.requests();
            assert_eq!(log.len(), targets.len(), "{log:?}");
            assert!(
                log.last()
                    .unwrap()
                    .starts_with(&format!("CONNECT localhost:{}", target.port())),
                "the proxy resolved the name: {log:?}"
            );
        }
    }

    #[tokio::test]
    async fn local_dns_hands_the_proxy_an_address() {
        let target = echo().await;
        let server = TestProxy::socks5(None).await.unwrap();
        let mut proxy = server.proxy();
        proxy.remote_dns = false;
        let name = Target::Name("localhost".into(), target.port());
        assert_eq!(round_trip(&proxy, &name).await.unwrap(), "echo: hi");
        assert!(!server.requests()[0].contains("localhost"));
    }

    #[tokio::test]
    async fn wrong_credentials_are_refused() {
        let target = Target::Addr(echo().await);
        for server in [
            TestProxy::socks5(Some(("me", "secret"))).await.unwrap(),
            TestProxy::http(Some(("me", "secret"))).await.unwrap(),
        ] {
            let mut proxy = server.proxy();
            proxy.credentials.as_mut().unwrap().password = "wrong".into();
            let err = proxy.connect(&target).await.unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
            proxy.credentials = None;
            let err = proxy.connect(&target).await.unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
            assert!(server.requests().is_empty());
        }
    }

    #[tokio::test]
    async fn udp_through_socks5() {
        let echo = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let echo_addr = echo.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            while let Ok((n, from)) = echo.recv_from(&mut buf).await {
                let _ = echo.send_to(&buf[..n], from).await;
            }
        });
        for creds in [None, Some(("me", "secret"))] {
            let server = TestProxy::socks5(creds).await.unwrap();
            let assoc = server.proxy().udp_associate().await.unwrap();
            for target in [
                Target::Addr(echo_addr),
                Target::Name("localhost".into(), echo_addr.port()),
            ] {
                assoc.send_to(b"ping", &target).await.unwrap();
                let mut buf = [0u8; 16];
                let (n, from) = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    assoc.recv_from(&mut buf),
                )
                .await
                .expect("an answer")
                .unwrap();
                assert_eq!(&buf[..n], b"ping");
                assert_eq!(from, Target::Addr(echo_addr));
            }
            assert_eq!(
                server.requests(),
                [
                    format!("UDP {echo_addr}"),
                    format!("UDP localhost:{}", echo_addr.port())
                ]
            );
        }
    }

    #[tokio::test]
    async fn udp_needs_socks5() {
        let server = TestProxy::http(None).await.unwrap();
        let err = server.proxy().udp_associate().await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        assert!(!ProxyKind::Http.carries_udp() && ProxyKind::Socks5.carries_udp());
    }
}

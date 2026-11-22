//! HTTP `CONNECT` (RFC 7231 §4.3.6): ask the proxy for a tunnel, then use the connection
//! as the tunnel.

use std::io;

use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::{Proxy, Target};

/// The longest response head accepted from the proxy.
const MAX_HEAD: usize = 8 * 1024;

pub(crate) async fn connect(proxy: &Proxy, target: &Target) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect((proxy.host.as_str(), proxy.port)).await?;
    stream.write_all(&request(proxy, target)).await?;
    stream.flush().await?;
    let head = read_head(&mut stream).await?;
    check_status(&head)?;
    Ok(stream)
}

fn request(proxy: &Proxy, target: &Target) -> Vec<u8> {
    let authority = target.authority();
    let mut req = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some(c) = &proxy.credentials {
        let token = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", c.username, c.password));
        req.push_str("Proxy-Authorization: Basic ");
        req.push_str(&token);
        req.push_str("\r\n");
    }
    req.push_str("\r\n");
    req.into_bytes()
}

/// Read the response head, up to and including the blank line, one byte at a time: the
/// tunnel's first bytes may follow at once, and they belong to the caller.
async fn read_head(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut head = Vec::with_capacity(128);
    loop {
        let b = stream.read_u8().await.map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "proxy: closed the connection before answering",
                )
            } else {
                e
            }
        })?;
        head.push(b);
        if head.ends_with(b"\r\n\r\n") {
            return Ok(head);
        }
        if head.len() > MAX_HEAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "proxy: response head too long",
            ));
        }
    }
}

fn check_status(head: &[u8]) -> io::Result<()> {
    let line = head
        .split(|&b| b == b'\r')
        .next()
        .map(String::from_utf8_lossy)
        .unwrap_or_default();
    let mut parts = line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    let code: u16 = parts.next().and_then(|c| c.parse().ok()).unwrap_or(0);
    if !version.starts_with("HTTP/1.") || code == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("proxy: not an HTTP response: {line:?}"),
        ));
    }
    match code {
        200..=299 => Ok(()),
        407 => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("proxy: authentication required or refused ({line})"),
        )),
        _ => Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("proxy: {line}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Credentials, ProxyKind};

    #[test]
    fn request_lines() {
        let mut proxy = Proxy {
            kind: ProxyKind::Http,
            host: "p".into(),
            port: 3128,
            credentials: None,
            remote_dns: true,
        };
        let v6 = Target::Addr("[2001:db8::1]:6881".parse().unwrap());
        assert_eq!(
            request(&proxy, &v6),
            b"CONNECT [2001:db8::1]:6881 HTTP/1.1\r\nHost: [2001:db8::1]:6881\r\n\r\n"
        );
        proxy.credentials = Some(Credentials {
            username: "Aladdin".into(),
            password: "open sesame".into(),
        });
        let name = Target::Name("t.example".into(), 443);
        assert_eq!(
            String::from_utf8(request(&proxy, &name)).unwrap(),
            "CONNECT t.example:443 HTTP/1.1\r\nHost: t.example:443\r\n\
             Proxy-Authorization: Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==\r\n\r\n"
        );
    }

    #[test]
    fn statuses() {
        assert!(check_status(b"HTTP/1.1 200 Connection established\r\n\r\n").is_ok());
        assert!(check_status(b"HTTP/1.0 200 OK\r\n\r\n").is_ok());
        assert_eq!(
            check_status(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            check_status(b"HTTP/1.1 403 Forbidden\r\n\r\n")
                .unwrap_err()
                .kind(),
            io::ErrorKind::ConnectionRefused
        );
        assert_eq!(
            check_status(b"SSH-2.0-OpenSSH\r\n\r\n").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}

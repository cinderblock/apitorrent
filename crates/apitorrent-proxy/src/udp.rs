//! SOCKS5 UDP associations (RFC 1928 §7): datagrams to and from any target, relayed by
//! the proxy. The association lasts as long as its TCP control connection.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

use crate::{Proxy, ProxyKind, Target};

const VERSION: u8 = 5;
const NO_AUTH: u8 = 0x00;
const PASSWORD: u8 = 0x02;
const NO_ACCEPTABLE: u8 = 0xff;
const UDP_ASSOCIATE: u8 = 0x03;
pub(crate) const ATYP_V4: u8 = 0x01;
pub(crate) const ATYP_NAME: u8 = 0x03;
pub(crate) const ATYP_V6: u8 = 0x04;
/// The largest datagram relayed: the SOCKS header and a full UDP payload.
const MAX_DATAGRAM: usize = 65_535;

/// A UDP association through a SOCKS5 proxy.
#[derive(Debug)]
pub struct UdpAssociation {
    /// Closing it ends the association, so it is held for as long as this lives.
    _control: TcpStream,
    /// Connected to the proxy's relay address, so only the relay's datagrams arrive.
    socket: UdpSocket,
}

impl Proxy {
    /// Open a UDP association. Only SOCKS5 proxies carry UDP (see
    /// [`ProxyKind::carries_udp`]); others fail with [`io::ErrorKind::Unsupported`].
    pub async fn udp_associate(&self) -> io::Result<UdpAssociation> {
        if self.kind != ProxyKind::Socks5 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "only SOCKS5 proxies carry UDP",
            ));
        }
        let mut control = TcpStream::connect((self.host.as_str(), self.port)).await?;
        self.authenticate(&mut control).await?;
        // The client's address is not known before the first datagram (NAT may change
        // it), so ask for an association from any address, as RFC 1928 allows.
        let proxy_addr = control.peer_addr()?;
        let any = if proxy_addr.is_ipv4() {
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
        } else {
            SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
        };
        let mut req = vec![VERSION, UDP_ASSOCIATE, 0];
        encode_addr(&mut req, &Target::Addr(any))?;
        control.write_all(&req).await?;
        let mut reply = [0u8; 3];
        control.read_exact(&mut reply).await?;
        if reply[0] != VERSION {
            return Err(invalid("proxy: not a SOCKS5 reply"));
        }
        if reply[1] != 0 {
            return Err(io::Error::other(format!(
                "proxy: refused the UDP association (code {})",
                reply[1]
            )));
        }
        let relay = match read_addr(&mut control).await? {
            Target::Addr(a) => a,
            Target::Name(..) => return Err(invalid("proxy: gave a name as its relay address")),
        };
        // A relay on the unspecified address means "the address you reached me on".
        let relay = if relay.ip().is_unspecified() {
            SocketAddr::new(proxy_addr.ip(), relay.port())
        } else {
            relay
        };
        let socket = UdpSocket::bind(any).await?;
        socket.connect(relay).await?;
        Ok(UdpAssociation {
            _control: control,
            socket,
        })
    }

    /// The SOCKS5 greeting and, when the proxy asks for it, username and password
    /// (RFC 1929).
    async fn authenticate(&self, control: &mut TcpStream) -> io::Result<()> {
        let greeting: &[u8] = if self.credentials.is_some() {
            &[VERSION, 2, NO_AUTH, PASSWORD]
        } else {
            &[VERSION, 1, NO_AUTH]
        };
        control.write_all(greeting).await?;
        let mut chosen = [0u8; 2];
        control.read_exact(&mut chosen).await?;
        if chosen[0] != VERSION {
            return Err(invalid("proxy: not a SOCKS5 server"));
        }
        match (chosen[1], &self.credentials) {
            (NO_AUTH, _) => Ok(()),
            (PASSWORD, Some(c)) => {
                let (user, pass) = (c.username.as_bytes(), c.password.as_bytes());
                let (Ok(ulen), Ok(plen)) = (u8::try_from(user.len()), u8::try_from(pass.len()))
                else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "proxy: the username and password must each be at most 255 bytes",
                    ));
                };
                let mut msg = vec![1, ulen];
                msg.extend_from_slice(user);
                msg.push(plen);
                msg.extend_from_slice(pass);
                control.write_all(&msg).await?;
                let mut status = [0u8; 2];
                control.read_exact(&mut status).await?;
                if status[1] == 0 {
                    Ok(())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "proxy: refused the username and password",
                    ))
                }
            }
            (NO_ACCEPTABLE | PASSWORD, _) => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "proxy: wants a username and password",
            )),
            (m, _) => Err(invalid(&format!("proxy: chose unknown method {m}"))),
        }
    }
}

impl UdpAssociation {
    /// Send `data` to `target` through the relay.
    pub async fn send_to(&self, data: &[u8], target: &Target) -> io::Result<()> {
        let mut packet = Vec::with_capacity(data.len() + 22);
        packet.extend_from_slice(&[0, 0, 0]);
        encode_addr(&mut packet, target)?;
        packet.extend_from_slice(data);
        self.socket.send(&packet).await?;
        Ok(())
    }

    /// Receive the next datagram into `buf`: its length (cut to `buf`), and where it came
    /// from. Fragments (which nobody sends) and malformed datagrams are skipped.
    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, Target)> {
        let mut packet = vec![0u8; MAX_DATAGRAM];
        loop {
            let n = self.socket.recv(&mut packet).await?;
            if let Some((from, payload)) = decode_datagram(&packet[..n]) {
                let len = payload.len().min(buf.len());
                buf[..len].copy_from_slice(&payload[..len]);
                return Ok((len, from));
            }
        }
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_owned())
}

/// Append a SOCKS5 address (`ATYP`, address, port).
pub(crate) fn encode_addr(out: &mut Vec<u8>, target: &Target) -> io::Result<()> {
    match target {
        Target::Addr(SocketAddr::V4(a)) => {
            out.push(ATYP_V4);
            out.extend_from_slice(&a.ip().octets());
            out.extend_from_slice(&a.port().to_be_bytes());
        }
        Target::Addr(SocketAddr::V6(a)) => {
            out.push(ATYP_V6);
            out.extend_from_slice(&a.ip().octets());
            out.extend_from_slice(&a.port().to_be_bytes());
        }
        Target::Name(host, port) => {
            let len = u8::try_from(host.len())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "host name too long"))?;
            out.push(ATYP_NAME);
            out.push(len);
            out.extend_from_slice(host.as_bytes());
            out.extend_from_slice(&port.to_be_bytes());
        }
    }
    Ok(())
}

/// Read a SOCKS5 address from a stream.
pub(crate) async fn read_addr(stream: &mut TcpStream) -> io::Result<Target> {
    let atyp = stream.read_u8().await?;
    let ip = match atyp {
        ATYP_V4 => {
            let mut b = [0u8; 4];
            stream.read_exact(&mut b).await?;
            IpAddr::from(b)
        }
        ATYP_V6 => {
            let mut b = [0u8; 16];
            stream.read_exact(&mut b).await?;
            IpAddr::from(b)
        }
        ATYP_NAME => {
            let len = stream.read_u8().await?;
            let mut name = vec![0u8; usize::from(len)];
            stream.read_exact(&mut name).await?;
            let port = stream.read_u16().await?;
            let name = String::from_utf8(name).map_err(|_| invalid("proxy: bad host name"))?;
            return Ok(Target::Name(name, port));
        }
        _ => return Err(invalid("proxy: unknown address type")),
    };
    let port = stream.read_u16().await?;
    Ok(Target::Addr(SocketAddr::new(ip, port)))
}

/// Split a relayed datagram into its source address and payload. `None` for fragments
/// and anything malformed.
pub(crate) fn decode_datagram(packet: &[u8]) -> Option<(Target, &[u8])> {
    let (&[0, 0, frag, atyp], rest) = packet.split_first_chunk::<4>()? else {
        return None;
    };
    if frag != 0 {
        return None;
    }
    let (target, rest) = match atyp {
        ATYP_V4 => {
            let (ip, rest) = rest.split_first_chunk::<4>()?;
            let (port, rest) = rest.split_first_chunk::<2>()?;
            let a = SocketAddr::new(IpAddr::from(*ip), u16::from_be_bytes(*port));
            (Target::Addr(a), rest)
        }
        ATYP_V6 => {
            let (ip, rest) = rest.split_first_chunk::<16>()?;
            let (port, rest) = rest.split_first_chunk::<2>()?;
            let a = SocketAddr::new(IpAddr::from(*ip), u16::from_be_bytes(*port));
            (Target::Addr(a), rest)
        }
        ATYP_NAME => {
            let (&len, rest) = rest.split_first()?;
            let len = usize::from(len);
            let name = std::str::from_utf8(rest.get(..len)?).ok()?.to_owned();
            let (port, rest) = rest.get(len..)?.split_first_chunk::<2>()?;
            (Target::Name(name, u16::from_be_bytes(*port)), rest)
        }
        _ => return None,
    };
    Some((target, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn datagram(target: &Target, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0, 0, 0];
        encode_addr(&mut p, target).unwrap();
        p.extend_from_slice(payload);
        p
    }

    #[test]
    fn datagrams_round_trip() {
        for t in [
            Target::Addr("192.0.2.1:6969".parse().unwrap()),
            Target::Addr("[2001:db8::2]:1337".parse().unwrap()),
            Target::Name("tracker.example".into(), 80),
        ] {
            let p = datagram(&t, b"hello");
            assert_eq!(decode_datagram(&p), Some((t, &b"hello"[..])));
        }
    }

    #[test]
    fn skips_fragments_and_junk() {
        let mut p = datagram(&Target::Addr("192.0.2.1:1".parse().unwrap()), b"x");
        p[2] = 1;
        assert_eq!(decode_datagram(&p), None);
        assert_eq!(decode_datagram(&[0, 0, 0, ATYP_V4, 1, 2]), None);
        assert_eq!(decode_datagram(&[0, 0, 0, 9]), None);
        assert_eq!(decode_datagram(&[0, 0, 0, ATYP_NAME, 200, b'a']), None);
        assert_eq!(decode_datagram(&[]), None);
    }
}

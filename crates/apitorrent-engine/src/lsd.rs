//! Local service discovery (BEP 14): finding peers on the local network by multicast.
//!
//! Every few minutes each public torrent is announced to the LSD multicast group, and
//! announces from others are fed to the matching torrents. Private torrents take no part
//! (BEP 27).
//!
//! An announce looks like an HTTP request:
//!
//! ```text
//! BT-SEARCH * HTTP/1.1\r\n
//! Host: 239.192.152.143:6771\r\n
//! Port: 6881\r\n
//! Infohash: <40 hex digits>\r\n          (one or more)
//! cookie: <ours, to recognise our own>\r\n
//! \r\n
//! \r\n
//! ```

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;

use apitorrent_metainfo::Sha1Hash;

use crate::session::Shared;

pub const MULTICAST_V4: Ipv4Addr = Ipv4Addr::new(239, 192, 152, 143);
pub const PORT: u16 = 6771;
/// Announces from one host are rate-limited by peers, so batch info-hashes per message.
const HASHES_PER_MESSAGE: usize = 8;
/// How often each torrent is announced.
const ANNOUNCE_EVERY: Duration = Duration::from_secs(5 * 60);
/// A datagram larger than this isn't an LSD announce.
const MAX_DATAGRAM: usize = 1400;

/// Where LSD listens and sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LsdConfig {
    /// The socket to receive on.
    pub bind: SocketAddr,
    /// Where announces go: the multicast group, normally.
    pub target: SocketAddr,
    /// Join this multicast group on all interfaces. `None` for plain unicast (tests).
    pub group: Option<Ipv4Addr>,
}

impl Default for LsdConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([0, 0, 0, 0], PORT)),
            target: SocketAddr::from((MULTICAST_V4, PORT)),
            group: Some(MULTICAST_V4),
        }
    }
}

/// A parsed announce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announce {
    pub port: u16,
    pub info_hashes: Vec<Sha1Hash>,
    pub cookie: Option<String>,
}

/// Build an announce message.
pub fn encode(host: SocketAddr, port: u16, hashes: &[Sha1Hash], cookie: &str) -> Vec<u8> {
    let mut s = format!("BT-SEARCH * HTTP/1.1\r\nHost: {host}\r\nPort: {port}\r\n");
    for h in hashes {
        s.push_str("Infohash: ");
        s.push_str(&h.to_hex());
        s.push_str("\r\n");
    }
    s.push_str("cookie: ");
    s.push_str(cookie);
    s.push_str("\r\n\r\n\r\n");
    s.into_bytes()
}

/// Parse an announce. Header names are case-insensitive; unknown headers are ignored.
pub fn decode(data: &[u8]) -> Option<Announce> {
    if data.len() > MAX_DATAGRAM {
        return None;
    }
    let text = std::str::from_utf8(data).ok()?;
    let mut lines = text.split("\r\n");
    if lines.next()?.trim() != "BT-SEARCH * HTTP/1.1" {
        return None;
    }
    let mut port = None;
    let mut info_hashes = Vec::new();
    let mut cookie = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "port" => port = value.parse::<u16>().ok().filter(|&p| p != 0),
            "infohash" => {
                if let Ok(h) = value.parse::<Sha1Hash>()
                    && info_hashes.len() < 64
                {
                    info_hashes.push(h);
                }
            }
            "cookie" => cookie = Some(value.to_owned()),
            _ => {}
        }
    }
    (!info_hashes.is_empty()).then_some(Announce {
        port: port?,
        info_hashes,
        cookie,
    })
}

fn bind(config: &LsdConfig) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let domain = if config.bind.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    // Other clients on this machine listen on the same port.
    socket.set_reuse_address(true)?;
    socket.bind(&config.bind.into())?;
    if let Some(group) = config.group {
        socket.join_multicast_v4(&group, &Ipv4Addr::UNSPECIFIED)?;
        socket.set_multicast_loop_v4(true)?;
        socket.set_multicast_ttl_v4(1)?;
    }
    socket.set_nonblocking(true)?;
    UdpSocket::from_std(socket.into())
}

/// Start the LSD service. Runs until the session's shared state is dropped.
pub(crate) fn start(shared: &Arc<Shared>, config: LsdConfig) -> std::io::Result<SocketAddr> {
    let socket = Arc::new(bind(&config)?);
    let local = socket.local_addr()?;
    let cookie = format!("{:08x}", rand::random::<u32>());
    let weak = Arc::downgrade(shared);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(ANNOUNCE_EVERY);
        let mut buf = vec![0u8; MAX_DATAGRAM + 1];
        let mut announced: HashSet<Sha1Hash> = HashSet::new();
        // New torrents are announced within this, rather than waiting a full interval.
        let mut fresh = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let Some(shared) = weak.upgrade() else { break };
                    announced = shared.public_hashes();
                    send(&socket, &config, &shared, &announced, &cookie).await;
                }
                _ = fresh.tick() => {
                    let Some(shared) = weak.upgrade() else { break };
                    let now = shared.public_hashes();
                    let new: HashSet<Sha1Hash> = now.difference(&announced).copied().collect();
                    if !new.is_empty() {
                        send(&socket, &config, &shared, &new, &cookie).await;
                    }
                    announced = now;
                }
                received = socket.recv_from(&mut buf) => {
                    let Ok((n, from)) = received else { continue };
                    let Some(shared) = weak.upgrade() else { break };
                    let Some(a) = decode(&buf[..n]) else { continue };
                    if a.cookie.as_deref() == Some(cookie.as_str()) {
                        continue; // our own announce, looped back
                    }
                    let peer = SocketAddr::new(canonical(from.ip()), a.port);
                    for h in a.info_hashes {
                        shared.discovered(&h, vec![peer]).await;
                    }
                }
            }
        }
    });
    Ok(local)
}

async fn send(
    socket: &UdpSocket,
    config: &LsdConfig,
    shared: &Shared,
    hashes: &HashSet<Sha1Hash>,
    cookie: &str,
) {
    let hashes: Vec<Sha1Hash> = hashes.iter().copied().collect();
    for chunk in hashes.chunks(HASHES_PER_MESSAGE) {
        let msg = encode(config.target, shared.listen_port(), chunk, cookie);
        if let Err(e) = socket.send_to(&msg, config.target).await {
            tracing::debug!(error = %e, "LSD announce failed");
        }
    }
}

fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 @ IpAddr::V4(_) => v4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let h1 = Sha1Hash::digest(b"one");
        let h2 = Sha1Hash::digest(b"two");
        let msg = encode(
            SocketAddr::from((MULTICAST_V4, PORT)),
            6881,
            &[h1, h2],
            "abc123",
        );
        let text = String::from_utf8(msg.clone()).unwrap();
        assert!(text.starts_with("BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\n"));
        assert!(text.ends_with("\r\n\r\n\r\n"));
        let a = decode(&msg).unwrap();
        assert_eq!(a.port, 6881);
        assert_eq!(a.info_hashes, [h1, h2]);
        assert_eq!(a.cookie.as_deref(), Some("abc123"));
    }

    #[test]
    fn tolerant_of_case_and_order_strict_about_the_rest() {
        let h = Sha1Hash::digest(b"x").to_hex();
        let msg =
            format!("BT-SEARCH * HTTP/1.1\r\nINFOHASH: {h}\r\nhost: x\r\nport: 51413\r\n\r\n\r\n");
        let a = decode(msg.as_bytes()).unwrap();
        assert_eq!(a.port, 51413);
        assert_eq!(a.cookie, None);
        assert!(decode(b"GET / HTTP/1.1\r\n\r\n").is_none());
        assert!(
            decode(format!("BT-SEARCH * HTTP/1.1\r\nPort: 0\r\nInfohash: {h}\r\n").as_bytes())
                .is_none()
        );
        assert!(
            decode(b"BT-SEARCH * HTTP/1.1\r\nPort: 1\r\n\r\n").is_none(),
            "no info-hash"
        );
        assert!(decode(&[0xff; 2000]).is_none());
    }
}

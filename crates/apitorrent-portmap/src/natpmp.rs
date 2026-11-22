//! NAT-PMP (RFC 6886): ask the gateway for its external address and to forward a port.
//!
//! Requests go to the gateway's UDP port 5351. A request is retried after 250 ms, then
//! twice that, and so on ([`TRIES`] times), as the RFC asks.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use tokio::net::UdpSocket;

use crate::Protocol;

/// The gateway's NAT-PMP port.
pub const PORT: u16 = 5351;
/// The RFC allows nine tries (about a minute); a router that hasn't answered after a few
/// seconds doesn't speak NAT-PMP, and UPnP is tried next.
const TRIES: u32 = 4;
const FIRST_WAIT: Duration = Duration::from_millis(250);

const VERSION: u8 = 0;
const OP_EXTERNAL: u8 = 0;
const OP_UDP: u8 = 1;
const OP_TCP: u8 = 2;
/// Responses carry the request's opcode plus 128.
const RESPONSE: u8 = 128;

#[derive(Debug, thiserror::Error)]
pub enum NatPmpError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("no answer from {0}")]
    NoAnswer(SocketAddr),
    #[error("the gateway refused: {0}")]
    Refused(&'static str),
    #[error("malformed answer")]
    Malformed,
}

/// The result codes of section 3.5.
fn result_text(code: u16) -> &'static str {
    match code {
        1 => "unsupported version",
        2 => "not authorized (NAT-PMP is off on the router)",
        3 => "network failure",
        4 => "out of resources",
        5 => "unsupported opcode",
        _ => "unknown error",
    }
}

/// A port forward the gateway granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Granted {
    pub internal: u16,
    /// The port outside, which may differ from the one asked for.
    pub external: u16,
    /// Seconds until it lapses; 0 after a removal.
    pub lifetime: u32,
}

pub(crate) fn encode_map(
    protocol: Protocol,
    internal: u16,
    external: u16,
    lifetime: u32,
) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[0] = VERSION;
    b[1] = match protocol {
        Protocol::Udp => OP_UDP,
        Protocol::Tcp => OP_TCP,
    };
    b[4..6].copy_from_slice(&internal.to_be_bytes());
    b[6..8].copy_from_slice(&external.to_be_bytes());
    b[8..12].copy_from_slice(&lifetime.to_be_bytes());
    b
}

fn check_header(b: &[u8], op: u8) -> Result<(), NatPmpError> {
    if b.len() < 8 || b[0] != VERSION || b[1] != op + RESPONSE {
        return Err(NatPmpError::Malformed);
    }
    match u16::from_be_bytes([b[2], b[3]]) {
        0 => Ok(()),
        code => Err(NatPmpError::Refused(result_text(code))),
    }
}

pub(crate) fn decode_map(b: &[u8], protocol: Protocol) -> Result<Granted, NatPmpError> {
    let op = match protocol {
        Protocol::Udp => OP_UDP,
        Protocol::Tcp => OP_TCP,
    };
    check_header(b, op)?;
    if b.len() < 16 {
        return Err(NatPmpError::Malformed);
    }
    Ok(Granted {
        internal: u16::from_be_bytes([b[8], b[9]]),
        external: u16::from_be_bytes([b[10], b[11]]),
        lifetime: u32::from_be_bytes([b[12], b[13], b[14], b[15]]),
    })
}

pub(crate) fn decode_external(b: &[u8]) -> Result<Ipv4Addr, NatPmpError> {
    check_header(b, OP_EXTERNAL)?;
    if b.len() < 12 {
        return Err(NatPmpError::Malformed);
    }
    Ok(Ipv4Addr::new(b[8], b[9], b[10], b[11]))
}

/// Send `request` to `server` and return the first answer, retrying as the RFC asks.
async fn exchange(server: SocketAddr, request: &[u8]) -> Result<Vec<u8>, NatPmpError> {
    let socket = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0))).await?;
    socket.connect(server).await?;
    let mut wait = FIRST_WAIT;
    let mut buf = [0u8; 64];
    for _ in 0..TRIES {
        socket.send(request).await?;
        match tokio::time::timeout(wait, socket.recv(&mut buf)).await {
            Ok(Ok(n)) => return Ok(buf[..n].to_vec()),
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => wait *= 2,
        }
    }
    Err(NatPmpError::NoAnswer(server))
}

/// The gateway's external address.
pub async fn external_address(server: SocketAddr) -> Result<Ipv4Addr, NatPmpError> {
    decode_external(&exchange(server, &[VERSION, OP_EXTERNAL]).await?)
}

/// Forward `external` (a wish; the gateway may pick another) to `internal` on this
/// machine for `lifetime` seconds. A lifetime of 0 removes the forward.
pub async fn map(
    server: SocketAddr,
    protocol: Protocol,
    internal: u16,
    external: u16,
    lifetime: u32,
) -> Result<Granted, NatPmpError> {
    let answer = exchange(server, &encode_map(protocol, internal, external, lifetime)).await?;
    decode_map(&answer, protocol)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A gateway on localhost that grants every forward one port up, and answers the
    /// first request of each kind only on the second try.
    async fn fake_gateway() -> SocketAddr {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            let mut seen_first = false;
            loop {
                let Ok((n, from)) = socket.recv_from(&mut buf).await else {
                    return;
                };
                if !seen_first {
                    seen_first = true;
                    continue;
                }
                let req = &buf[..n];
                let mut out = vec![0, req[1] + RESPONSE, 0, 0, 0, 0, 0, 7];
                if req[1] == OP_EXTERNAL {
                    out.extend([203, 0, 113, 9]);
                } else {
                    let internal = u16::from_be_bytes([req[4], req[5]]);
                    let external = u16::from_be_bytes([req[6], req[7]]) + 1;
                    out.extend(internal.to_be_bytes());
                    out.extend(external.to_be_bytes());
                    out.extend(&req[8..12]);
                }
                let _ = socket.send_to(&out, from).await;
            }
        });
        addr
    }

    #[tokio::test]
    async fn maps_a_port_and_reads_the_external_address() {
        let gw = fake_gateway().await;
        let g = map(gw, Protocol::Tcp, 40_000, 40_000, 3600).await.unwrap();
        assert_eq!(
            g,
            Granted {
                internal: 40_000,
                external: 40_001,
                lifetime: 3600
            }
        );
        assert_eq!(
            external_address(gw).await.unwrap(),
            Ipv4Addr::new(203, 0, 113, 9)
        );
    }

    #[test]
    fn refusals_and_garbage_are_errors() {
        let refused = [
            0,
            OP_TCP + RESPONSE,
            0,
            2,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        assert!(matches!(
            decode_map(&refused, Protocol::Tcp),
            Err(NatPmpError::Refused(t)) if t.contains("not authorized")
        ));
        assert!(matches!(
            decode_map(&[0, 1], Protocol::Tcp),
            Err(NatPmpError::Malformed)
        ));
        // An answer to the other protocol's request.
        let udp = [
            0,
            OP_UDP + RESPONSE,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            1,
            0,
            1,
            0,
            0,
            0,
            1,
        ];
        assert!(decode_map(&udp, Protocol::Tcp).is_err());
        assert_eq!(
            encode_map(Protocol::Udp, 1, 2, 3),
            [0, 1, 0, 0, 0, 1, 0, 2, 0, 0, 0, 3]
        );
    }

    #[tokio::test]
    async fn a_silent_gateway_is_no_answer() {
        let quiet = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = quiet.local_addr().unwrap();
        assert!(matches!(
            external_address(addr).await,
            Err(NatPmpError::NoAnswer(_))
        ));
    }
}

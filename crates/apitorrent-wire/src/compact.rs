//! Compact peer addresses (BEP 23 for IPv4, BEP 7 for IPv6), shared by trackers, PEX and DHT.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

/// Parse concatenated 6-byte IPv4 entries. A trailing partial entry is ignored.
pub fn parse_v4(data: &[u8]) -> Vec<SocketAddr> {
    data.as_chunks::<6>()
        .0
        .iter()
        .map(|c| {
            let ip = Ipv4Addr::new(c[0], c[1], c[2], c[3]);
            let port = u16::from_be_bytes([c[4], c[5]]);
            SocketAddr::V4(SocketAddrV4::new(ip, port))
        })
        .collect()
}

/// Parse concatenated 18-byte IPv6 entries. A trailing partial entry is ignored.
pub fn parse_v6(data: &[u8]) -> Vec<SocketAddr> {
    data.as_chunks::<18>()
        .0
        .iter()
        .map(|c| {
            let ip: [u8; 16] = c[..16].try_into().expect("16 bytes");
            let port = u16::from_be_bytes([c[16], c[17]]);
            SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::from(ip), port, 0, 0))
        })
        .collect()
}

/// Encode one address in compact form (6 bytes for IPv4, 18 for IPv6).
pub fn encode(addr: &SocketAddr, out: &mut Vec<u8>) {
    match addr {
        SocketAddr::V4(a) => out.extend_from_slice(&a.ip().octets()),
        SocketAddr::V6(a) => out.extend_from_slice(&a.ip().octets()),
    }
    out.extend_from_slice(&addr.port().to_be_bytes());
}

/// Parse a bare compact IP (4 or 16 bytes), as in the BEP 10 `yourip` field.
pub fn parse_ip(data: &[u8]) -> Option<IpAddr> {
    match data.len() {
        4 => Some(IpAddr::V4(Ipv4Addr::new(
            data[0], data[1], data[2], data[3],
        ))),
        16 => {
            let octets: [u8; 16] = data.try_into().ok()?;
            Some(IpAddr::V6(Ipv6Addr::from(octets)))
        }
        _ => None,
    }
}

pub fn encode_ip(ip: &IpAddr) -> Vec<u8> {
    match ip {
        IpAddr::V4(a) => a.octets().to_vec(),
        IpAddr::V6(a) => a.octets().to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let addrs: Vec<SocketAddr> = vec!["1.2.3.4:6881".parse().unwrap()];
        let mut out = Vec::new();
        encode(&addrs[0], &mut out);
        assert_eq!(out, [1, 2, 3, 4, 0x1a, 0xe1]);
        assert_eq!(parse_v4(&out), addrs);

        let v6: SocketAddr = "[2001:db8::1]:51413".parse().unwrap();
        let mut out = Vec::new();
        encode(&v6, &mut out);
        assert_eq!(out.len(), 18);
        assert_eq!(parse_v6(&out), [v6]);
    }
}

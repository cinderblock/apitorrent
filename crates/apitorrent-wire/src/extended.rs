//! Extension-protocol payloads: the BEP 10 handshake, BEP 9 metadata exchange, BEP 11 peer
//! exchange, BEP 55 holepunching, and libtorrent's `upload_only` and `lt_donthave`.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};

use apitorrent_bencode::{DecodeOptions, OwnedValue, Value};
use bytes::Bytes;

use crate::compact;

/// Extension names this implementation speaks, as used in the handshake's `m` dictionary.
pub mod names {
    pub const UT_METADATA: &str = "ut_metadata";
    pub const UT_PEX: &str = "ut_pex";
    pub const UPLOAD_ONLY: &str = "upload_only";
    pub const UT_HOLEPUNCH: &str = "ut_holepunch";
    pub const LT_DONTHAVE: &str = "lt_donthave";
}

/// libtorrent's `upload_only` message: whether the sender has stopped downloading (it has
/// everything it wants), sent when that changes after the handshake. One byte, 0 or 1.
pub fn decode_upload_only(payload: &[u8]) -> Result<bool, ExtendedError> {
    payload
        .first()
        .map(|&b| b != 0)
        .ok_or(ExtendedError::Malformed("upload_only"))
}

pub fn encode_upload_only(upload_only: bool) -> Vec<u8> {
    vec![u8::from(upload_only)]
}

/// libtorrent's `lt_donthave` message: the sender no longer has this piece. Its index,
/// big-endian.
pub fn decode_dont_have(payload: &[u8]) -> Result<u32, ExtendedError> {
    payload
        .get(..4)
        .map(|b| u32::from_be_bytes(b.try_into().expect("four bytes")))
        .ok_or(ExtendedError::Malformed("lt_donthave"))
}

pub fn encode_dont_have(piece: u32) -> Vec<u8> {
    piece.to_be_bytes().to_vec()
}

/// A BEP 55 holepunch error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HolepunchError {
    /// The target isn't a valid peer address.
    NoSuchPeer,
    /// The relay isn't connected to the target.
    NotConnected,
    /// The target doesn't support holepunching.
    NoSupport,
    /// The target is the sender itself.
    NoSelf,
    /// A code this implementation doesn't know.
    Other(u32),
}

impl HolepunchError {
    pub fn code(self) -> u32 {
        match self {
            Self::NoSuchPeer => 1,
            Self::NotConnected => 2,
            Self::NoSupport => 3,
            Self::NoSelf => 4,
            Self::Other(c) => c,
        }
    }

    pub fn from_code(code: u32) -> Self {
        match code {
            1 => Self::NoSuchPeer,
            2 => Self::NotConnected,
            3 => Self::NoSupport,
            4 => Self::NoSelf,
            c => Self::Other(c),
        }
    }
}

/// A BEP 55 `ut_holepunch` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HolepunchMessage {
    /// To a relay: put me in touch with this peer.
    Rendezvous(SocketAddr),
    /// From a relay: connect to this peer (over uTP), which is connecting to you.
    Connect(SocketAddr),
    /// From a relay: the rendezvous for this peer failed.
    Error(SocketAddr, HolepunchError),
}

impl HolepunchMessage {
    pub fn addr(self) -> SocketAddr {
        match self {
            Self::Rendezvous(a) | Self::Connect(a) | Self::Error(a, _) => a,
        }
    }

    /// `msg_type`, `addr_type` (0 IPv4, 1 IPv6), the address and port, then the error code.
    /// BEP 55 always sends the code (0 when not an error); libtorrent sends it only for
    /// errors. Both are read.
    pub fn decode(payload: &[u8]) -> Result<Self, ExtendedError> {
        let bad = || ExtendedError::Malformed("ut_holepunch");
        let (&kind, rest) = payload.split_first().ok_or_else(bad)?;
        let (&family, rest) = rest.split_first().ok_or_else(bad)?;
        let ip_len = match family {
            0 => 4,
            1 => 16,
            _ => return Err(bad()),
        };
        let addr_bytes = rest.get(..ip_len + 2).ok_or_else(bad)?;
        let ip = compact::parse_ip(&addr_bytes[..ip_len]).ok_or_else(bad)?;
        let port = u16::from_be_bytes([addr_bytes[ip_len], addr_bytes[ip_len + 1]]);
        let addr = SocketAddr::new(ip, port);
        let code = rest
            .get(ip_len + 2..ip_len + 6)
            .map(|b| u32::from_be_bytes(b.try_into().expect("four bytes")));
        match kind {
            0 => Ok(Self::Rendezvous(addr)),
            1 => Ok(Self::Connect(addr)),
            2 => Ok(Self::Error(
                addr,
                HolepunchError::from_code(code.ok_or_else(bad)?),
            )),
            _ => Err(bad()),
        }
    }

    /// Always with the error code, as BEP 55 says (libtorrent reads past it).
    pub fn encode(self) -> Vec<u8> {
        let (kind, code) = match self {
            Self::Rendezvous(_) => (0u8, 0),
            Self::Connect(_) => (1, 0),
            Self::Error(_, e) => (2, e.code()),
        };
        // An IPv4 peer seen on a dual-stack socket goes out as IPv4.
        let addr = SocketAddr::new(self.addr().ip().to_canonical(), self.addr().port());
        let mut out = vec![kind, u8::from(addr.is_ipv6())];
        compact::encode(&addr, &mut out);
        out.extend_from_slice(&code.to_be_bytes());
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExtendedError {
    #[error("invalid bencoding: {0}")]
    Bencode(#[from] apitorrent_bencode::Error),
    #[error("malformed {0} message")]
    Malformed(&'static str),
}

/// The BEP 10 extension handshake.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtensionHandshake {
    /// `m`: extension name → the message id the sender wants to *receive* it on. An id of 0
    /// means the extension is disabled.
    pub extensions: BTreeMap<String, u8>,
    /// `p`: the sender's listen port.
    pub listen_port: Option<u16>,
    /// `v`: client name and version.
    pub client: Option<String>,
    /// `yourip`: the receiver's address as the sender sees it.
    pub your_ip: Option<IpAddr>,
    /// `reqq`: how many outstanding requests the sender allows.
    pub request_queue: Option<u32>,
    /// `metadata_size` (BEP 9): the size of the info dictionary.
    pub metadata_size: Option<u64>,
    /// `upload_only` (BEP 21): the sender is a seed or partial seed that won't download.
    pub upload_only: bool,
}

impl ExtensionHandshake {
    pub fn decode(payload: &[u8]) -> Result<Self, ExtendedError> {
        let v = apitorrent_bencode::decode_with(payload, &DecodeOptions::lenient())?;
        let d = v
            .as_dict()
            .ok_or(ExtendedError::Malformed("extension handshake"))?;
        let mut out = ExtensionHandshake::default();
        if let Some(m) = d.get_dict(b"m") {
            for (name, id) in m.iter() {
                let (Ok(name), Some(id)) = (std::str::from_utf8(name), id.as_int()) else {
                    continue;
                };
                if let Ok(id) = u8::try_from(id) {
                    out.extensions.insert(name.to_owned(), id);
                }
            }
        }
        out.listen_port = d
            .get_int(b"p")
            .and_then(|p| u16::try_from(p).ok())
            .filter(|&p| p != 0);
        out.client = d.get_str(b"v").map(str::to_owned);
        out.your_ip = d.get_bytes(b"yourip").and_then(compact::parse_ip);
        out.request_queue = d
            .get_int(b"reqq")
            .and_then(|q| u32::try_from(q).ok())
            .filter(|&q| q > 0);
        out.metadata_size = d
            .get_int(b"metadata_size")
            .and_then(|s| u64::try_from(s).ok())
            .filter(|&s| s > 0);
        out.upload_only = d.get_int(b"upload_only").is_some_and(|u| u != 0);
        Ok(out)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut m = OwnedValue::dict();
        for (name, id) in &self.extensions {
            m = m.insert(name.as_bytes(), i64::from(*id));
        }
        let mut d = OwnedValue::dict().insert("m", m);
        if let Some(p) = self.listen_port {
            d = d.insert("p", i64::from(p));
        }
        if let Some(v) = &self.client {
            d = d.insert("v", v.as_str());
        }
        if let Some(ip) = &self.your_ip {
            d = d.insert("yourip", compact::encode_ip(ip));
        }
        if let Some(q) = self.request_queue {
            d = d.insert("reqq", i64::from(q));
        }
        if let Some(s) = self.metadata_size {
            d = d.insert("metadata_size", i64::try_from(s).unwrap_or(i64::MAX));
        }
        if self.upload_only {
            d = d.insert("upload_only", 1i64);
        }
        d.to_bytes()
    }

    /// The id to send extension `name` with, if the peer supports it.
    pub fn id_of(&self, name: &str) -> Option<u8> {
        self.extensions.get(name).copied().filter(|&id| id != 0)
    }
}

/// The size of a metadata piece in BEP 9.
pub const METADATA_PIECE_LEN: usize = 16 * 1024;

/// A BEP 9 `ut_metadata` message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataMessage {
    Request {
        piece: u32,
    },
    Data {
        piece: u32,
        total_size: u64,
        data: Bytes,
    },
    Reject {
        piece: u32,
    },
}

impl MetadataMessage {
    pub fn decode(payload: &Bytes) -> Result<Self, ExtendedError> {
        let (v, used) = apitorrent_bencode::decode_prefix(payload, &DecodeOptions::lenient())?;
        let d = v.as_dict().ok_or(ExtendedError::Malformed("ut_metadata"))?;
        let piece = d
            .get_int(b"piece")
            .and_then(|p| u32::try_from(p).ok())
            .ok_or(ExtendedError::Malformed("ut_metadata"))?;
        match d.get_int(b"msg_type") {
            Some(0) => Ok(MetadataMessage::Request { piece }),
            Some(1) => {
                let total_size = d
                    .get_int(b"total_size")
                    .and_then(|s| u64::try_from(s).ok())
                    .ok_or(ExtendedError::Malformed("ut_metadata"))?;
                let data = payload.slice(used..);
                if data.len() > METADATA_PIECE_LEN {
                    return Err(ExtendedError::Malformed("ut_metadata"));
                }
                Ok(MetadataMessage::Data {
                    piece,
                    total_size,
                    data,
                })
            }
            Some(2) => Ok(MetadataMessage::Reject { piece }),
            _ => Err(ExtendedError::Malformed("ut_metadata")),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let (msg_type, piece) = match self {
            MetadataMessage::Request { piece } => (0i64, piece),
            MetadataMessage::Data { piece, .. } => (1, piece),
            MetadataMessage::Reject { piece } => (2, piece),
        };
        let mut d = OwnedValue::dict()
            .insert("msg_type", msg_type)
            .insert("piece", i64::from(*piece));
        if let MetadataMessage::Data { total_size, .. } = self {
            d = d.insert("total_size", i64::try_from(*total_size).unwrap_or(i64::MAX));
        }
        let mut out = d.to_bytes();
        if let MetadataMessage::Data { data, .. } = self {
            out.extend_from_slice(data);
        }
        out
    }
}

/// BEP 11 per-peer flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PexFlags(pub u8);

impl PexFlags {
    pub fn prefers_encryption(self) -> bool {
        self.0 & 0x01 != 0
    }
    pub fn is_seed(self) -> bool {
        self.0 & 0x02 != 0
    }
    pub fn supports_utp(self) -> bool {
        self.0 & 0x04 != 0
    }
    pub fn supports_holepunch(self) -> bool {
        self.0 & 0x08 != 0
    }
    pub fn reachable(self) -> bool {
        self.0 & 0x10 != 0
    }
}

/// A BEP 11 `ut_pex` message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PexMessage {
    pub added: Vec<(SocketAddr, PexFlags)>,
    pub dropped: Vec<SocketAddr>,
}

impl PexMessage {
    /// BEP 11 caps a message at 50 added and 50 dropped peers; a peer that sends more is
    /// truncated rather than trusted.
    pub const MAX_PEERS: usize = 50;

    pub fn decode(payload: &[u8]) -> Result<Self, ExtendedError> {
        let v = apitorrent_bencode::decode_with(payload, &DecodeOptions::lenient())?;
        let d = v.as_dict().ok_or(ExtendedError::Malformed("ut_pex"))?;
        let mut out = PexMessage::default();
        for (key, flags_key, parse) in [
            (
                &b"added"[..],
                &b"added.f"[..],
                compact::parse_v4 as fn(&[u8]) -> Vec<SocketAddr>,
            ),
            (b"added6", b"added6.f", compact::parse_v6),
        ] {
            let addrs = d.get_bytes(key).map(parse).unwrap_or_default();
            let flags = d.get_bytes(flags_key).unwrap_or_default();
            for (i, a) in addrs.into_iter().take(Self::MAX_PEERS).enumerate() {
                out.added
                    .push((a, PexFlags(flags.get(i).copied().unwrap_or_default())));
            }
        }
        for (key, parse) in [
            (
                &b"dropped"[..],
                compact::parse_v4 as fn(&[u8]) -> Vec<SocketAddr>,
            ),
            (b"dropped6", compact::parse_v6),
        ] {
            let addrs = d.get_bytes(key).map(parse).unwrap_or_default();
            out.dropped.extend(addrs.into_iter().take(Self::MAX_PEERS));
        }
        Ok(out)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut added = (Vec::new(), Vec::new());
        let mut added6 = (Vec::new(), Vec::new());
        for (a, f) in &self.added {
            let (addrs, flags) = if a.is_ipv4() { &mut added } else { &mut added6 };
            compact::encode(a, addrs);
            flags.push(f.0);
        }
        let mut dropped = Vec::new();
        let mut dropped6 = Vec::new();
        for a in &self.dropped {
            compact::encode(
                a,
                if a.is_ipv4() {
                    &mut dropped
                } else {
                    &mut dropped6
                },
            );
        }
        OwnedValue::dict()
            .insert("added", added.0)
            .insert("added.f", added.1)
            .insert("added6", added6.0)
            .insert("added6.f", added6.1)
            .insert("dropped", dropped)
            .insert("dropped6", dropped6)
            .to_bytes()
    }
}

/// Decode a value only to check it is bencoded, for extension payloads we pass through.
pub fn is_bencoded(payload: &[u8]) -> bool {
    matches!(
        apitorrent_bencode::decode_with(payload, &DecodeOptions::lenient()),
        Ok(Value::Dict(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_round_trip() {
        let mut hs = ExtensionHandshake {
            listen_port: Some(51413),
            client: Some("apitorrent 0.1.0".into()),
            your_ip: Some("203.0.113.7".parse().unwrap()),
            request_queue: Some(500),
            metadata_size: Some(31_337),
            upload_only: true,
            ..Default::default()
        };
        hs.extensions.insert(names::UT_METADATA.into(), 1);
        hs.extensions.insert(names::UT_PEX.into(), 2);
        let decoded = ExtensionHandshake::decode(&hs.encode()).unwrap();
        assert_eq!(decoded, hs);
        assert_eq!(decoded.id_of(names::UT_PEX), Some(2));
        assert_eq!(decoded.id_of("lt_donthave"), None);
    }

    #[test]
    fn metadata_messages() {
        for m in [
            MetadataMessage::Request { piece: 2 },
            MetadataMessage::Reject { piece: 2 },
            MetadataMessage::Data {
                piece: 0,
                total_size: 5,
                data: Bytes::from_static(b"d1:ae"),
            },
        ] {
            let bytes = Bytes::from(m.encode());
            assert_eq!(MetadataMessage::decode(&bytes).unwrap(), m);
        }
    }

    #[test]
    fn upload_only_and_dont_have() {
        assert_eq!(decode_upload_only(&encode_upload_only(true)), Ok(true));
        assert_eq!(decode_upload_only(&encode_upload_only(false)), Ok(false));
        assert!(decode_upload_only(&[]).is_err());
        // libtorrent's `write_dont_have`: the piece index, big-endian.
        assert_eq!(encode_dont_have(0x0102_0304), [1, 2, 3, 4]);
        assert_eq!(decode_dont_have(&[0, 0, 1, 0]), Ok(256));
        assert!(decode_dont_have(&[0, 0, 1]).is_err());
    }

    #[test]
    fn holepunch_messages() {
        let v4: SocketAddr = "1.2.3.4:6881".parse().unwrap();
        let v6: SocketAddr = "[2001:db8::1]:51413".parse().unwrap();
        for m in [
            HolepunchMessage::Rendezvous(v4),
            HolepunchMessage::Connect(v6),
            HolepunchMessage::Error(v4, HolepunchError::NotConnected),
            HolepunchMessage::Error(v6, HolepunchError::Other(9)),
        ] {
            assert_eq!(HolepunchMessage::decode(&m.encode()), Ok(m));
        }
        // BEP 55's layout: type, address type, address, port, error code.
        assert_eq!(
            HolepunchMessage::Error(v4, HolepunchError::NoSupport).encode(),
            [2, 0, 1, 2, 3, 4, 0x1a, 0xe1, 0, 0, 0, 3]
        );
        // libtorrent leaves the code out of rendezvous and connect messages.
        assert_eq!(
            HolepunchMessage::decode(&[1, 0, 1, 2, 3, 4, 0x1a, 0xe1]),
            Ok(HolepunchMessage::Connect(v4))
        );
        // An IPv4 address seen as IPv6-mapped goes out as IPv4.
        let mapped: SocketAddr = "[::ffff:1.2.3.4]:6881".parse().unwrap();
        assert_eq!(
            HolepunchMessage::Connect(mapped).encode()[..8],
            [1, 0, 1, 2, 3, 4, 0x1a, 0xe1]
        );
        for bad in [
            &[][..],
            &[0],
            &[0, 2, 1, 2, 3, 4, 0, 1],
            &[0, 0, 1, 2, 3],
            &[3, 0, 1, 2, 3, 4, 0, 1],
            &[2, 0, 1, 2, 3, 4, 0, 1],
        ] {
            assert!(HolepunchMessage::decode(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn pex_round_trip() {
        let m = PexMessage {
            added: vec![
                ("1.2.3.4:6881".parse().unwrap(), PexFlags(0x12)),
                ("[2001:db8::2]:6882".parse().unwrap(), PexFlags(0x04)),
            ],
            dropped: vec!["5.6.7.8:1".parse().unwrap()],
        };
        let decoded = PexMessage::decode(&m.encode()).unwrap();
        assert_eq!(decoded, m);
        assert!(decoded.added[0].1.is_seed());
        assert!(decoded.added[1].1.supports_utp());
    }
}

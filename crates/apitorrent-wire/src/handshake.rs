//! The BitTorrent handshake (BEP 3), with the reserved-bit extensions of BEP 5, 6, 10 and 52.

use std::fmt;

use apitorrent_metainfo::Sha1Hash;

pub const PROTOCOL: &[u8; 19] = b"BitTorrent protocol";
pub const HANDSHAKE_LEN: usize = 68;
/// Bytes needed before the info-hash is known: enough for an incoming connection to pick
/// the torrent it wants.
pub const HANDSHAKE_PREFIX_LEN: usize = 48;

/// The 8 reserved handshake bytes, which advertise protocol extensions.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Reserved(pub [u8; 8]);

impl Reserved {
    const EXTENSION_PROTOCOL: (usize, u8) = (5, 0x10); // BEP 10
    const FAST: (usize, u8) = (7, 0x04); // BEP 6
    const DHT: (usize, u8) = (7, 0x01); // BEP 5
    const V2: (usize, u8) = (7, 0x10); // BEP 52 upgrade

    fn get(self, (byte, mask): (usize, u8)) -> bool {
        self.0[byte] & mask != 0
    }

    #[must_use]
    fn with(mut self, (byte, mask): (usize, u8), on: bool) -> Self {
        if on {
            self.0[byte] |= mask;
        } else {
            self.0[byte] &= !mask;
        }
        self
    }

    pub fn extension_protocol(self) -> bool {
        self.get(Self::EXTENSION_PROTOCOL)
    }
    pub fn fast(self) -> bool {
        self.get(Self::FAST)
    }
    pub fn dht(self) -> bool {
        self.get(Self::DHT)
    }
    pub fn v2(self) -> bool {
        self.get(Self::V2)
    }

    #[must_use]
    pub fn with_extension_protocol(self, on: bool) -> Self {
        self.with(Self::EXTENSION_PROTOCOL, on)
    }
    #[must_use]
    pub fn with_fast(self, on: bool) -> Self {
        self.with(Self::FAST, on)
    }
    #[must_use]
    pub fn with_dht(self, on: bool) -> Self {
        self.with(Self::DHT, on)
    }
    #[must_use]
    pub fn with_v2(self, on: bool) -> Self {
        self.with(Self::V2, on)
    }
}

impl fmt::Debug for Reserved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_set();
        if self.extension_protocol() {
            s.entry(&"extension_protocol");
        }
        if self.fast() {
            s.entry(&"fast");
        }
        if self.dht() {
            s.entry(&"dht");
        }
        if self.v2() {
            s.entry(&"v2");
        }
        s.finish()
    }
}

/// A 20-byte peer id.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerId(pub [u8; 20]);

impl PeerId {
    /// An Azureus-style peer id: `-AT0100-` followed by 12 bytes the caller supplies (they
    /// should be random). The version is `major.minor.patch`, one character each.
    pub fn azureus(client: [u8; 2], version: [u8; 4], random: [u8; 12]) -> Self {
        let mut id = [0u8; 20];
        id[0] = b'-';
        id[1..3].copy_from_slice(&client);
        id[3..7].copy_from_slice(&version);
        id[7] = b'-';
        id[8..].copy_from_slice(&random);
        Self(id)
    }

    /// A readable client name and version, for the peer list. Recognises the
    /// Azureus-style convention most clients follow.
    pub fn client(&self) -> String {
        let id = &self.0;
        if id[0] == b'-' && id[7] == b'-' {
            let code = &id[1..3];
            let name = match code {
                b"AT" => "apitorrent",
                b"AZ" => "Vuze",
                b"BI" => "BiglyBT",
                b"BT" => "BitTorrent",
                b"DE" => "Deluge",
                b"LT" => "libtorrent",
                b"lt" => "libTorrent (rakshasa)",
                b"qB" => "qBittorrent",
                b"rq" | b"RQ" => "rqbit",
                b"TR" => "Transmission",
                b"TX" => "Tixati",
                b"UT" => "µTorrent",
                b"UM" => "µTorrent Mac",
                b"WW" => "WebTorrent",
                b"FD" => "Free Download Manager",
                b"BC" => "BitComet",
                b"PI" => "PicoTorrent",
                _ => "",
            };
            let version: String = id[3..7]
                .iter()
                .filter(|b| b.is_ascii_alphanumeric())
                .map(|&b| char::from(b))
                .collect();
            if name.is_empty() {
                return format!("{} {version}", String::from_utf8_lossy(code));
            }
            return format!("{name} {version}");
        }
        "unknown".to_owned()
    }
}

impl fmt::Debug for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let printable: String = self
            .0
            .iter()
            .map(|&b| {
                if b.is_ascii_graphic() {
                    char::from(b)
                } else {
                    '.'
                }
            })
            .collect();
        write!(f, "PeerId({printable})")
    }
}

/// A complete handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handshake {
    pub reserved: Reserved,
    /// The v1 info-hash, or the truncated v2 info-hash for a v2-only swarm.
    pub info_hash: Sha1Hash,
    pub peer_id: PeerId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HandshakeError {
    #[error("not a BitTorrent handshake")]
    WrongProtocol,
    #[error("handshake is incomplete")]
    Incomplete,
}

impl Handshake {
    pub fn encode(&self) -> [u8; HANDSHAKE_LEN] {
        let mut out = [0u8; HANDSHAKE_LEN];
        out[0] = 19;
        out[1..20].copy_from_slice(PROTOCOL);
        out[20..28].copy_from_slice(&self.reserved.0);
        out[28..48].copy_from_slice(&self.info_hash.0);
        out[48..68].copy_from_slice(&self.peer_id.0);
        out
    }

    /// Decode a handshake from the first 68 bytes of `buf`.
    pub fn decode(buf: &[u8]) -> Result<Self, HandshakeError> {
        let (reserved, info_hash) = Self::decode_prefix(buf)?;
        let peer_id = buf
            .get(48..68)
            .ok_or(HandshakeError::Incomplete)?
            .try_into()
            .map(PeerId)
            .expect("slice is 20 bytes");
        Ok(Self {
            reserved,
            info_hash,
            peer_id,
        })
    }

    /// Decode the reserved bytes and info-hash from the first 48 bytes. An incoming
    /// connection reads this much before deciding whether it serves the torrent.
    pub fn decode_prefix(buf: &[u8]) -> Result<(Reserved, Sha1Hash), HandshakeError> {
        if let Some(&len) = buf.first()
            && len != 19
        {
            return Err(HandshakeError::WrongProtocol);
        }
        if buf.len() >= 20 && &buf[1..20] != PROTOCOL {
            return Err(HandshakeError::WrongProtocol);
        }
        if buf.len() < HANDSHAKE_PREFIX_LEN {
            return Err(HandshakeError::Incomplete);
        }
        let reserved = Reserved(buf[20..28].try_into().expect("8 bytes"));
        let info_hash = Sha1Hash::from_slice(&buf[28..48]).expect("20 bytes");
        Ok((reserved, info_hash))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let hs = Handshake {
            reserved: Reserved::default()
                .with_extension_protocol(true)
                .with_fast(true)
                .with_dht(true)
                .with_v2(true),
            info_hash: Sha1Hash([7; 20]),
            peer_id: PeerId::azureus(*b"AT", *b"0100", [9; 12]),
        };
        let bytes = hs.encode();
        assert_eq!(&bytes[..20], b"\x13BitTorrent protocol");
        assert_eq!(bytes[25], 0x10);
        assert_eq!(bytes[27], 0x15);
        assert_eq!(Handshake::decode(&bytes).unwrap(), hs);
        assert_eq!(hs.peer_id.client(), "apitorrent 0100");
    }

    #[test]
    fn rejects_other_protocols() {
        assert_eq!(
            Handshake::decode(b"GET / HTTP/1.1\r\n").unwrap_err(),
            HandshakeError::WrongProtocol
        );
        assert_eq!(
            Handshake::decode(b"\x13BitTorrent").unwrap_err(),
            HandshakeError::Incomplete
        );
        assert_eq!(
            Handshake::decode(b"\x13BitTorrent protocoX........").unwrap_err(),
            HandshakeError::WrongProtocol
        );
    }
}

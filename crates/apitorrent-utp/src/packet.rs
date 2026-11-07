//! The uTP packet format ([BEP 29]): a 20-byte header, an extension chain and a payload.
//!
//! ```text
//! 0       4       8               16              24              32
//! +-------+-------+---------------+---------------+---------------+
//! | type  | ver   | extension     | connection_id                 |
//! +-------+-------+---------------+---------------+---------------+
//! | timestamp_microseconds                                        |
//! +---------------+---------------+---------------+---------------+
//! | timestamp_difference_microseconds                             |
//! +---------------+---------------+---------------+---------------+
//! | wnd_size                                                      |
//! +---------------+---------------+---------------+---------------+
//! | seq_nr                        | ack_nr                        |
//! +---------------+---------------+---------------+---------------+
//! ```
//!
//! [BEP 29]: https://www.bittorrent.org/beps/bep_0029.html

use std::fmt;

/// The header length.
pub const HEADER_LEN: usize = 20;

/// The protocol version in every packet.
pub const VERSION: u8 = 1;

/// The selective-ACK extension.
pub const EXT_SELECTIVE_ACK: u8 = 1;

/// The longest selective-ACK bitmask we send or read: 256 packets past `ack_nr + 1`.
pub const MAX_SACK_BYTES: usize = 32;

/// Packet types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PacketType {
    /// Payload data.
    Data = 0,
    /// The last packet of the sender's stream.
    Fin = 1,
    /// An ACK with no data.
    State = 2,
    /// Abort the connection.
    Reset = 3,
    /// Open a connection.
    Syn = 4,
}

impl PacketType {
    fn from_nibble(n: u8) -> Option<Self> {
        Some(match n {
            0 => Self::Data,
            1 => Self::Fin,
            2 => Self::State,
            3 => Self::Reset,
            4 => Self::Syn,
            _ => return None,
        })
    }
}

/// Why a datagram is not a uTP packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PacketError {
    #[error("shorter than a uTP header")]
    TooShort,
    #[error("not uTP version 1")]
    BadVersion,
    #[error("unknown packet type")]
    BadType,
    #[error("truncated extension")]
    BadExtension,
}

/// The fixed header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub ty: PacketType,
    pub connection_id: u16,
    pub timestamp: u32,
    pub timestamp_diff: u32,
    pub wnd_size: u32,
    pub seq_nr: u16,
    pub ack_nr: u16,
}

/// A selective ACK: bit `i` set means packet `ack_nr + 2 + i` has arrived. Within each
/// byte the least significant bit comes first.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct SelectiveAck {
    bits: [u8; MAX_SACK_BYTES],
    len: u8,
}

impl fmt::Debug for SelectiveAck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let set: Vec<usize> = (0..self.bit_len()).filter(|&i| self.get(i)).collect();
        f.debug_struct("SelectiveAck")
            .field("len", &self.len)
            .field("set", &set)
            .finish_non_exhaustive()
    }
}

impl SelectiveAck {
    /// An empty mask covering at least `bits` packets (rounded up to 32, capped at 256).
    pub fn with_capacity(bits: usize) -> Self {
        let bytes = bits.div_ceil(32).clamp(1, MAX_SACK_BYTES / 4) * 4;
        Self {
            bits: [0; MAX_SACK_BYTES],
            len: u8::try_from(bytes).expect("at most 32"),
        }
    }

    /// Parse a mask from the wire. Masks over 32 bytes are cut to 32: the bits beyond only
    /// cost us a later retransmission. `None` when the length is not a positive multiple
    /// of 4.
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.is_empty() || !data.len().is_multiple_of(4) {
            return None;
        }
        let n = data.len().min(MAX_SACK_BYTES);
        let mut bits = [0; MAX_SACK_BYTES];
        bits[..n].copy_from_slice(&data[..n]);
        Some(Self {
            bits,
            len: u8::try_from(n).expect("at most 32"),
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bits[..usize::from(self.len)]
    }

    /// How many packets the mask covers.
    pub fn bit_len(&self) -> usize {
        usize::from(self.len) * 8
    }

    pub fn get(&self, i: usize) -> bool {
        i < self.bit_len() && self.bits[i / 8] & (1 << (i % 8)) != 0
    }

    pub fn set(&mut self, i: usize) {
        if i < self.bit_len() {
            self.bits[i / 8] |= 1 << (i % 8);
        }
    }
}

/// A parsed packet borrowing its payload from the datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet<'a> {
    pub header: Header,
    pub sack: Option<SelectiveAck>,
    pub payload: &'a [u8],
}

/// Whether a datagram looks like uTP: long enough, version 1 and a known type. Used to
/// share a UDP port with other protocols; a DHT message (a bencoded dictionary starting
/// with `d`, 0x64) never passes.
pub fn looks_like_utp(datagram: &[u8]) -> bool {
    datagram.len() >= HEADER_LEN
        && datagram[0] & 0x0f == VERSION
        && PacketType::from_nibble(datagram[0] >> 4).is_some()
}

impl<'a> Packet<'a> {
    pub fn decode(data: &'a [u8]) -> Result<Self, PacketError> {
        if data.len() < HEADER_LEN {
            return Err(PacketError::TooShort);
        }
        if data[0] & 0x0f != VERSION {
            return Err(PacketError::BadVersion);
        }
        let ty = PacketType::from_nibble(data[0] >> 4).ok_or(PacketError::BadType)?;
        let u16_at = |i: usize| u16::from_be_bytes([data[i], data[i + 1]]);
        let u32_at =
            |i: usize| u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
        let header = Header {
            ty,
            connection_id: u16_at(2),
            timestamp: u32_at(4),
            timestamp_diff: u32_at(8),
            wnd_size: u32_at(12),
            seq_nr: u16_at(16),
            ack_nr: u16_at(18),
        };
        let mut ext = data[1];
        let mut pos = HEADER_LEN;
        let mut sack = None;
        while ext != 0 {
            if data.len() < pos + 2 {
                return Err(PacketError::BadExtension);
            }
            let next = data[pos];
            let len = usize::from(data[pos + 1]);
            pos += 2;
            if data.len() < pos + len {
                return Err(PacketError::BadExtension);
            }
            if ext == EXT_SELECTIVE_ACK {
                sack = Some(
                    SelectiveAck::from_bytes(&data[pos..pos + len])
                        .ok_or(PacketError::BadExtension)?,
                );
            }
            // Other extensions (such as the legacy extension-bits one) are skipped.
            pos += len;
            ext = next;
        }
        Ok(Self {
            header,
            sack,
            payload: &data[pos..],
        })
    }
}

/// Append an encoded packet to `out`.
pub fn encode(header: &Header, sack: Option<&SelectiveAck>, payload: &[u8], out: &mut Vec<u8>) {
    out.reserve(HEADER_LEN + sack.map_or(0, |s| 2 + s.as_bytes().len()) + payload.len());
    out.push(((header.ty as u8) << 4) | VERSION);
    out.push(if sack.is_some() { EXT_SELECTIVE_ACK } else { 0 });
    out.extend_from_slice(&header.connection_id.to_be_bytes());
    out.extend_from_slice(&header.timestamp.to_be_bytes());
    out.extend_from_slice(&header.timestamp_diff.to_be_bytes());
    out.extend_from_slice(&header.wnd_size.to_be_bytes());
    out.extend_from_slice(&header.seq_nr.to_be_bytes());
    out.extend_from_slice(&header.ack_nr.to_be_bytes());
    if let Some(s) = sack {
        out.push(0);
        out.push(u8::try_from(s.as_bytes().len()).expect("at most 32"));
        out.extend_from_slice(s.as_bytes());
    }
    out.extend_from_slice(payload);
}

/// Sequence-number arithmetic modulo 2^16.
pub mod seq {
    /// `a` comes before `b`.
    pub fn lt(a: u16, b: u16) -> bool {
        a != b && b.wrapping_sub(a) < 0x8000
    }

    /// `a` comes before or is `b`.
    pub fn le(a: u16, b: u16) -> bool {
        a == b || lt(a, b)
    }

    /// How far `b` is past `a`, if it is not before it.
    pub fn distance(a: u16, b: u16) -> Option<u16> {
        le(a, b).then(|| b.wrapping_sub(a))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(ty: PacketType) -> Header {
        Header {
            ty,
            connection_id: 0x1234,
            timestamp: 0xdead_beef,
            timestamp_diff: 7,
            wnd_size: 65_536,
            seq_nr: 0xfffe,
            ack_nr: 3,
        }
    }

    #[test]
    fn round_trip_with_sack() {
        let mut sack = SelectiveAck::with_capacity(40);
        sack.set(0);
        sack.set(9);
        sack.set(39);
        let mut out = Vec::new();
        encode(&header(PacketType::State), Some(&sack), b"", &mut out);
        assert_eq!(out.len(), HEADER_LEN + 2 + 8);
        let p = Packet::decode(&out).unwrap();
        assert_eq!(p.header, header(PacketType::State));
        let s = p.sack.unwrap();
        assert!(s.get(0) && s.get(9) && s.get(39) && !s.get(1) && !s.get(64));
        assert!(p.payload.is_empty());
    }

    #[test]
    fn byte_layout_matches_bep_29() {
        let mut out = Vec::new();
        encode(&header(PacketType::Syn), None, b"xy", &mut out);
        assert_eq!(out[0], 0x41);
        assert_eq!(out[1], 0);
        assert_eq!(&out[2..4], &[0x12, 0x34]);
        assert_eq!(&out[16..20], &[0xff, 0xfe, 0, 3]);
        assert_eq!(&out[20..], b"xy");
        // The first bit of the mask is ack_nr + 2, least significant bit first.
        let s = SelectiveAck::from_bytes(&[0b0000_0101, 0, 0, 0x80]).unwrap();
        assert!(s.get(0) && !s.get(1) && s.get(2) && s.get(31));
    }

    #[test]
    fn unknown_extensions_are_skipped() {
        let mut out = Vec::new();
        encode(&header(PacketType::Data), None, b"", &mut out);
        out[1] = 2; // extension bits
        out.extend_from_slice(&[0, 8, 0, 0, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(b"data");
        let p = Packet::decode(&out).unwrap();
        assert_eq!(p.payload, b"data");
        assert!(p.sack.is_none());
    }

    #[test]
    fn bad_packets_are_refused() {
        assert_eq!(Packet::decode(&[0x41; 19]), Err(PacketError::TooShort));
        let mut out = Vec::new();
        encode(&header(PacketType::Data), None, b"", &mut out);
        out[0] = 0x02;
        assert_eq!(Packet::decode(&out), Err(PacketError::BadVersion));
        out[0] = 0x51;
        assert_eq!(Packet::decode(&out), Err(PacketError::BadType));
        out[0] = 0x01;
        out[1] = 1;
        assert_eq!(Packet::decode(&out), Err(PacketError::BadExtension));
        out.extend_from_slice(&[0, 3, 1, 2, 3]);
        assert_eq!(Packet::decode(&out), Err(PacketError::BadExtension));
    }

    #[test]
    fn classifier_rejects_dht() {
        assert!(!looks_like_utp(
            b"d1:ad2:id20:abcdefghij0123456789e1:q4:ping1:t2:aa1:y1:qe"
        ));
        let mut out = Vec::new();
        encode(&header(PacketType::Syn), None, b"", &mut out);
        assert!(looks_like_utp(&out));
    }

    #[test]
    fn sequence_numbers_wrap() {
        assert!(seq::lt(0xffff, 0));
        assert!(seq::lt(65_000, 100));
        assert!(!seq::lt(100, 65_000));
        assert!(seq::le(5, 5));
        assert_eq!(seq::distance(0xfffe, 1), Some(3));
        assert_eq!(seq::distance(1, 0xfffe), None);
    }
}

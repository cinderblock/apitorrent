//! Peer wire messages after the handshake (BEP 3, 6, 10 and 52), and an incremental decoder.

use bytes::{Buf, BufMut, Bytes, BytesMut};

use apitorrent_metainfo::Sha256Hash;

/// A block of a piece: the unit of requests (BEP 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockRef {
    pub piece: u32,
    pub offset: u32,
    pub length: u32,
}

/// A BEP 52 hash request: a range of nodes from one layer of a file's merkle tree, plus the
/// uncle hashes needed to prove them against the file's root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HashRequest {
    pub pieces_root: Sha256Hash,
    pub base_layer: u32,
    pub index: u32,
    pub length: u32,
    pub proof_layers: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    KeepAlive,
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have(u32),
    Bitfield(Bytes),
    Request(BlockRef),
    Piece {
        piece: u32,
        offset: u32,
        data: Bytes,
    },
    Cancel(BlockRef),
    /// BEP 5: the peer's DHT port.
    Port(u16),
    // BEP 6, the fast extension.
    Suggest(u32),
    HaveAll,
    HaveNone,
    Reject(BlockRef),
    AllowedFast(u32),
    /// BEP 10. `id` 0 is the extension handshake; others are as the receiver assigned them.
    Extended {
        id: u8,
        payload: Bytes,
    },
    // BEP 52.
    HashRequest(HashRequest),
    Hashes {
        request: HashRequest,
        hashes: Bytes,
    },
    HashReject(HashRequest),
    /// A message id we don't know. Ignored rather than treated as an error, so newer peers
    /// can talk to us.
    Unknown {
        id: u8,
        payload: Bytes,
    },
}

mod id {
    pub const CHOKE: u8 = 0;
    pub const UNCHOKE: u8 = 1;
    pub const INTERESTED: u8 = 2;
    pub const NOT_INTERESTED: u8 = 3;
    pub const HAVE: u8 = 4;
    pub const BITFIELD: u8 = 5;
    pub const REQUEST: u8 = 6;
    pub const PIECE: u8 = 7;
    pub const CANCEL: u8 = 8;
    pub const PORT: u8 = 9;
    pub const SUGGEST: u8 = 0x0d;
    pub const HAVE_ALL: u8 = 0x0e;
    pub const HAVE_NONE: u8 = 0x0f;
    pub const REJECT: u8 = 0x10;
    pub const ALLOWED_FAST: u8 = 0x11;
    pub const EXTENDED: u8 = 20;
    pub const HASH_REQUEST: u8 = 21;
    pub const HASHES: u8 = 22;
    pub const HASH_REJECT: u8 = 23;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    #[error("message of {len} bytes exceeds the limit of {max}")]
    TooLong { len: u32, max: u32 },
    #[error("message {id} has invalid length {len}")]
    BadLength { id: u8, len: usize },
}

impl Message {
    /// Append the encoded message to `out`. Piece data is copied; a writer that wants to
    /// avoid the copy can send [`piece_header`] followed by the data in a vectored write.
    pub fn encode(&self, out: &mut BytesMut) {
        match self {
            Message::KeepAlive => out.put_u32(0),
            Message::Choke => simple(out, id::CHOKE),
            Message::Unchoke => simple(out, id::UNCHOKE),
            Message::Interested => simple(out, id::INTERESTED),
            Message::NotInterested => simple(out, id::NOT_INTERESTED),
            Message::HaveAll => simple(out, id::HAVE_ALL),
            Message::HaveNone => simple(out, id::HAVE_NONE),
            Message::Have(i) => with_u32(out, id::HAVE, *i),
            Message::Suggest(i) => with_u32(out, id::SUGGEST, *i),
            Message::AllowedFast(i) => with_u32(out, id::ALLOWED_FAST, *i),
            Message::Bitfield(b) => with_payload(out, id::BITFIELD, b),
            Message::Request(b) => block(out, id::REQUEST, b),
            Message::Cancel(b) => block(out, id::CANCEL, b),
            Message::Reject(b) => block(out, id::REJECT, b),
            Message::Piece {
                piece,
                offset,
                data,
            } => {
                out.put_slice(&piece_header(*piece, *offset, data.len()));
                out.put_slice(data);
            }
            Message::Port(p) => {
                out.put_u32(3);
                out.put_u8(id::PORT);
                out.put_u16(*p);
            }
            Message::Extended { id: ext, payload } => {
                out.put_u32(2 + len_u32(payload.len()));
                out.put_u8(id::EXTENDED);
                out.put_u8(*ext);
                out.put_slice(payload);
            }
            Message::HashRequest(r) => hash_request(out, id::HASH_REQUEST, r, &[]),
            Message::HashReject(r) => hash_request(out, id::HASH_REJECT, r, &[]),
            Message::Hashes { request, hashes } => hash_request(out, id::HASHES, request, hashes),
            Message::Unknown {
                id: unknown,
                payload,
            } => with_payload(out, *unknown, payload),
        }
    }

    pub fn to_bytes(&self) -> Bytes {
        let mut out = BytesMut::new();
        self.encode(&mut out);
        out.freeze()
    }

    fn decode(id: u8, mut payload: Bytes) -> Result<Self, WireError> {
        let len = payload.len();
        let bad = || WireError::BadLength { id, len };
        let exact = |n: usize| if len == n { Ok(()) } else { Err(bad()) };
        Ok(match id {
            id::CHOKE => exact(0).map(|()| Message::Choke)?,
            id::UNCHOKE => exact(0).map(|()| Message::Unchoke)?,
            id::INTERESTED => exact(0).map(|()| Message::Interested)?,
            id::NOT_INTERESTED => exact(0).map(|()| Message::NotInterested)?,
            id::HAVE_ALL => exact(0).map(|()| Message::HaveAll)?,
            id::HAVE_NONE => exact(0).map(|()| Message::HaveNone)?,
            id::HAVE => {
                exact(4)?;
                Message::Have(payload.get_u32())
            }
            id::SUGGEST => {
                exact(4)?;
                Message::Suggest(payload.get_u32())
            }
            id::ALLOWED_FAST => {
                exact(4)?;
                Message::AllowedFast(payload.get_u32())
            }
            id::BITFIELD => Message::Bitfield(payload),
            id::REQUEST | id::CANCEL | id::REJECT => {
                exact(12)?;
                let b = BlockRef {
                    piece: payload.get_u32(),
                    offset: payload.get_u32(),
                    length: payload.get_u32(),
                };
                match id {
                    id::REQUEST => Message::Request(b),
                    id::CANCEL => Message::Cancel(b),
                    _ => Message::Reject(b),
                }
            }
            id::PIECE => {
                if len < 8 {
                    return Err(bad());
                }
                let piece = payload.get_u32();
                let offset = payload.get_u32();
                Message::Piece {
                    piece,
                    offset,
                    data: payload,
                }
            }
            id::PORT => {
                exact(2)?;
                Message::Port(payload.get_u16())
            }
            id::EXTENDED => {
                if len < 1 {
                    return Err(bad());
                }
                let ext = payload.get_u8();
                Message::Extended { id: ext, payload }
            }
            id::HASH_REQUEST | id::HASH_REJECT => {
                exact(48)?;
                let r = read_hash_request(&mut payload);
                if id == id::HASH_REQUEST {
                    Message::HashRequest(r)
                } else {
                    Message::HashReject(r)
                }
            }
            id::HASHES => {
                if len < 48 || !(len - 48).is_multiple_of(32) {
                    return Err(bad());
                }
                let request = read_hash_request(&mut payload);
                Message::Hashes {
                    request,
                    hashes: payload,
                }
            }
            other => Message::Unknown { id: other, payload },
        })
    }
}

/// The 13-byte header of a `piece` message carrying `len` bytes of data.
pub fn piece_header(piece: u32, offset: u32, len: usize) -> [u8; 13] {
    let mut h = [0u8; 13];
    h[..4].copy_from_slice(&(9 + len_u32(len)).to_be_bytes());
    h[4] = id::PIECE;
    h[5..9].copy_from_slice(&piece.to_be_bytes());
    h[9..13].copy_from_slice(&offset.to_be_bytes());
    h
}

fn len_u32(len: usize) -> u32 {
    u32::try_from(len).expect("message payload larger than 4 GiB")
}

fn simple(out: &mut BytesMut, id: u8) {
    out.put_u32(1);
    out.put_u8(id);
}

fn with_u32(out: &mut BytesMut, id: u8, value: u32) {
    out.put_u32(5);
    out.put_u8(id);
    out.put_u32(value);
}

fn with_payload(out: &mut BytesMut, id: u8, payload: &[u8]) {
    out.put_u32(1 + len_u32(payload.len()));
    out.put_u8(id);
    out.put_slice(payload);
}

fn block(out: &mut BytesMut, id: u8, b: &BlockRef) {
    out.put_u32(13);
    out.put_u8(id);
    out.put_u32(b.piece);
    out.put_u32(b.offset);
    out.put_u32(b.length);
}

fn hash_request(out: &mut BytesMut, id: u8, r: &HashRequest, hashes: &[u8]) {
    out.put_u32(49 + len_u32(hashes.len()));
    out.put_u8(id);
    out.put_slice(&r.pieces_root.0);
    out.put_u32(r.base_layer);
    out.put_u32(r.index);
    out.put_u32(r.length);
    out.put_u32(r.proof_layers);
    out.put_slice(hashes);
}

fn read_hash_request(payload: &mut Bytes) -> HashRequest {
    let mut root = [0u8; 32];
    payload.copy_to_slice(&mut root);
    HashRequest {
        pieces_root: Sha256Hash(root),
        base_layer: payload.get_u32(),
        index: payload.get_u32(),
        length: payload.get_u32(),
        proof_layers: payload.get_u32(),
    }
}

/// Splits an incoming byte stream into messages.
///
/// Read socket data straight into [`Decoder::buffer`], then call [`Decoder::next`] until it
/// returns `Ok(None)`. Piece payloads are split off the buffer without copying.
#[derive(Debug)]
pub struct Decoder {
    buf: BytesMut,
    max_len: u32,
}

/// The largest block a peer may send or request. BEP 3 says 16 KiB, but some clients use
/// up to 128 KiB, and we accept those.
pub const MAX_BLOCK_LEN: u32 = 128 * 1024;

impl Decoder {
    /// `max_len` bounds a single message. It must cover the largest bitfield the torrent
    /// can have, so use [`Decoder::max_len_for`].
    pub fn new(max_len: u32) -> Self {
        Self {
            buf: BytesMut::with_capacity(64 * 1024),
            max_len,
        }
    }

    /// The message size limit for a torrent with `piece_count` pieces: room for a full
    /// bitfield, a maximal block, or a large extension message (metadata pieces are 16 KiB,
    /// PEX is a few KiB).
    pub fn max_len_for(piece_count: u64) -> u32 {
        let bitfield = u32::try_from(piece_count.div_ceil(8)).unwrap_or(u32::MAX);
        bitfield.max(MAX_BLOCK_LEN).saturating_add(64)
    }

    pub fn set_max_len(&mut self, max_len: u32) {
        self.max_len = max_len;
    }

    /// The receive buffer, for reading socket data into.
    pub fn buffer(&mut self) -> &mut BytesMut {
        &mut self.buf
    }

    /// Bytes received but not yet decoded.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    pub fn extend(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// The next complete message, or `None` if more data is needed.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Message>, WireError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes(self.buf[..4].try_into().expect("4 bytes"));
        if len > self.max_len {
            return Err(WireError::TooLong {
                len,
                max: self.max_len,
            });
        }
        let total = 4 + len as usize;
        if self.buf.len() < total {
            self.buf.reserve(total - self.buf.len());
            return Ok(None);
        }
        self.buf.advance(4);
        if len == 0 {
            return Ok(Some(Message::KeepAlive));
        }
        let mut frame = self.buf.split_to(len as usize).freeze();
        let id = frame.get_u8();
        Message::decode(id, frame).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_messages() -> Vec<Message> {
        let b = BlockRef {
            piece: 3,
            offset: 16384,
            length: 16384,
        };
        let r = HashRequest {
            pieces_root: Sha256Hash([5; 32]),
            base_layer: 0,
            index: 8,
            length: 4,
            proof_layers: 2,
        };
        vec![
            Message::KeepAlive,
            Message::Choke,
            Message::Unchoke,
            Message::Interested,
            Message::NotInterested,
            Message::Have(42),
            Message::Bitfield(Bytes::from_static(&[0xff, 0x80])),
            Message::Request(b),
            Message::Piece {
                piece: 3,
                offset: 0,
                data: Bytes::from(vec![1u8; 100]),
            },
            Message::Cancel(b),
            Message::Port(6881),
            Message::Suggest(9),
            Message::HaveAll,
            Message::HaveNone,
            Message::Reject(b),
            Message::AllowedFast(11),
            Message::Extended {
                id: 0,
                payload: Bytes::from_static(b"de"),
            },
            Message::HashRequest(r),
            Message::Hashes {
                request: r,
                hashes: Bytes::from(vec![7u8; 64]),
            },
            Message::HashReject(r),
            Message::Unknown {
                id: 99,
                payload: Bytes::from_static(b"x"),
            },
        ]
    }

    #[test]
    fn round_trip_in_one_stream() {
        let messages = all_messages();
        let mut stream = BytesMut::new();
        for m in &messages {
            m.encode(&mut stream);
        }
        let mut d = Decoder::new(Decoder::max_len_for(1000));
        // Feed one byte at a time to exercise partial frames.
        let mut out = Vec::new();
        for byte in &stream {
            d.extend(&[*byte]);
            while let Some(m) = d.next().unwrap() {
                out.push(m);
            }
        }
        assert_eq!(out, messages);
        assert_eq!(d.pending(), 0);
    }

    #[test]
    fn piece_header_matches_encoding() {
        let m = Message::Piece {
            piece: 1,
            offset: 2,
            data: Bytes::from_static(b"abc"),
        };
        let bytes = m.to_bytes();
        assert_eq!(&bytes[..13], &piece_header(1, 2, 3));
    }

    #[test]
    fn limits_and_bad_lengths() {
        let mut d = Decoder::new(100);
        d.extend(&1000u32.to_be_bytes());
        assert_eq!(
            d.next(),
            Err(WireError::TooLong {
                len: 1000,
                max: 100
            })
        );

        let mut d = Decoder::new(100);
        d.extend(&[0, 0, 0, 3, id::HAVE, 0, 0]);
        assert_eq!(
            d.next(),
            Err(WireError::BadLength {
                id: id::HAVE,
                len: 2
            })
        );

        let mut d = Decoder::new(100);
        d.extend(&[0, 0, 0, 2, id::CHOKE, 0]);
        assert_eq!(
            d.next(),
            Err(WireError::BadLength {
                id: id::CHOKE,
                len: 1
            })
        );
    }

    #[test]
    fn max_len_covers_big_bitfields() {
        assert!(Decoder::max_len_for(1 << 24) > (1 << 21));
        assert!(Decoder::max_len_for(10) >= MAX_BLOCK_LEN + 13);
    }
}

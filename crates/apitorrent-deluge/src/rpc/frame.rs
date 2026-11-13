//! Deluge 2's message framing (`deluge/transfer.py`).
//!
//! Each message is a 5-byte header, then the body:
//!
//! ```text
//! | version: u8 = 1 | length: u32 big-endian | body: zlib(rencode(value)) |
//! ```
//!
//! (Deluge 2.0's betas used a `D` byte here; 2.0.0 and later send the protocol version, 1.)
//! The body is a zlib stream (RFC 1950), as Python's `zlib.compress` writes it.

use std::io::{Read, Write};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;

use super::rencode::{self, Value};

/// The protocol version byte Deluge 2 sends and expects.
pub const PROTOCOL_VERSION: u8 = 1;
pub const HEADER_LEN: usize = 5;

/// Bounds on a message we'll read.
#[derive(Debug, Clone, Copy)]
pub struct FrameLimits {
    /// The most compressed bytes one message may carry.
    pub max_compressed: usize,
    /// The most bytes one message may inflate to. Bounds a zlib bomb.
    pub max_decompressed: usize,
    pub rencode: rencode::Limits,
}

impl Default for FrameLimits {
    fn default() -> Self {
        // A status listing of a few thousand torrents with their trackers is a few MB of
        // rencode; give it plenty of room without letting a stranger make us allocate GBs.
        Self {
            max_compressed: 64 << 20,
            max_decompressed: 256 << 20,
            rencode: rencode::Limits::default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("unknown Deluge protocol version {0} (expected {PROTOCOL_VERSION})")]
    Version(u8),
    #[error("message of {len} bytes is larger than the {max}-byte limit")]
    TooLarge { len: usize, max: usize },
    #[error("message inflates past the {0}-byte limit")]
    InflatesTooLarge(usize),
    #[error("message is cut short")]
    Truncated,
    #[error("message body is not valid zlib: {0}")]
    Zlib(std::io::Error),
    #[error(transparent)]
    Rencode(#[from] rencode::DecodeError),
    #[error(transparent)]
    Encode(#[from] rencode::EncodeError),
}

/// Encode a value as one complete message, header included.
pub fn encode(v: &Value) -> Result<Vec<u8>, FrameError> {
    let raw = rencode::encode(v)?;
    let mut z = ZlibEncoder::new(
        Vec::with_capacity(raw.len() / 2 + 16),
        Compression::default(),
    );
    z.write_all(&raw).expect("writing to a Vec");
    let body = z.finish().expect("writing to a Vec");
    let len = u32::try_from(body.len()).map_err(|_| FrameError::TooLarge {
        len: body.len(),
        max: u32::MAX as usize,
    })?;
    let mut out = Vec::with_capacity(HEADER_LEN + body.len());
    out.push(PROTOCOL_VERSION);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Check a header and return the body length that follows it.
pub fn parse_header(h: [u8; HEADER_LEN], limits: &FrameLimits) -> Result<usize, FrameError> {
    if h[0] != PROTOCOL_VERSION {
        return Err(FrameError::Version(h[0]));
    }
    let len = u32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
    if len > limits.max_compressed {
        return Err(FrameError::TooLarge {
            len,
            max: limits.max_compressed,
        });
    }
    Ok(len)
}

/// Inflate and decode a message body.
pub fn decode_body(body: &[u8], limits: &FrameLimits) -> Result<Value, FrameError> {
    let mut raw = Vec::new();
    let max = limits.max_decompressed;
    ZlibDecoder::new(body)
        .take(max as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(FrameError::Zlib)?;
    if raw.len() > max {
        return Err(FrameError::InflatesTooLarge(max));
    }
    Ok(rencode::decode(&raw, limits.rencode)?)
}

/// Decode one complete message (header and body) from `data`, which must hold exactly one.
/// For tests and fuzzing; the client reads the header and body off the socket separately.
pub fn decode(data: &[u8], limits: &FrameLimits) -> Result<Value, FrameError> {
    let header: [u8; HEADER_LEN] = data
        .get(..HEADER_LEN)
        .and_then(|h| h.try_into().ok())
        .ok_or(FrameError::Truncated)?;
    let len = parse_header(header, limits)?;
    let body = &data[HEADER_LEN..];
    if body.len() != len {
        return Err(FrameError::Truncated);
    }
    decode_body(body, limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> Value {
        Value::Str(x.to_owned())
    }

    #[test]
    fn python_message_decodes() {
        // `DelugeTransferProtocol.transfer_message((1, 0, 10))` in Deluge 2.1.1 with Python
        // 3.12: struct.pack('!BI', 1, len) + zlib.compress(rencode.dumps((1, 0, 10))).
        let msg = hex::decode("010000000c789c3bccc8c00500031d00cf").unwrap();
        let v = decode(&msg, &FrameLimits::default()).unwrap();
        assert_eq!(
            v,
            Value::List(vec![Value::Int(1), Value::Int(0), Value::Int(10)])
        );
    }

    #[test]
    fn python_error_decodes() {
        // (2, 3, "BadLoginError", ("Password does not match", "localclient"), {}, "Traceback...")
        let msg = hex::decode(concat!(
            "010000004c789c3bc6c4dceb9498e2939f9e99e75a54945f74687a40627171797e518a424a7e6ab1",
            "425e7e89426e62497246774e7e72624e724e666a5e495a4f485162726a526272b69e9e1e009fe01a89",
        ))
        .unwrap();
        let v = decode(&msg, &FrameLimits::default()).unwrap();
        let l = v.as_list().unwrap();
        assert_eq!(l.len(), 6);
        assert_eq!(l[2].as_str(), Some("BadLoginError"));
    }

    #[test]
    fn round_trip() {
        let v = Value::List(vec![Value::List(vec![
            Value::Int(7),
            s("daemon.info"),
            Value::List(vec![]),
            Value::Dict(vec![]),
        ])]);
        let msg = encode(&v).unwrap();
        assert_eq!(msg[0], 1);
        assert_eq!(
            u32::from_be_bytes(msg[1..5].try_into().unwrap()) as usize,
            msg.len() - 5
        );
        assert_eq!(decode(&msg, &FrameLimits::default()).unwrap(), v);
    }

    #[test]
    fn rejects_other_versions() {
        let mut msg = encode(&Value::None).unwrap();
        msg[0] = b'D';
        assert!(matches!(
            decode(&msg, &FrameLimits::default()),
            Err(FrameError::Version(b'D'))
        ));
    }

    #[test]
    fn bounds_sizes() {
        let limits = FrameLimits {
            max_compressed: 100,
            max_decompressed: 1000,
            ..FrameLimits::default()
        };
        assert!(matches!(
            parse_header([1, 0, 0, 0, 101], &limits),
            Err(FrameError::TooLarge { len: 101, .. })
        ));
        // 2000 zero bytes compress to a few bytes but inflate past the limit.
        let bomb = encode(&Value::Bytes(vec![0; 2000])).unwrap();
        assert!(bomb.len() < 100);
        assert!(matches!(
            decode(&bomb, &limits),
            Err(FrameError::InflatesTooLarge(1000))
        ));
        assert!(matches!(
            decode(&[1, 0, 0, 0, 2, 0, 0], &limits),
            Err(FrameError::Zlib(_))
        ));
        assert!(matches!(
            decode(&[1, 0, 0, 0, 3, 0, 0], &limits),
            Err(FrameError::Truncated)
        ));
        assert!(matches!(
            decode(&[1, 0], &limits),
            Err(FrameError::Truncated)
        ));
    }
}

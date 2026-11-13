//! rencode, the serialisation Deluge's daemon RPC speaks.
//!
//! Written from the reference implementation Deluge 2 depends on (the `rencode` package,
//! <https://github.com/aresch/rencode>, 1.0.x: `_rencode.pyx`, which Deluge loads, and
//! `rencode_orig.py`). Every value starts with one type byte:
//!
//! | Byte | Meaning |
//! | --- | --- |
//! | `0x00..=0x2B` | integer 0 to 43, in the byte itself |
//! | `0x2C` | float64, 8 bytes big-endian |
//! | `'1'..='9'` | string of more than 63 bytes: ASCII length, `:`, bytes |
//! | `0x3B` | list, items until `0x7F` |
//! | `0x3C` | dict, key/value pairs until `0x7F` |
//! | `0x3D` | integer as ASCII decimal (under 64 characters), then `0x7F` |
//! | `0x3E` / `0x3F` / `0x40` / `0x41` | signed integer, 1 / 2 / 4 / 8 bytes big-endian |
//! | `0x42` | float32, 4 bytes big-endian |
//! | `0x43` / `0x44` / `0x45` | `True` / `False` / `None` |
//! | `0x46..=0x65` | integer -1 to -32, in the byte itself |
//! | `0x66..=0x7E` | dict of 0 to 24 pairs |
//! | `0x80..=0xBF` | string of 0 to 63 bytes |
//! | `0xC0..=0xFF` | list of 0 to 63 items |
//!
//! Python's encoder picks the shortest form; this one does the same, so its output is byte for
//! byte what Deluge would send. Python encodes `float` as float32 by default (Deluge never asks
//! for 64); this encoder writes [`Value::Float`] as float64, which Deluge reads back exactly.
//!
//! Decoding is for network input: it never panics, never recurses deeper than
//! [`Limits::max_depth`], never allocates more than the input could fill, and rejects
//! anything the reference decoder would reject, plus trailing bytes. It accepts the
//! non-shortest forms the reference decoder accepts (a 5-byte string written `5:abcde`).

use std::fmt;

const CHR_LIST: u8 = 59;
const CHR_DICT: u8 = 60;
const CHR_INT: u8 = 61;
const CHR_INT1: u8 = 62;
const CHR_INT2: u8 = 63;
const CHR_INT4: u8 = 64;
const CHR_INT8: u8 = 65;
const CHR_FLOAT32: u8 = 66;
const CHR_FLOAT64: u8 = 44;
const CHR_TRUE: u8 = 67;
const CHR_FALSE: u8 = 68;
const CHR_NONE: u8 = 69;
const CHR_TERM: u8 = 127;

const INT_POS_FIXED_START: u8 = 0;
const INT_POS_FIXED_COUNT: u8 = 44;
const INT_NEG_FIXED_START: u8 = 70;
const INT_NEG_FIXED_COUNT: u8 = 32;
const DICT_FIXED_START: u8 = 102;
const DICT_FIXED_COUNT: u8 = 25;
const STR_FIXED_START: u8 = 128;
const STR_FIXED_COUNT: u8 = 64;
const LIST_FIXED_START: u8 = STR_FIXED_START + STR_FIXED_COUNT;
const LIST_FIXED_COUNT: u8 = 64;

/// rencode refuses integers of 64 or more decimal characters.
const MAX_INT_LENGTH: usize = 64;

/// A decoded rencode value.
///
/// rencode has one string type; Deluge decodes every string as UTF-8. A string that is valid
/// UTF-8 decodes to [`Value::Str`], anything else to [`Value::Bytes`]. Both encode the same
/// way. Python's tuples and lists are both [`Value::List`].
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    None,
    Bool(bool),
    Int(i64),
    /// An integer outside `i64`, as its decimal digits (with a leading `-` when negative).
    BigInt(String),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    /// Pairs in the order they were read (or are to be written). Decoding rejects a
    /// repeated key.
    Dict(Vec<(Value, Value)>),
}

impl Value {
    /// A dict from string keys, in the given order.
    pub fn dict<K: Into<String>>(pairs: impl IntoIterator<Item = (K, Value)>) -> Value {
        Value::Dict(
            pairs
                .into_iter()
                .map(|(k, v)| (Value::Str(k.into()), v))
                .collect(),
        )
    }

    /// A list of strings.
    pub fn str_list<S: AsRef<str>>(items: impl IntoIterator<Item = S>) -> Value {
        Value::List(
            items
                .into_iter()
                .map(|s| Value::Str(s.as_ref().to_owned()))
                .collect(),
        )
    }

    /// The value under string key `key`, when this is a dict.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Dict(pairs) => pairs
                .iter()
                .find(|(k, _)| k.as_str() == Some(key))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// The number as a float, for a float or an integer.
    #[allow(clippy::cast_precision_loss)]
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            Value::Int(i) => Some(*i as f64),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&[(Value, Value)]> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }

    pub fn is_none(&self) -> bool {
        matches!(self, Value::None)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(s.to_owned())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(s)
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<f64> for Value {
    fn from(f: f64) -> Self {
        Value::Float(f)
    }
}

impl From<Vec<Value>> for Value {
    fn from(l: Vec<Value>) -> Self {
        Value::List(l)
    }
}

/// Bounds on what [`decode`] accepts.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// How deeply lists and dicts may nest. A value that isn't a container is depth 0.
    pub max_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        // Deluge's deepest reply (torrent status → trackers → tracker dict) is 4 deep.
        Self { max_depth: 32 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError {
    /// Offset of the byte the problem was found at.
    pub at: usize,
    pub reason: &'static str,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bad rencode at byte {}: {}", self.at, self.reason)
    }
}

impl std::error::Error for DecodeError {}

/// Decode exactly one value, which must fill `data`.
pub fn decode(data: &[u8], limits: Limits) -> Result<Value, DecodeError> {
    let mut d = Decoder {
        data,
        pos: 0,
        limits,
    };
    let v = d.value(0)?;
    if d.pos != data.len() {
        return Err(d.err("trailing bytes after the value"));
    }
    Ok(v)
}

struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
    limits: Limits,
}

impl Decoder<'_> {
    fn err(&self, reason: &'static str) -> DecodeError {
        DecodeError {
            at: self.pos,
            reason,
        }
    }

    fn peek(&self) -> Result<u8, DecodeError> {
        self.data
            .get(self.pos)
            .copied()
            .ok_or_else(|| self.err("unexpected end of input"))
    }

    fn take(&mut self, n: usize) -> Result<&[u8], DecodeError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.data.len())
            .ok_or_else(|| self.err("unexpected end of input"))?;
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn take_array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let s = self.take(N)?;
        Ok(s.try_into().expect("took N bytes"))
    }

    fn container_depth(&self, depth: usize) -> Result<usize, DecodeError> {
        let d = depth + 1;
        if d > self.limits.max_depth {
            return Err(self.err("nested too deeply"));
        }
        Ok(d)
    }

    fn value(&mut self, depth: usize) -> Result<Value, DecodeError> {
        let start = self.pos;
        let t = self.peek()?;
        self.pos += 1;
        Ok(match t {
            _ if t < INT_POS_FIXED_START + INT_POS_FIXED_COUNT => {
                Value::Int(i64::from(t - INT_POS_FIXED_START))
            }
            _ if (INT_NEG_FIXED_START..INT_NEG_FIXED_START + INT_NEG_FIXED_COUNT).contains(&t) => {
                Value::Int(-1 - i64::from(t - INT_NEG_FIXED_START))
            }
            CHR_INT1 => Value::Int(i64::from(i8::from_be_bytes(self.take_array()?))),
            CHR_INT2 => Value::Int(i64::from(i16::from_be_bytes(self.take_array()?))),
            CHR_INT4 => Value::Int(i64::from(i32::from_be_bytes(self.take_array()?))),
            CHR_INT8 => Value::Int(i64::from_be_bytes(self.take_array()?)),
            CHR_INT => self.big_int()?,
            CHR_FLOAT32 => Value::Float(f64::from(f32::from_be_bytes(self.take_array()?))),
            CHR_FLOAT64 => Value::Float(f64::from_be_bytes(self.take_array()?)),
            CHR_TRUE => Value::Bool(true),
            CHR_FALSE => Value::Bool(false),
            CHR_NONE => Value::None,
            b'1'..=b'9' => {
                self.pos = start;
                let n = self.str_len()?;
                string(self.take(n)?)
            }
            _ if (STR_FIXED_START..STR_FIXED_START + STR_FIXED_COUNT).contains(&t) => {
                string(self.take(usize::from(t - STR_FIXED_START))?)
            }
            _ if t >= LIST_FIXED_START => {
                let depth = self.container_depth(depth)?;
                let n = usize::from(t - LIST_FIXED_START);
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(self.value(depth)?);
                }
                Value::List(items)
            }
            CHR_LIST => {
                let depth = self.container_depth(depth)?;
                let mut items = Vec::new();
                while self.peek()? != CHR_TERM {
                    items.push(self.value(depth)?);
                }
                self.pos += 1;
                Value::List(items)
            }
            _ if (DICT_FIXED_START..DICT_FIXED_START + DICT_FIXED_COUNT).contains(&t) => {
                let depth = self.container_depth(depth)?;
                let n = usize::from(t - DICT_FIXED_START);
                let mut pairs = Vec::with_capacity(n);
                for _ in 0..n {
                    self.pair(depth, &mut pairs)?;
                }
                Value::Dict(pairs)
            }
            CHR_DICT => {
                let depth = self.container_depth(depth)?;
                let mut pairs = Vec::new();
                while self.peek()? != CHR_TERM {
                    self.pair(depth, &mut pairs)?;
                }
                self.pos += 1;
                Value::Dict(pairs)
            }
            _ => {
                self.pos = start;
                return Err(self.err("unknown type byte"));
            }
        })
    }

    fn pair(&mut self, depth: usize, pairs: &mut Vec<(Value, Value)>) -> Result<(), DecodeError> {
        let at = self.pos;
        let k = self.value(depth)?;
        if matches!(k, Value::List(_) | Value::Dict(_) | Value::Float(_)) {
            // Python can't key a dict by a list or dict (unhashable); a float key would
            // decode, but Deluge never sends one and equality on floats is a trap.
            return Err(DecodeError {
                at,
                reason: "dict key is not a string, integer, bool or None",
            });
        }
        if pairs.iter().any(|(existing, _)| *existing == k) {
            return Err(DecodeError {
                at,
                reason: "repeated dict key",
            });
        }
        let v = self.value(depth)?;
        pairs.push((k, v));
        Ok(())
    }

    /// The `<digits>:` before a long string. The first digit is 1 to 9.
    fn str_len(&mut self) -> Result<usize, DecodeError> {
        let start = self.pos;
        let mut n: usize = 0;
        loop {
            let c = self.peek()?;
            self.pos += 1;
            match c {
                b':' if self.pos - start > 1 => return Ok(n),
                b'0'..=b'9' => {
                    n = n
                        .checked_mul(10)
                        .and_then(|n| n.checked_add(usize::from(c - b'0')))
                        .filter(|&n| n <= self.data.len())
                        .ok_or_else(|| self.err("string longer than the input"))?;
                }
                _ => return Err(self.err("bad string length")),
            }
        }
    }

    /// `0x3D`, then an optional `-` and decimal digits without leading zeros, then `0x7F`.
    fn big_int(&mut self) -> Result<Value, DecodeError> {
        let start = self.pos;
        let rest = &self.data[start..];
        let len = rest
            .iter()
            .take(MAX_INT_LENGTH)
            .position(|&b| b == CHR_TERM)
            .ok_or_else(|| self.err("integer too long or unterminated"))?;
        let text = &rest[..len];
        let digits = text.strip_prefix(b"-").unwrap_or(text);
        let valid = !digits.is_empty()
            && digits.iter().all(u8::is_ascii_digit)
            && (digits[0] != b'0' || (digits.len() == 1 && text.len() == 1));
        if !valid {
            return Err(self.err("bad decimal integer"));
        }
        self.pos = start + len + 1;
        let text = std::str::from_utf8(text).expect("ASCII digits");
        Ok(match text.parse::<i64>() {
            Ok(i) => Value::Int(i),
            Err(_) => Value::BigInt(text.to_owned()),
        })
    }
}

fn string(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => Value::Str(s.to_owned()),
        Err(_) => Value::Bytes(b.to_vec()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodeError(pub &'static str);

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "can't rencode: {}", self.0)
    }
}

impl std::error::Error for EncodeError {}

/// Encode a value, in the same bytes Python's `rencode.dumps` would write (but with floats as
/// float64).
pub fn encode(v: &Value) -> Result<Vec<u8>, EncodeError> {
    let mut out = Vec::new();
    encode_into(v, &mut out)?;
    Ok(out)
}

pub fn encode_into(v: &Value, out: &mut Vec<u8>) -> Result<(), EncodeError> {
    match v {
        Value::None => out.push(CHR_NONE),
        Value::Bool(true) => out.push(CHR_TRUE),
        Value::Bool(false) => out.push(CHR_FALSE),
        Value::Int(i) => encode_int(*i, out),
        Value::BigInt(s) => {
            let digits = s.strip_prefix('-').unwrap_or(s);
            if s.len() >= MAX_INT_LENGTH
                || digits.is_empty()
                || !digits.bytes().all(|b| b.is_ascii_digit())
            {
                return Err(EncodeError(
                    "BigInt is not a decimal integer under 64 characters",
                ));
            }
            if let Ok(i) = s.parse::<i64>() {
                encode_int(i, out);
            } else {
                out.push(CHR_INT);
                out.extend_from_slice(s.as_bytes());
                out.push(CHR_TERM);
            }
        }
        Value::Float(f) => {
            out.push(CHR_FLOAT64);
            out.extend_from_slice(&f.to_be_bytes());
        }
        Value::Str(s) => encode_str(s.as_bytes(), out),
        Value::Bytes(b) => encode_str(b, out),
        Value::List(items) => {
            if let Some(n) = fixed(items.len(), LIST_FIXED_COUNT) {
                out.push(LIST_FIXED_START + n);
                for i in items {
                    encode_into(i, out)?;
                }
            } else {
                out.push(CHR_LIST);
                for i in items {
                    encode_into(i, out)?;
                }
                out.push(CHR_TERM);
            }
        }
        Value::Dict(pairs) => {
            if let Some(n) = fixed(pairs.len(), DICT_FIXED_COUNT) {
                out.push(DICT_FIXED_START + n);
            } else {
                out.push(CHR_DICT);
            }
            for (k, v) in pairs {
                encode_into(k, out)?;
                encode_into(v, out)?;
            }
            if fixed(pairs.len(), DICT_FIXED_COUNT).is_none() {
                out.push(CHR_TERM);
            }
        }
    }
    Ok(())
}

fn fixed(len: usize, count: u8) -> Option<u8> {
    u8::try_from(len).ok().filter(|&n| n < count)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn encode_int(i: i64, out: &mut Vec<u8>) {
    if (0..i64::from(INT_POS_FIXED_COUNT)).contains(&i) {
        out.push(INT_POS_FIXED_START + i as u8);
    } else if (-i64::from(INT_NEG_FIXED_COUNT)..0).contains(&i) {
        out.push(INT_NEG_FIXED_START + (-1 - i) as u8);
    } else if let Ok(b) = i8::try_from(i) {
        out.push(CHR_INT1);
        out.extend_from_slice(&b.to_be_bytes());
    } else if let Ok(h) = i16::try_from(i) {
        out.push(CHR_INT2);
        out.extend_from_slice(&h.to_be_bytes());
    } else if let Ok(l) = i32::try_from(i) {
        out.push(CHR_INT4);
        out.extend_from_slice(&l.to_be_bytes());
    } else {
        out.push(CHR_INT8);
        out.extend_from_slice(&i.to_be_bytes());
    }
}

fn encode_str(b: &[u8], out: &mut Vec<u8>) {
    if let Some(n) = fixed(b.len(), STR_FIXED_COUNT) {
        out.push(STR_FIXED_START + n);
    } else {
        out.extend_from_slice(b.len().to_string().as_bytes());
        out.push(b':');
    }
    out.extend_from_slice(b);
}

#[cfg(test)]
mod tests;

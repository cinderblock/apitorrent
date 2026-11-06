use crate::value::{Dict, List, Value};

/// Why decoding failed. Every variant carries the byte offset where the problem was found.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("unexpected end of input at offset {0}")]
    UnexpectedEof(usize),
    #[error("unexpected byte {byte:#04x} at offset {offset}")]
    UnexpectedByte { offset: usize, byte: u8 },
    #[error("malformed integer at offset {0}")]
    MalformedInt(usize),
    #[error("integer out of range at offset {0}")]
    IntOutOfRange(usize),
    #[error("malformed string length at offset {0}")]
    MalformedLength(usize),
    #[error("nesting deeper than {max} levels at offset {offset}")]
    TooDeep { offset: usize, max: usize },
    #[error("dictionary key is not a byte string at offset {0}")]
    NonStringKey(usize),
    #[error("dictionary keys out of order at offset {0}")]
    UnsortedKeys(usize),
    #[error("duplicate dictionary key at offset {0}")]
    DuplicateKey(usize),
    #[error("trailing data at offset {0}")]
    TrailingData(usize),
}

impl Error {
    /// The byte offset in the input where decoding failed.
    pub fn offset(&self) -> usize {
        match *self {
            Error::UnexpectedEof(o)
            | Error::MalformedInt(o)
            | Error::IntOutOfRange(o)
            | Error::MalformedLength(o)
            | Error::NonStringKey(o)
            | Error::UnsortedKeys(o)
            | Error::DuplicateKey(o)
            | Error::TrailingData(o)
            | Error::UnexpectedByte { offset: o, .. }
            | Error::TooDeep { offset: o, .. } => o,
        }
    }
}

/// Decoder settings. [`DecodeOptions::default`] is strict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeOptions {
    /// The deepest nesting of lists and dictionaries allowed. A `.torrent` with a v2 file tree
    /// nests one level per directory, so this must leave room for deep paths.
    pub max_depth: usize,
    /// Reject dictionaries whose keys are not in ascending byte order. Off is lenient but
    /// still safe: entries are sorted after decoding, and duplicates are always rejected.
    pub require_sorted_keys: bool,
}

impl Default for DecodeOptions {
    fn default() -> Self {
        Self {
            max_depth: 128,
            require_sorted_keys: true,
        }
    }
}

impl DecodeOptions {
    /// Settings for `.torrent` files found in the wild: keys out of order are accepted.
    pub fn lenient() -> Self {
        Self {
            require_sorted_keys: false,
            ..Self::default()
        }
    }
}

/// Decode one value that must span the whole input, with strict default options.
pub fn decode(input: &[u8]) -> Result<Value<'_>, Error> {
    decode_with(input, &DecodeOptions::default())
}

/// Decode one value that must span the whole input.
pub fn decode_with<'a>(input: &'a [u8], options: &DecodeOptions) -> Result<Value<'a>, Error> {
    let (value, consumed) = decode_prefix(input, options)?;
    if consumed != input.len() {
        return Err(Error::TrailingData(consumed));
    }
    Ok(value)
}

/// Decode one value from the start of the input, and return it with the number of bytes it
/// used. Some protocol messages, such as BEP 9 metadata pieces, put raw data after a
/// bencoded dictionary.
pub fn decode_prefix<'a>(
    input: &'a [u8],
    options: &DecodeOptions,
) -> Result<(Value<'a>, usize), Error> {
    let mut d = Decoder {
        input,
        pos: 0,
        options,
    };
    let value = d.value(0)?;
    Ok((value, d.pos))
}

struct Decoder<'a, 'o> {
    input: &'a [u8],
    pos: usize,
    options: &'o DecodeOptions,
}

impl<'a> Decoder<'a, '_> {
    fn peek(&self) -> Result<u8, Error> {
        self.input
            .get(self.pos)
            .copied()
            .ok_or(Error::UnexpectedEof(self.pos))
    }

    fn value(&mut self, depth: usize) -> Result<Value<'a>, Error> {
        match self.peek()? {
            b'i' => self.int().map(Value::Int),
            b'0'..=b'9' => self.bytes().map(Value::Bytes),
            b'l' => self.list(depth).map(Value::List),
            b'd' => self.dict(depth).map(Value::Dict),
            byte => Err(Error::UnexpectedByte {
                offset: self.pos,
                byte,
            }),
        }
    }

    fn enter(&self, depth: usize) -> Result<usize, Error> {
        let next = depth + 1;
        if next > self.options.max_depth {
            return Err(Error::TooDeep {
                offset: self.pos,
                max: self.options.max_depth,
            });
        }
        Ok(next)
    }

    fn int(&mut self) -> Result<i64, Error> {
        let start = self.pos;
        self.pos += 1; // 'i'
        let rest = &self.input[self.pos..];
        let end = rest
            .iter()
            .position(|&b| b == b'e')
            .ok_or(Error::UnexpectedEof(self.input.len()))?;
        let text = &rest[..end];
        let value = parse_int(text).map_err(|kind| match kind {
            IntError::Malformed => Error::MalformedInt(start),
            IntError::OutOfRange => Error::IntOutOfRange(start),
        })?;
        self.pos += end + 1;
        Ok(value)
    }

    fn bytes(&mut self) -> Result<&'a [u8], Error> {
        let start = self.pos;
        let rest = &self.input[self.pos..];
        let colon = rest
            .iter()
            .position(|&b| b == b':')
            .ok_or(Error::UnexpectedEof(self.input.len()))?;
        let digits = &rest[..colon];
        let len = parse_len(digits).ok_or(Error::MalformedLength(start))?;
        let data_start = self.pos + colon + 1;
        let data_end = data_start
            .checked_add(len)
            .filter(|&end| end <= self.input.len())
            .ok_or(Error::UnexpectedEof(self.input.len()))?;
        self.pos = data_end;
        Ok(&self.input[data_start..data_end])
    }

    fn list(&mut self, depth: usize) -> Result<List<'a>, Error> {
        let depth = self.enter(depth)?;
        let start = self.pos;
        self.pos += 1; // 'l'
        let mut items = Vec::new();
        while self.peek()? != b'e' {
            items.push(self.value(depth)?);
        }
        self.pos += 1;
        Ok(List {
            items,
            raw: &self.input[start..self.pos],
        })
    }

    fn dict(&mut self, depth: usize) -> Result<Dict<'a>, Error> {
        let depth = self.enter(depth)?;
        let start = self.pos;
        self.pos += 1; // 'd'
        let mut entries: Vec<(&'a [u8], Value<'a>)> = Vec::new();
        let mut sorted = true;
        while self.peek()? != b'e' {
            let key_offset = self.pos;
            if !self.peek()?.is_ascii_digit() {
                return Err(Error::NonStringKey(key_offset));
            }
            let key = self.bytes()?;
            if let Some((prev, _)) = entries.last() {
                match (*prev).cmp(key) {
                    std::cmp::Ordering::Less => {}
                    std::cmp::Ordering::Equal => return Err(Error::DuplicateKey(key_offset)),
                    std::cmp::Ordering::Greater => {
                        if self.options.require_sorted_keys {
                            return Err(Error::UnsortedKeys(key_offset));
                        }
                        sorted = false;
                    }
                }
            }
            let value = self.value(depth)?;
            entries.push((key, value));
        }
        self.pos += 1;
        if !sorted {
            entries.sort_by(|a, b| a.0.cmp(b.0));
            if entries.windows(2).any(|w| w[0].0 == w[1].0) {
                // The offset of the second occurrence is gone after sorting; the dictionary's
                // own offset is the most useful position left.
                return Err(Error::DuplicateKey(start));
            }
        }
        Ok(Dict {
            entries,
            raw: &self.input[start..self.pos],
            sorted_in_input: sorted,
        })
    }
}

enum IntError {
    Malformed,
    OutOfRange,
}

fn parse_int(text: &[u8]) -> Result<i64, IntError> {
    let (negative, digits) = match text.split_first() {
        Some((b'-', rest)) => (true, rest),
        _ => (false, text),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(IntError::Malformed);
    }
    // Canonical form: no leading zeros, and no negative zero.
    if digits[0] == b'0' && (digits.len() > 1 || negative) {
        return Err(IntError::Malformed);
    }
    // i64 has at most 19 digits; anything longer is out of range (and not worth parsing).
    if digits.len() > 19 {
        return Err(IntError::OutOfRange);
    }
    let mut value: i128 = 0;
    for &d in digits {
        value = value * 10 + i128::from(d - b'0');
    }
    if negative {
        value = -value;
    }
    i64::try_from(value).map_err(|_| IntError::OutOfRange)
}

fn parse_len(digits: &[u8]) -> Option<usize> {
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if digits[0] == b'0' && digits.len() > 1 {
        return None;
    }
    let mut value: usize = 0;
    for &d in digits {
        value = value.checked_mul(10)?.checked_add(usize::from(d - b'0'))?;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(input: &[u8]) -> Error {
        decode(input).unwrap_err()
    }

    #[test]
    fn integers() {
        assert_eq!(decode(b"i0e").unwrap(), Value::Int(0));
        assert_eq!(decode(b"i42e").unwrap(), Value::Int(42));
        assert_eq!(decode(b"i-42e").unwrap(), Value::Int(-42));
        assert_eq!(
            decode(b"i9223372036854775807e").unwrap(),
            Value::Int(i64::MAX)
        );
        assert_eq!(
            decode(b"i-9223372036854775808e").unwrap(),
            Value::Int(i64::MIN)
        );
    }

    #[test]
    fn non_canonical_integers_are_rejected() {
        for bad in [
            &b"i-0e"[..],
            b"i03e",
            b"i-03e",
            b"ie",
            b"i-e",
            b"i+1e",
            b"i1.0e",
            b"i 1e",
        ] {
            assert_eq!(
                err(bad),
                Error::MalformedInt(0),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        assert_eq!(err(b"i9223372036854775808e"), Error::IntOutOfRange(0));
        assert_eq!(err(b"i99999999999999999999999e"), Error::IntOutOfRange(0));
        assert_eq!(err(b"i12"), Error::UnexpectedEof(3));
    }

    #[test]
    fn byte_strings() {
        assert_eq!(decode(b"0:").unwrap(), Value::Bytes(b""));
        assert_eq!(decode(b"4:spam").unwrap(), Value::Bytes(b"spam"));
        assert_eq!(err(b"04:spam"), Error::MalformedLength(0));
        assert_eq!(err(b"5:spam"), Error::UnexpectedEof(6));
        assert_eq!(
            err(b"99999999999999999999999999:x"),
            Error::MalformedLength(0)
        );
        assert_eq!(err(b"18446744073709551615:x"), Error::UnexpectedEof(22));
    }

    #[test]
    fn lists_and_dicts_record_raw_spans() {
        let input = b"d3:bar4:spam3:fooli42ei-1eee";
        let v = decode(input).unwrap();
        let d = v.as_dict().unwrap();
        assert_eq!(d.raw(), input);
        assert_eq!(d.get_str(b"bar"), Some("spam"));
        let foo = d.get_list(b"foo").unwrap();
        assert_eq!(foo.raw(), b"li42ei-1ee");
        assert_eq!(foo.len(), 2);
        assert_eq!(v.encode(), input);
    }

    #[test]
    fn dictionary_key_rules() {
        assert_eq!(err(b"d3:foo1:a3:bar1:be"), Error::UnsortedKeys(9));
        assert_eq!(err(b"d3:foo1:a3:foo1:be"), Error::DuplicateKey(9));
        assert_eq!(err(b"di1e1:ae"), Error::NonStringKey(1));

        let lenient = DecodeOptions::lenient();
        let v = decode_with(b"d3:foo1:a3:bar1:be", &lenient).unwrap();
        let d = v.as_dict().unwrap();
        assert_eq!(
            d.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            [b"bar", b"foo"]
        );
        assert_eq!(d.raw(), b"d3:foo1:a3:bar1:be");
        // Duplicates are rejected even when they are not adjacent in the input.
        assert_eq!(
            decode_with(b"d3:foo1:a3:bar1:b3:foo1:ce", &lenient).unwrap_err(),
            Error::DuplicateKey(0)
        );
    }

    #[test]
    fn depth_limit() {
        let opts = DecodeOptions {
            max_depth: 3,
            ..DecodeOptions::default()
        };
        assert!(decode_with(b"llleee", &opts).is_ok());
        assert_eq!(
            decode_with(b"lllleeee", &opts).unwrap_err(),
            Error::TooDeep { offset: 3, max: 3 }
        );
        // The default limit protects the stack from absurd nesting.
        let deep = [vec![b'l'; 1_000_000], vec![b'e'; 1_000_000]].concat();
        assert!(matches!(decode(&deep), Err(Error::TooDeep { .. })));
    }

    #[test]
    fn trailing_data_and_prefix() {
        assert_eq!(err(b"i1ei2e"), Error::TrailingData(3));
        let (v, used) =
            decode_prefix(b"d1:ai1ee<raw piece data>", &DecodeOptions::default()).unwrap();
        assert_eq!(used, 8);
        assert_eq!(v.as_dict().unwrap().get_int(b"a"), Some(1));
    }

    #[test]
    fn garbage() {
        assert_eq!(err(b""), Error::UnexpectedEof(0));
        assert_eq!(
            err(b"x"),
            Error::UnexpectedByte {
                offset: 0,
                byte: b'x'
            }
        );
        assert_eq!(err(b"l"), Error::UnexpectedEof(1));
        assert_eq!(err(b"d3:foo"), Error::UnexpectedEof(6));
    }
}

use std::collections::BTreeMap;
use std::fmt;

use crate::value::fmt_bytes;

/// An owned bencode value, for building documents.
///
/// Dictionaries are `BTreeMap`s, so encoding always produces sorted keys: the output is
/// canonical, which matters because info-hashes are computed over it.
#[derive(Clone, PartialEq, Eq)]
pub enum OwnedValue {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<OwnedValue>),
    Dict(BTreeMap<Vec<u8>, OwnedValue>),
    /// Bytes that are already bencoded, written out verbatim. Used to embed an info
    /// dictionary byte-for-byte, so its hash is preserved. The caller must make sure the
    /// bytes are one valid bencode value.
    Raw(Vec<u8>),
}

impl OwnedValue {
    /// An empty dictionary, to fill in with [`OwnedValue::insert`].
    pub fn dict() -> Self {
        OwnedValue::Dict(BTreeMap::new())
    }

    /// Insert into a dictionary and return it, for chaining.
    ///
    /// # Panics
    ///
    /// Panics if `self` is not a dictionary. That is a programming error, not a data error.
    #[must_use]
    pub fn insert(mut self, key: impl Into<Vec<u8>>, value: impl Into<OwnedValue>) -> Self {
        match &mut self {
            OwnedValue::Dict(map) => {
                map.insert(key.into(), value.into());
            }
            other => panic!("OwnedValue::insert on a {}", other.kind()),
        }
        self
    }

    pub fn kind(&self) -> &'static str {
        match self {
            OwnedValue::Int(_) => "integer",
            OwnedValue::Bytes(_) => "byte string",
            OwnedValue::List(_) => "list",
            OwnedValue::Dict(_) => "dictionary",
            OwnedValue::Raw(_) => "raw value",
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.write_to(&mut out);
        out
    }

    pub fn write_to(&self, out: &mut Vec<u8>) {
        match self {
            OwnedValue::Int(i) => {
                out.push(b'i');
                out.extend_from_slice(i.to_string().as_bytes());
                out.push(b'e');
            }
            OwnedValue::Bytes(b) => write_bytes(b, out),
            OwnedValue::List(items) => {
                out.push(b'l');
                for item in items {
                    item.write_to(out);
                }
                out.push(b'e');
            }
            OwnedValue::Dict(map) => {
                out.push(b'd');
                for (k, v) in map {
                    write_bytes(k, out);
                    v.write_to(out);
                }
                out.push(b'e');
            }
            OwnedValue::Raw(raw) => out.extend_from_slice(raw),
        }
    }
}

fn write_bytes(b: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(b.len().to_string().as_bytes());
    out.push(b':');
    out.extend_from_slice(b);
}

impl From<i64> for OwnedValue {
    fn from(i: i64) -> Self {
        OwnedValue::Int(i)
    }
}

impl From<u32> for OwnedValue {
    fn from(i: u32) -> Self {
        OwnedValue::Int(i64::from(i))
    }
}

impl From<&str> for OwnedValue {
    fn from(s: &str) -> Self {
        OwnedValue::Bytes(s.as_bytes().to_vec())
    }
}

impl From<String> for OwnedValue {
    fn from(s: String) -> Self {
        OwnedValue::Bytes(s.into_bytes())
    }
}

impl From<&[u8]> for OwnedValue {
    fn from(b: &[u8]) -> Self {
        OwnedValue::Bytes(b.to_vec())
    }
}

impl From<Vec<u8>> for OwnedValue {
    fn from(b: Vec<u8>) -> Self {
        OwnedValue::Bytes(b)
    }
}

impl From<Vec<OwnedValue>> for OwnedValue {
    fn from(items: Vec<OwnedValue>) -> Self {
        OwnedValue::List(items)
    }
}

impl fmt::Debug for OwnedValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OwnedValue::Int(i) => write!(f, "{i}"),
            OwnedValue::Bytes(b) => fmt_bytes(b, f),
            OwnedValue::List(items) => f.debug_list().entries(items).finish(),
            OwnedValue::Dict(map) => {
                let mut m = f.debug_map();
                for (k, v) in map {
                    m.entry(&Key(k), v);
                }
                m.finish()
            }
            OwnedValue::Raw(raw) => {
                f.write_str("Raw(")?;
                fmt_bytes(raw, f)?;
                f.write_str(")")
            }
        }
    }
}

struct Key<'a>(&'a [u8]);

impl fmt::Debug for Key<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_bytes(self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_sorted() {
        let v = OwnedValue::dict()
            .insert("zeta", 1i64)
            .insert("alpha", "x")
            .insert("list", vec![OwnedValue::Int(-3), OwnedValue::from("y")]);
        assert_eq!(v.to_bytes(), b"d5:alpha1:x4:listli-3e1:ye4:zetai1ee");
    }

    #[test]
    fn raw_is_verbatim() {
        let v = OwnedValue::dict().insert("info", OwnedValue::Raw(b"d1:ai1ee".to_vec()));
        assert_eq!(v.to_bytes(), b"d4:infod1:ai1eee");
    }
}

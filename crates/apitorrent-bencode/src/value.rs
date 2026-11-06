use std::fmt;

use crate::encode::OwnedValue;

/// A decoded bencode value that borrows from the input buffer.
#[derive(Clone, PartialEq, Eq)]
pub enum Value<'a> {
    Int(i64),
    Bytes(&'a [u8]),
    List(List<'a>),
    Dict(Dict<'a>),
}

/// A decoded list, together with the raw bytes it was decoded from.
#[derive(Clone, PartialEq, Eq)]
pub struct List<'a> {
    pub(crate) items: Vec<Value<'a>>,
    pub(crate) raw: &'a [u8],
}

/// A decoded dictionary, together with the raw bytes it was decoded from.
///
/// Entries are always held sorted by key, so lookups are a binary search even when the input
/// was out of order (see [`crate::DecodeOptions::require_sorted_keys`]).
#[derive(Clone, PartialEq, Eq)]
pub struct Dict<'a> {
    pub(crate) entries: Vec<(&'a [u8], Value<'a>)>,
    pub(crate) raw: &'a [u8],
}

impl<'a> Value<'a> {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&'a [u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    /// The value as UTF-8 text, if it is a byte string holding valid UTF-8.
    pub fn as_str(&self) -> Option<&'a str> {
        self.as_bytes().and_then(|b| std::str::from_utf8(b).ok())
    }

    pub fn as_list(&self) -> Option<&List<'a>> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&Dict<'a>> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }

    /// A short name for the value's type, for error messages.
    pub fn kind(&self) -> &'static str {
        match self {
            Value::Int(_) => "integer",
            Value::Bytes(_) => "byte string",
            Value::List(_) => "list",
            Value::Dict(_) => "dictionary",
        }
    }

    /// Copy the value out of the input buffer.
    pub fn to_owned_value(&self) -> OwnedValue {
        match self {
            Value::Int(i) => OwnedValue::Int(*i),
            Value::Bytes(b) => OwnedValue::Bytes(b.to_vec()),
            Value::List(l) => OwnedValue::List(l.iter().map(Value::to_owned_value).collect()),
            Value::Dict(d) => OwnedValue::Dict(
                d.iter()
                    .map(|(k, v)| (k.to_vec(), v.to_owned_value()))
                    .collect(),
            ),
        }
    }

    /// Encode the value canonically (sorted keys). For a value that was decoded from canonical
    /// input this reproduces the input exactly.
    pub fn encode(&self) -> Vec<u8> {
        self.to_owned_value().to_bytes()
    }
}

impl<'a> List<'a> {
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&Value<'a>> {
        self.items.get(index)
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Value<'a>> {
        self.items.iter()
    }

    /// The exact bytes this list was decoded from, including the `l` and `e` delimiters.
    pub fn raw(&self) -> &'a [u8] {
        self.raw
    }
}

impl<'l, 'a> IntoIterator for &'l List<'a> {
    type Item = &'l Value<'a>;
    type IntoIter = std::slice::Iter<'l, Value<'a>>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}

impl<'a> Dict<'a> {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, key: &[u8]) -> Option<&Value<'a>> {
        self.entries
            .binary_search_by(|(k, _)| (*k).cmp(key))
            .ok()
            .map(|i| &self.entries[i].1)
    }

    pub fn contains_key(&self, key: &[u8]) -> bool {
        self.get(key).is_some()
    }

    pub fn get_int(&self, key: &[u8]) -> Option<i64> {
        self.get(key).and_then(Value::as_int)
    }

    pub fn get_bytes(&self, key: &[u8]) -> Option<&'a [u8]> {
        self.get(key).and_then(Value::as_bytes)
    }

    pub fn get_str(&self, key: &[u8]) -> Option<&'a str> {
        self.get(key).and_then(Value::as_str)
    }

    pub fn get_list(&self, key: &[u8]) -> Option<&List<'a>> {
        self.get(key).and_then(Value::as_list)
    }

    pub fn get_dict(&self, key: &[u8]) -> Option<&Dict<'a>> {
        self.get(key).and_then(Value::as_dict)
    }

    /// Entries in ascending key order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&'a [u8], &Value<'a>)> + '_ {
        self.entries.iter().map(|(k, v)| (*k, v))
    }

    /// The exact bytes this dictionary was decoded from, including the `d` and `e` delimiters.
    pub fn raw(&self) -> &'a [u8] {
        self.raw
    }
}

impl fmt::Debug for Value<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(i) => write!(f, "{i}"),
            Value::Bytes(b) => fmt_bytes(b, f),
            Value::List(l) => l.fmt(f),
            Value::Dict(d) => d.fmt(f),
        }
    }
}

impl fmt::Debug for List<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(&self.items).finish()
    }
}

impl fmt::Debug for Dict<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut m = f.debug_map();
        for (k, v) in &self.entries {
            m.entry(&DebugBytes(k), v);
        }
        m.finish()
    }
}

struct DebugBytes<'a>(&'a [u8]);

impl fmt::Debug for DebugBytes<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_bytes(self.0, f)
    }
}

/// Text that is valid UTF-8 prints as a string; anything else prints as length-prefixed hex,
/// truncated, so piece hashes don't flood the output.
pub(crate) fn fmt_bytes(b: &[u8], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    const MAX: usize = 32;
    if let Ok(s) = std::str::from_utf8(b) {
        return write!(f, "{s:?}");
    }
    write!(f, "<{} bytes: ", b.len())?;
    for byte in b.iter().take(MAX) {
        write!(f, "{byte:02x}")?;
    }
    if b.len() > MAX {
        f.write_str("…")?;
    }
    f.write_str(">")
}

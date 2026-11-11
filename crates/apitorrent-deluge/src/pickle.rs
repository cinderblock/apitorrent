//! A minimal, safe reader for Python pickles, enough for Deluge's `torrents.state`.
//!
//! Pickles are programs for a small stack machine, and Python's own unpickler runs them,
//! which can execute arbitrary code. This one only builds data: class references become
//! [`Value::Class`] markers, and "call this class" becomes a [`Value::Object`] holding the
//! arguments and the state that `BUILD` sets. Nothing is ever called or imported.
//!
//! Protocols 0–5 are read, but only the opcodes Python emits for plain data, classes and
//! their instances. Anything else is an error, as is any input that would need more than
//! [`MAX_MEMORY`] bytes of values and strings, or nesting deeper than [`MAX_DEPTH`]: the
//! input is not trusted. `DUP` and the memo copy values, so a short
//! pickle could otherwise double its size with every pair of opcodes.

use std::collections::HashMap;

/// The most memory a pickle may build, in bytes, to bound hostile input. Values are
/// counted at [`VALUE_COST`] each, plus the strings that `DUP` and the memo copy. A
/// Deluge state of tens of thousands of torrents fits well inside it.
pub const MAX_MEMORY: usize = 384 * 1024 * 1024;

/// What one value is counted as: its own size, doubled for the allocations it may own (a
/// boxed class, a vector's spare capacity).
const VALUE_COST: usize = 2 * std::mem::size_of::<Value>();

/// The deepest nesting of containers a pickle may build. Deluge's state is a few levels
/// deep; the limit keeps dropping or printing a value from exhausting the stack.
pub const MAX_DEPTH: u32 = 512;

/// A value from a pickle.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    None,
    Bool(bool),
    Int(i64),
    /// An integer too large for `i64`, as little-endian two's-complement bytes.
    BigInt(Vec<u8>),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Tuple(Vec<Value>),
    Dict(Vec<(Value, Value)>),
    /// A reference to a class or function, by module and name. Never resolved.
    Class {
        module: String,
        name: String,
    },
    /// An instance: the class it was made from, its constructor arguments, and the state
    /// set by `BUILD` (usually its `__dict__`).
    Object {
        class: Box<Value>,
        args: Vec<Value>,
        state: Option<Box<Value>>,
    },
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Bool(b) => Some(i64::from(*b)),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            #[allow(clippy::cast_precision_loss)]
            Value::Int(i) => Some(*i as f64),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::Int(i) => Some(*i != 0),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(v) | Value::Tuple(v) => Some(v),
            _ => None,
        }
    }

    /// Look up a string key in a dict, or in an object's state dict.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Dict(items) => items
                .iter()
                .find(|(k, _)| k.as_str() == Some(key))
                .map(|(_, v)| v),
            Value::Object {
                state: Some(state), ..
            } => state.get(key),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PickleError {
    #[error("pickle ended early")]
    Truncated,
    #[error("unsupported pickle opcode 0x{0:02x}")]
    Opcode(u8),
    #[error("malformed pickle: {0}")]
    Malformed(&'static str),
    #[error("pickle is too large")]
    TooLarge,
    #[error("pickle nests too deeply")]
    TooDeep,
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], PickleError> {
        let end = self.pos.checked_add(n).ok_or(PickleError::Truncated)?;
        let s = self.data.get(self.pos..end).ok_or(PickleError::Truncated)?;
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, PickleError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, PickleError> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("2 bytes"),
        ))
    }

    fn u32(&mut self) -> Result<u32, PickleError> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    fn u64(&mut self) -> Result<u64, PickleError> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }

    fn line(&mut self) -> Result<&'a str, PickleError> {
        let rest = &self.data[self.pos..];
        let n = rest
            .iter()
            .position(|&b| b == b'\n')
            .ok_or(PickleError::Truncated)?;
        self.pos += n + 1;
        std::str::from_utf8(&rest[..n]).map_err(|_| PickleError::Malformed("non-UTF-8 line"))
    }

    fn sized(&mut self, n: u64) -> Result<&'a [u8], PickleError> {
        let n = usize::try_from(n).map_err(|_| PickleError::TooLarge)?;
        self.take(n)
    }
}

enum Slot {
    Mark,
    /// A value and how deeply it nests (0 for a scalar or an empty container).
    Value(Value, u32),
}

impl Slot {
    fn leaf(v: Value) -> Self {
        Self::Value(v, 0)
    }
}

/// The memory built so far, against the limit.
struct Budget {
    used: usize,
    max: usize,
}

impl Budget {
    fn charge(&mut self, items: usize, bytes: usize) -> Result<(), PickleError> {
        self.used = self
            .used
            .saturating_add(items.saturating_mul(VALUE_COST))
            .saturating_add(bytes);
        if self.used > self.max {
            return Err(PickleError::TooLarge);
        }
        Ok(())
    }

    /// Charge for a copy of `v`, then make it. Everything in `v` was charged when it was
    /// built, so the walk costs no more than the budget allows.
    fn copy(&mut self, v: &Value) -> Result<Value, PickleError> {
        let (mut items, mut bytes) = (0usize, 0usize);
        let mut todo = vec![v];
        while let Some(v) = todo.pop() {
            items += 1;
            match v {
                Value::Str(s) => bytes += s.len(),
                Value::Bytes(b) | Value::BigInt(b) => bytes += b.len(),
                Value::Class { module, name } => bytes += module.len() + name.len(),
                Value::List(l) | Value::Tuple(l) => todo.extend(l),
                Value::Dict(d) => todo.extend(d.iter().flat_map(|(k, v)| [k, v])),
                Value::Object { class, args, state } => {
                    todo.push(class);
                    todo.extend(args);
                    todo.extend(state.as_deref());
                }
                Value::None | Value::Bool(_) | Value::Int(_) | Value::Float(_) => {}
            }
        }
        self.charge(items, bytes)?;
        Ok(v.clone())
    }
}

/// The depth of a container holding something `depth` deep.
fn nest(depth: u32) -> Result<u32, PickleError> {
    let d = depth + 1;
    if d > MAX_DEPTH {
        return Err(PickleError::TooDeep);
    }
    Ok(d)
}

/// Read a pickle.
pub fn load(data: &[u8]) -> Result<Value, PickleError> {
    load_limited(data, MAX_MEMORY)
}

/// [`load`] with a smaller memory limit, so tests of the limit run quickly.
fn load_limited(data: &[u8], max: usize) -> Result<Value, PickleError> {
    let mut r = Reader { data, pos: 0 };
    let mut stack: Vec<Slot> = Vec::new();
    let mut memo: HashMap<u64, (Value, u32)> = HashMap::new();
    let mut budget = Budget { used: 0, max };

    loop {
        budget.charge(1, 0)?;
        let op = r.u8()?;
        match op {
            0x80 => {
                r.u8()?; // PROTO
            }
            0x95 => {
                r.u64()?; // FRAME: a size hint only
            }
            b'.' => return pop(&mut stack).map(|(v, _)| v), // STOP
            b'(' => stack.push(Slot::Mark),
            b'N' => stack.push(Slot::leaf(Value::None)),
            0x88 => stack.push(Slot::leaf(Value::Bool(true))),
            0x89 => stack.push(Slot::leaf(Value::Bool(false))),
            b'J' => stack.push(Slot::leaf(Value::Int(i64::from(r.u32()?.cast_signed())))),
            b'K' => stack.push(Slot::leaf(Value::Int(i64::from(r.u8()?)))),
            b'M' => stack.push(Slot::leaf(Value::Int(i64::from(r.u16()?)))),
            0x8a | 0x8b => {
                // LONG1, LONG4: little-endian two's complement
                let n = if op == 0x8a {
                    u64::from(r.u8()?)
                } else {
                    u64::from(r.u32()?)
                };
                let bytes = r.sized(n)?;
                let v = if bytes.len() <= 8 {
                    let mut buf = [if bytes.last().is_some_and(|b| b & 0x80 != 0) {
                        0xff
                    } else {
                        0
                    }; 8];
                    buf[..bytes.len()].copy_from_slice(bytes);
                    Value::Int(i64::from_le_bytes(buf))
                } else {
                    Value::BigInt(bytes.to_vec())
                };
                stack.push(Slot::leaf(v));
            }
            b'I' => {
                // INT (protocol 0): "01"/"00" are booleans
                let line = r.line()?;
                let v = match line {
                    "01" => Value::Bool(true),
                    "00" => Value::Bool(false),
                    s => Value::Int(
                        s.trim()
                            .parse()
                            .map_err(|_| PickleError::Malformed("INT"))?,
                    ),
                };
                stack.push(Slot::leaf(v));
            }
            b'L' => {
                let line = r.line()?.trim_end_matches('L');
                let v = line
                    .parse::<i64>()
                    .map_or_else(|_| Value::BigInt(line.as_bytes().to_vec()), Value::Int);
                stack.push(Slot::leaf(v));
            }
            b'G' => stack.push(Slot::leaf(Value::Float(f64::from_be_bytes(
                r.take(8)?.try_into().expect("8 bytes"),
            )))),
            b'F' => {
                let line = r.line()?;
                stack.push(Slot::leaf(Value::Float(
                    line.trim()
                        .parse()
                        .map_err(|_| PickleError::Malformed("FLOAT"))?,
                )));
            }
            0x8c | b'X' | 0x8d => {
                // SHORT_BINUNICODE, BINUNICODE, BINUNICODE8
                let n = match op {
                    0x8c => u64::from(r.u8()?),
                    b'X' => u64::from(r.u32()?),
                    _ => r.u64()?,
                };
                let s = String::from_utf8_lossy(r.sized(n)?).into_owned();
                stack.push(Slot::leaf(Value::Str(s)));
            }
            b'V' => {
                let s = r.line()?.to_owned();
                stack.push(Slot::leaf(Value::Str(s)));
            }
            b'U' | b'T' => {
                // SHORT_BINSTRING, BINSTRING (Python 2 str): bytes, read as Latin-1 text
                let n = if op == b'U' {
                    u64::from(r.u8()?)
                } else {
                    u64::from(r.u32()?)
                };
                let s: String = r.sized(n)?.iter().map(|&b| char::from(b)).collect();
                stack.push(Slot::leaf(Value::Str(s)));
            }
            b'S' => {
                let line = r.line()?;
                let s = line.trim().trim_matches(['\'', '"']).to_owned();
                stack.push(Slot::leaf(Value::Str(s)));
            }
            b'C' | b'B' | 0x8e | 0x96 => {
                // SHORT_BINBYTES, BINBYTES, BINBYTES8, BYTEARRAY8
                let n = match op {
                    b'C' => u64::from(r.u8()?),
                    b'B' => u64::from(r.u32()?),
                    _ => r.u64()?,
                };
                stack.push(Slot::leaf(Value::Bytes(r.sized(n)?.to_vec())));
            }
            b'}' => stack.push(Slot::leaf(Value::Dict(Vec::new()))),
            b']' => stack.push(Slot::leaf(Value::List(Vec::new()))),
            b')' => stack.push(Slot::leaf(Value::Tuple(Vec::new()))),
            b'd' => {
                let (items, d) = pop_mark(&mut stack)?;
                stack.push(Slot::Value(Value::Dict(pairs(items)?), nest(d)?));
            }
            b'l' => {
                let (items, d) = pop_mark(&mut stack)?;
                stack.push(Slot::Value(Value::List(items), nest(d)?));
            }
            b't' => {
                let (items, d) = pop_mark(&mut stack)?;
                stack.push(Slot::Value(Value::Tuple(items), nest(d)?));
            }
            0x85..=0x87 => {
                let n = usize::from(op - 0x84);
                if stack.len() < n {
                    return Err(PickleError::Malformed("stack underflow"));
                }
                let mut items = Vec::with_capacity(n);
                let mut d = 0;
                for _ in 0..n {
                    let (v, vd) = pop(&mut stack)?;
                    items.push(v);
                    d = d.max(vd);
                }
                items.reverse();
                stack.push(Slot::Value(Value::Tuple(items), nest(d)?));
            }
            b's' => {
                let (v, vd) = pop(&mut stack)?;
                let (k, kd) = pop(&mut stack)?;
                match top(&mut stack)? {
                    (Value::Dict(d), depth) => {
                        *depth = (*depth).max(nest(vd.max(kd))?);
                        d.push((k, v));
                    }
                    _ => return Err(PickleError::Malformed("SETITEM on a non-dict")),
                }
            }
            b'u' => {
                let (items, nd) = pop_mark(&mut stack)?;
                let new = pairs(items)?;
                match top(&mut stack)? {
                    (Value::Dict(d), depth) => {
                        *depth = (*depth).max(nest(nd)?);
                        d.extend(new);
                    }
                    _ => return Err(PickleError::Malformed("SETITEMS on a non-dict")),
                }
            }
            b'a' => {
                let (v, vd) = pop(&mut stack)?;
                match top(&mut stack)? {
                    (Value::List(l), depth) => {
                        *depth = (*depth).max(nest(vd)?);
                        l.push(v);
                    }
                    _ => return Err(PickleError::Malformed("APPEND on a non-list")),
                }
            }
            b'e' => {
                let (items, nd) = pop_mark(&mut stack)?;
                match top(&mut stack)? {
                    (Value::List(l), depth) => {
                        *depth = (*depth).max(nest(nd)?);
                        l.extend(items);
                    }
                    _ => return Err(PickleError::Malformed("APPENDS on a non-list")),
                }
            }
            0x8f => stack.push(Slot::leaf(Value::List(Vec::new()))), // EMPTY_SET
            0x90 => {
                // ADDITEMS
                let (items, nd) = pop_mark(&mut stack)?;
                match top(&mut stack)? {
                    (Value::List(l), depth) => {
                        *depth = (*depth).max(nest(nd)?);
                        l.extend(items);
                    }
                    _ => return Err(PickleError::Malformed("ADDITEMS on a non-set")),
                }
            }
            0x91 => {
                let (items, d) = pop_mark(&mut stack)?; // FROZENSET
                stack.push(Slot::Value(Value::List(items), nest(d)?));
            }
            b'q' | b'r' | 0x94 | b'p' => {
                let id = match op {
                    b'q' => u64::from(r.u8()?),
                    b'r' => u64::from(r.u32()?),
                    b'p' => r
                        .line()?
                        .trim()
                        .parse()
                        .map_err(|_| PickleError::Malformed("PUT"))?,
                    _ => memo.len() as u64, // MEMOIZE
                };
                let (v, d) = top(&mut stack)?;
                let d = *d;
                let v = budget.copy(v)?;
                memo.insert(id, (v, d));
            }
            b'h' | b'j' | b'g' => {
                let id = match op {
                    b'h' => u64::from(r.u8()?),
                    b'j' => u64::from(r.u32()?),
                    _ => r
                        .line()?
                        .trim()
                        .parse()
                        .map_err(|_| PickleError::Malformed("GET"))?,
                };
                let (v, d) = memo.get(&id).ok_or(PickleError::Malformed("memo miss"))?;
                stack.push(Slot::Value(budget.copy(v)?, *d));
            }
            b'c' => {
                let module = r.line()?.to_owned();
                let name = r.line()?.to_owned();
                stack.push(Slot::leaf(Value::Class { module, name }));
            }
            0x93 => {
                // STACK_GLOBAL
                let (name, _) = pop(&mut stack)?;
                let (module, _) = pop(&mut stack)?;
                let (Value::Str(module), Value::Str(name)) = (module, name) else {
                    return Err(PickleError::Malformed("STACK_GLOBAL needs strings"));
                };
                stack.push(Slot::leaf(Value::Class { module, name }));
            }
            0x81 | b'R' => {
                // NEWOBJ (cls, args) and REDUCE (callable, args): recorded, never called
                let (args, ad) = pop(&mut stack)?;
                let (class, cd) = pop(&mut stack)?;
                let args = match args {
                    Value::Tuple(a) => a,
                    other => vec![other],
                };
                stack.push(Slot::Value(
                    Value::Object {
                        class: Box::new(class),
                        args,
                        state: None,
                    },
                    nest(ad.max(cd))?,
                ));
            }
            0x92 => {
                // NEWOBJ_EX (cls, args, kwargs)
                let _kwargs = pop(&mut stack)?;
                let (args, ad) = pop(&mut stack)?;
                let (class, cd) = pop(&mut stack)?;
                stack.push(Slot::Value(
                    Value::Object {
                        class: Box::new(class),
                        args: match args {
                            Value::Tuple(a) => a,
                            other => vec![other],
                        },
                        state: None,
                    },
                    nest(ad.max(cd))?,
                ));
            }
            b'b' => {
                // BUILD: set the state of the object below
                let (state, sd) = pop(&mut stack)?;
                match top(&mut stack)? {
                    (Value::Object { state: s, .. }, depth) => {
                        *depth = (*depth).max(nest(sd)?);
                        *s = Some(Box::new(state));
                    }
                    // Building onto something that isn't an object: keep the state.
                    (other, depth) => {
                        *other = state;
                        *depth = sd;
                    }
                }
            }
            b'0' => {
                pop(&mut stack)?; // POP
            }
            b'1' => {
                pop_mark(&mut stack)?; // POP_MARK
            }
            b'2' => {
                let (v, d) = top(&mut stack)?; // DUP
                let d = *d;
                let v = budget.copy(v)?;
                stack.push(Slot::Value(v, d));
            }
            other => return Err(PickleError::Opcode(other)),
        }
    }
}

fn pop(stack: &mut Vec<Slot>) -> Result<(Value, u32), PickleError> {
    match stack.pop() {
        Some(Slot::Value(v, d)) => Ok((v, d)),
        _ => Err(PickleError::Malformed("stack underflow")),
    }
}

/// Pop everything above the topmost mark, and the mark. Also returns the deepest item's
/// depth.
fn pop_mark(stack: &mut Vec<Slot>) -> Result<(Vec<Value>, u32), PickleError> {
    let at = stack
        .iter()
        .rposition(|s| matches!(s, Slot::Mark))
        .ok_or(PickleError::Malformed("no mark"))?;
    let mut depth = 0;
    let items = stack
        .drain(at + 1..)
        .map(|s| match s {
            Slot::Value(v, d) => {
                depth = depth.max(d);
                v
            }
            Slot::Mark => unreachable!("marks were searched from the top"),
        })
        .collect();
    stack.pop();
    Ok((items, depth))
}

fn top(stack: &mut [Slot]) -> Result<(&mut Value, &mut u32), PickleError> {
    match stack.last_mut() {
        Some(Slot::Value(v, d)) => Ok((v, d)),
        _ => Err(PickleError::Malformed("no value on the stack")),
    }
}

fn pairs(items: Vec<Value>) -> Result<Vec<(Value, Value)>, PickleError> {
    if !items.len().is_multiple_of(2) {
        return Err(PickleError::Malformed("odd number of dict items"));
    }
    let mut out = Vec::with_capacity(items.len() / 2);
    let mut it = items.into_iter();
    while let (Some(k), Some(v)) = (it.next(), it.next()) {
        out.push((k, v));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `pickle.dumps({"a": [1, 2.5, None, True], "b": "x"}, protocol=2)`
    #[test]
    fn protocol_2_data() {
        let p: &[u8] = b"\x80\x02}q\x00(X\x01\x00\x00\x00aq\x01]q\x02(K\x01G@\x04\x00\x00\x00\x00\x00\x00N\x88eX\x01\x00\x00\x00bq\x03X\x01\x00\x00\x00xq\x04u.";
        let v = load(p).unwrap();
        assert_eq!(
            v.get("a").unwrap(),
            &Value::List(vec![
                Value::Int(1),
                Value::Float(2.5),
                Value::None,
                Value::Bool(true)
            ])
        );
        assert_eq!(v.get("b").and_then(Value::as_str), Some("x"));
    }

    /// An instance of a class with a `__dict__`, as `pickle.dumps(obj, protocol=2)` writes
    /// it: `GLOBAL`, `EMPTY_TUPLE`, `NEWOBJ`, then `BUILD` with the dict.
    #[test]
    fn objects_keep_their_state_and_nothing_runs() {
        let p: &[u8] = b"\x80\x02cdeluge.core.torrentmanager\nTorrentState\nq\x00)\x81q\x01}q\x02(X\x04\x00\x00\x00nameq\x03X\x03\x00\x00\x00isoq\x04X\x05\x00\x00\x00queueq\x05K\x07ub.";
        let v = load(p).unwrap();
        let Value::Object { class, .. } = &v else {
            panic!("{v:?}")
        };
        assert_eq!(
            **class,
            Value::Class {
                module: "deluge.core.torrentmanager".into(),
                name: "TorrentState".into()
            }
        );
        assert_eq!(v.get("name").and_then(Value::as_str), Some("iso"));
        assert_eq!(v.get("queue").and_then(Value::as_int), Some(7));
    }

    #[test]
    fn longs_and_memo() {
        // `pickle.dumps((-2, 2**40, s, s), protocol=2)` with `s = "hi"`: BININT, LONG1, and
        // a memoised string fetched again.
        let p: &[u8] = b"\x80\x02(J\xfe\xff\xff\xff\x8a\x06\x00\x00\x00\x00\x00\x01X\x02\x00\x00\x00hiq\x00h\x00tq\x01.";
        let v = load(p).unwrap();
        assert_eq!(
            v,
            Value::Tuple(vec![
                Value::Int(-2),
                Value::Int(1 << 40),
                Value::Str("hi".into()),
                Value::Str("hi".into())
            ])
        );
    }

    #[test]
    fn negative_long1() {
        assert_eq!(load(b"\x80\x02\x8a\x01\xfe.").unwrap(), Value::Int(-2));
        assert_eq!(
            load(b"\x80\x02\x8a\x02\x00\x80.").unwrap(),
            Value::Int(-32768)
        );
    }

    #[test]
    fn hostile_input_is_an_error_not_a_panic() {
        for bad in [
            &b""[..],
            b"\x80\x02",
            b"\x80\x02s.",
            b"\x80\x02\xff",
            b"\x80\x02X\xff\xff\xff\xff",
            b"\x80\x02h\x05.",
        ] {
            assert!(load(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn copies_count_against_the_limit() {
        // DUP and APPEND double a list with every pair of opcodes.
        let mut p = b"\x80\x02]".to_vec();
        for _ in 0..64 {
            p.extend_from_slice(b"2a");
        }
        p.push(b'.');
        assert!(matches!(
            load_limited(&p, 8 << 20),
            Err(PickleError::TooLarge | PickleError::TooDeep)
        ));
        // The same through the memo: PUT, then GET it twice and append.
        let mut p = b"\x80\x02]".to_vec();
        for _ in 0..64 {
            p.extend_from_slice(b"q\x00h\x00a");
        }
        p.push(b'.');
        assert!(load_limited(&p, 8 << 20).is_err());
        // The input the fuzzer found (out of memory at 2 GB before the fix).
        let found = b"((l2222ra\xff\xff\xffRa222ra\xff\xff\xffRaaa2222aa2222ra2(l2222ra22222aa2222ra2(l2222r\x002222aaaa2222aaa22aa2222aa2222ra2(l2222ra22222aa2222ra2(l2222r\x002222aaaa2222aaa2222aa222";
        assert!(load_limited(found, 8 << 20).is_err());
        // A second one: NEWOBJ_EX objects copied by DUP, over 2 GB under AddressSanitizer
        // when only the number of values was limited.
        let found = b")]\x89)\x9222\x8f\x89\x9222\x92222\x9222\x8f\x89])a\x922\x92222\x8f\x89\x9222\x92\x92\x92222\x8f\x89\x9222\x92\x922\x9222\x8f\x89\x922\x9222\x8f\x89\x9222\x92222\x9222\x8f\x89\x9222\x8f\x89\x9222\x92222\x9222\x8f\x89\x9222\x92222\x9222\x8f\x89])a\x922\x92222\x8f\x89\x9222\x92\x92\x92222\x8f\x89\x9222\x92\x922\x9222\x8f\x89\x922\x9222\x8f\x89\x9222\x92222\x9222\x8f\x89\x9222\x8f\x89\x9222\x9222.\x922\xc8";
        assert!(load_limited(found, 8 << 20).is_err());
    }

    #[test]
    fn deep_nesting_is_refused() {
        let mut p = b"\x80\x02".to_vec();
        p.extend(std::iter::repeat_n(b']', 10_000));
        p.extend(std::iter::repeat_n(b'a', 9_999));
        p.push(b'.');
        assert_eq!(load(&p), Err(PickleError::TooDeep));
        // Shallow nesting still loads.
        assert!(load(b"\x80\x02]]]aa.").is_ok());
    }
}

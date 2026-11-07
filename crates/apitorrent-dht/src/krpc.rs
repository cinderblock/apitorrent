//! KRPC, the DHT's message format ([BEP 5]): bencoded dictionaries over UDP, as plain data
//! and codecs with no I/O.
//!
//! Supported on top of BEP 5: the `want` key and `nodes6` of [BEP 32], the `ip` key of
//! [BEP 42], the `ro` flag of [BEP 43], and the `scrape`, `noseed`, `seed`, `BFsd` and
//! `BFpe` keys of [BEP 33].
//!
//! The decoder treats every datagram as hostile. It never panics, caps every list it
//! builds, and rejects a message that is missing a key its kind requires. A query whose
//! transaction id could be read but whose arguments could not is reported with that id
//! ([`DecodeError::transaction_id`]), so the node can answer with error 203.
//!
//! [BEP 5]: https://www.bittorrent.org/beps/bep_0005.html
//! [BEP 32]: https://www.bittorrent.org/beps/bep_0032.html
//! [BEP 33]: https://www.bittorrent.org/beps/bep_0033.html
//! [BEP 42]: https://www.bittorrent.org/beps/bep_0042.html
//! [BEP 43]: https://www.bittorrent.org/beps/bep_0043.html

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use apitorrent_bencode::{DecodeOptions, Dict, OwnedValue, Value};

use crate::id::NodeId;

/// The longest transaction id we accept. Real clients use two to four bytes.
pub const MAX_TRANSACTION_ID_LEN: usize = 16;
/// The longest write token we accept. Real clients use four to twenty bytes.
pub const MAX_TOKEN_LEN: usize = 64;
/// The longest client version string we keep. Longer ones are dropped, not rejected.
pub const MAX_VERSION_LEN: usize = 16;
/// The most peers we take from one `values` list.
pub const MAX_VALUES: usize = 256;
/// The most nodes we take from one `nodes` or `nodes6` string.
pub const MAX_NODES: usize = 64;
/// The size of a BEP 33 bloom filter in bytes.
pub const BLOOM_LEN: usize = 256;
/// The longest error message we keep.
pub const MAX_ERROR_MESSAGE_LEN: usize = 256;

/// The size of a compact IPv4 node: 20-byte ID, 4-byte address, 2-byte port.
pub const COMPACT_NODE_V4_LEN: usize = 26;
/// The size of a compact IPv6 node: 20-byte ID, 16-byte address, 2-byte port.
pub const COMPACT_NODE_V6_LEN: usize = 38;

/// KRPC error codes (BEP 5).
pub mod error_code {
    pub const GENERIC: i64 = 201;
    pub const SERVER: i64 = 202;
    pub const PROTOCOL: i64 = 203;
    pub const METHOD_UNKNOWN: i64 = 204;
}

/// Decoder limits: KRPC nests at most three levels (message, arguments, a list), so a
/// small depth cap costs nothing. Keys out of order are accepted because some clients send
/// them; duplicates are still rejected.
const DECODE_OPTIONS: DecodeOptions = DecodeOptions {
    max_depth: 8,
    require_sorted_keys: false,
};

/// A node's ID and UDP address, as carried in `nodes` and `nodes6`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeInfo {
    pub id: NodeId,
    pub addr: SocketAddr,
}

/// Which address families of nodes a querier wants back (BEP 32). With neither set, the
/// responder returns nodes of the family the query arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Want {
    pub v4: bool,
    pub v6: bool,
}

impl Want {
    pub const NONE: Want = Want {
        v4: false,
        v6: false,
    };

    /// Whether the querier named any family.
    pub fn is_specified(self) -> bool {
        self.v4 || self.v6
    }
}

/// One decoded KRPC message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// The `t` key: the querier's transaction id, echoed in the reply.
    pub transaction_id: Vec<u8>,
    /// The `v` key: a client version string, if present and short.
    pub version: Option<Vec<u8>>,
    /// The `ro` key of BEP 43: the sender is a read-only node and answers no queries.
    pub read_only: bool,
    /// The `ip` key of BEP 42: the address the responder saw the querier at.
    pub ip: Option<SocketAddr>,
    pub body: Body,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    Query(Query),
    Response(Response),
    Error { code: i64, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    Ping {
        id: NodeId,
    },
    FindNode {
        id: NodeId,
        target: NodeId,
        want: Want,
    },
    GetPeers {
        id: NodeId,
        info_hash: NodeId,
        want: Want,
        /// BEP 33: the querier wants no seeds in `values`.
        noseed: bool,
        /// BEP 33: the querier wants the `BFsd` and `BFpe` bloom filters.
        scrape: bool,
    },
    AnnouncePeer {
        id: NodeId,
        info_hash: NodeId,
        port: u16,
        /// Use the UDP source port instead of `port` (for peers behind NAT using uTP).
        implied_port: bool,
        token: Vec<u8>,
        /// BEP 33: the announcing peer is a seed.
        seed: bool,
    },
    /// A method we don't know. BEP 5 asks us to answer it with error 204.
    Unknown {
        id: Option<NodeId>,
        method: Vec<u8>,
    },
}

impl Query {
    /// The querying node's ID, if it sent one.
    pub fn sender_id(&self) -> Option<NodeId> {
        match self {
            Query::Ping { id }
            | Query::FindNode { id, .. }
            | Query::GetPeers { id, .. }
            | Query::AnnouncePeer { id, .. } => Some(*id),
            Query::Unknown { id, .. } => *id,
        }
    }

    /// The KRPC method name.
    pub fn method(&self) -> &[u8] {
        match self {
            Query::Ping { .. } => b"ping",
            Query::FindNode { .. } => b"find_node",
            Query::GetPeers { .. } => b"get_peers",
            Query::AnnouncePeer { .. } => b"announce_peer",
            Query::Unknown { method, .. } => method,
        }
    }
}

/// A response. KRPC responses don't name their method, so every response decodes into this
/// one shape and the caller reads the keys its query expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub id: NodeId,
    pub nodes: Vec<NodeInfo>,
    pub nodes6: Vec<NodeInfo>,
    /// Peers from a `get_peers` response, both families mixed.
    pub values: Vec<SocketAddr>,
    pub token: Option<Vec<u8>>,
    /// BEP 33 bloom filter of seeds.
    pub bf_seeds: Option<Box<[u8; BLOOM_LEN]>>,
    /// BEP 33 bloom filter of downloaders.
    pub bf_peers: Option<Box<[u8; BLOOM_LEN]>>,
}

impl Response {
    /// A response that carries only the responder's ID, as for `ping` and `announce_peer`.
    pub fn with_id(id: NodeId) -> Self {
        Self {
            id,
            nodes: Vec::new(),
            nodes6: Vec::new(),
            values: Vec::new(),
            token: None,
            bf_seeds: None,
            bf_peers: None,
        }
    }
}

/// Why a datagram could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("malformed KRPC message: {reason}")]
pub struct DecodeError {
    /// The transaction id, when it was readable and the message was a query, so the node
    /// can reply with a protocol error.
    pub transaction_id: Option<Vec<u8>>,
    pub reason: &'static str,
}

impl DecodeError {
    fn new(reason: &'static str) -> Self {
        Self {
            transaction_id: None,
            reason,
        }
    }
}

/// Decode one datagram.
pub fn decode(buf: &[u8]) -> Result<Message, DecodeError> {
    let value = apitorrent_bencode::decode_with(buf, &DECODE_OPTIONS)
        .map_err(|_| DecodeError::new("not valid bencode"))?;
    let dict = value
        .as_dict()
        .ok_or_else(|| DecodeError::new("not a dictionary"))?;
    let transaction_id = dict
        .get_bytes(b"t")
        .ok_or_else(|| DecodeError::new("missing transaction id"))?;
    if transaction_id.len() > MAX_TRANSACTION_ID_LEN {
        return Err(DecodeError::new("transaction id too long"));
    }
    let kind = dict
        .get_bytes(b"y")
        .ok_or_else(|| DecodeError::new("missing message type"))?;
    let version = dict
        .get_bytes(b"v")
        .filter(|v| v.len() <= MAX_VERSION_LEN)
        .map(<[u8]>::to_vec);
    let read_only = dict.get_int(b"ro") == Some(1);
    let ip = dict.get_bytes(b"ip").and_then(parse_compact_addr);

    let body = match kind {
        b"q" => decode_query(dict).map_err(|reason| DecodeError {
            transaction_id: Some(transaction_id.to_vec()),
            reason,
        })?,
        b"r" => Body::Response(decode_response(dict).map_err(DecodeError::new)?),
        b"e" => decode_error(dict).map_err(DecodeError::new)?,
        _ => return Err(DecodeError::new("unknown message type")),
    };
    Ok(Message {
        transaction_id: transaction_id.to_vec(),
        version,
        read_only,
        ip,
        body,
    })
}

fn get_id(d: &Dict<'_>, key: &[u8]) -> Result<NodeId, &'static str> {
    d.get_bytes(key)
        .and_then(NodeId::from_slice)
        .ok_or("missing or malformed 20-byte id")
}

fn get_flag(d: &Dict<'_>, key: &[u8]) -> bool {
    d.get_int(key).is_some_and(|v| v != 0)
}

fn get_want(d: &Dict<'_>) -> Want {
    let mut want = Want::NONE;
    if let Some(list) = d.get_list(b"want") {
        // Only look at the first few entries: a long list is hostile, not useful.
        for item in list.iter().take(8) {
            match item.as_bytes() {
                Some(b"n4") => want.v4 = true,
                Some(b"n6") => want.v6 = true,
                _ => {}
            }
        }
    }
    want
}

fn decode_query(dict: &Dict<'_>) -> Result<Body, &'static str> {
    let method = dict.get_bytes(b"q").ok_or("missing method")?;
    let args = dict.get_dict(b"a").ok_or("missing arguments")?;
    let query = match method {
        b"ping" => Query::Ping {
            id: get_id(args, b"id")?,
        },
        b"find_node" => Query::FindNode {
            id: get_id(args, b"id")?,
            target: get_id(args, b"target")?,
            want: get_want(args),
        },
        b"get_peers" => Query::GetPeers {
            id: get_id(args, b"id")?,
            info_hash: get_id(args, b"info_hash")?,
            want: get_want(args),
            noseed: get_flag(args, b"noseed"),
            scrape: get_flag(args, b"scrape"),
        },
        b"announce_peer" => {
            let implied_port = get_flag(args, b"implied_port");
            // With implied_port the port may be absent or junk.
            let port = match args.get_int(b"port").and_then(|p| u16::try_from(p).ok()) {
                Some(p) if p != 0 => p,
                _ if implied_port => 0,
                _ => return Err("missing or invalid port"),
            };
            let token = args.get_bytes(b"token").ok_or("missing token")?;
            if token.len() > MAX_TOKEN_LEN {
                return Err("token too long");
            }
            Query::AnnouncePeer {
                id: get_id(args, b"id")?,
                info_hash: get_id(args, b"info_hash")?,
                port,
                implied_port,
                token: token.to_vec(),
                seed: get_flag(args, b"seed"),
            }
        }
        other => {
            if other.len() > 32 {
                return Err("method name too long");
            }
            Query::Unknown {
                id: args.get_bytes(b"id").and_then(NodeId::from_slice),
                method: other.to_vec(),
            }
        }
    };
    Ok(Body::Query(query))
}

fn decode_response(dict: &Dict<'_>) -> Result<Response, &'static str> {
    let r = dict.get_dict(b"r").ok_or("missing response dictionary")?;
    let id = get_id(r, b"id")?;
    let nodes = match r.get_bytes(b"nodes") {
        Some(b) => parse_compact_nodes(b, COMPACT_NODE_V4_LEN)?,
        None => Vec::new(),
    };
    let nodes6 = match r.get_bytes(b"nodes6") {
        Some(b) => parse_compact_nodes(b, COMPACT_NODE_V6_LEN)?,
        None => Vec::new(),
    };
    let mut values = Vec::new();
    if let Some(list) = r.get_list(b"values") {
        for item in list.iter().take(MAX_VALUES) {
            // Skip entries that aren't a compact address rather than drop the response:
            // one buggy peer entry shouldn't cost us the rest.
            if let Some(addr) = item.as_bytes().and_then(parse_compact_addr)
                && addr.port() != 0
                && !addr.ip().is_unspecified()
            {
                values.push(addr);
            }
        }
    }
    let token = match r.get_bytes(b"token") {
        Some(t) if t.len() <= MAX_TOKEN_LEN => Some(t.to_vec()),
        Some(_) => return Err("token too long"),
        None => None,
    };
    Ok(Response {
        id,
        nodes,
        nodes6,
        values,
        token,
        bf_seeds: get_bloom(r, b"BFsd"),
        bf_peers: get_bloom(r, b"BFpe"),
    })
}

fn get_bloom(d: &Dict<'_>, key: &[u8]) -> Option<Box<[u8; BLOOM_LEN]>> {
    d.get_bytes(key)
        .and_then(|b| <[u8; BLOOM_LEN]>::try_from(b).ok())
        .map(Box::new)
}

fn decode_error(dict: &Dict<'_>) -> Result<Body, &'static str> {
    let list = dict.get_list(b"e").ok_or("missing error list")?;
    let code = list
        .get(0)
        .and_then(Value::as_int)
        .ok_or("missing error code")?;
    let message = list
        .get(1)
        .and_then(Value::as_bytes)
        .map(|m| {
            let m = &m[..m.len().min(MAX_ERROR_MESSAGE_LEN)];
            String::from_utf8_lossy(m).into_owned()
        })
        .unwrap_or_default();
    Ok(Body::Error { code, message })
}

/// Parse a compact node string. The length must be a whole number of entries; entries
/// with port 0 or an unspecified address are skipped, and at most [`MAX_NODES`] are kept.
pub fn parse_compact_nodes(data: &[u8], entry_len: usize) -> Result<Vec<NodeInfo>, &'static str> {
    if !data.len().is_multiple_of(entry_len) {
        return Err("compact node list has a partial entry");
    }
    let mut out = Vec::with_capacity((data.len() / entry_len).min(MAX_NODES));
    for chunk in data.chunks_exact(entry_len).take(MAX_NODES) {
        let id = NodeId::from_slice(&chunk[..20]).ok_or("bad node id")?;
        let Some(addr) = parse_compact_addr(&chunk[20..]) else {
            continue;
        };
        if addr.port() == 0 || addr.ip().is_unspecified() {
            continue;
        }
        out.push(NodeInfo { id, addr });
    }
    Ok(out)
}

/// Parse a 6-byte (IPv4) or 18-byte (IPv6) compact address.
pub fn parse_compact_addr(b: &[u8]) -> Option<SocketAddr> {
    match b.len() {
        6 => {
            let ip = Ipv4Addr::new(b[0], b[1], b[2], b[3]);
            let port = u16::from_be_bytes([b[4], b[5]]);
            Some(SocketAddr::V4(SocketAddrV4::new(ip, port)))
        }
        18 => {
            let mut o = [0u8; 16];
            o.copy_from_slice(&b[..16]);
            let port = u16::from_be_bytes([b[16], b[17]]);
            Some(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from(o),
                port,
                0,
                0,
            )))
        }
        _ => None,
    }
}

/// Encode an address in compact form. An IPv4-mapped IPv6 address is written as IPv4.
pub fn encode_compact_addr(addr: &SocketAddr, out: &mut Vec<u8>) {
    match canonical(*addr) {
        SocketAddr::V4(a) => out.extend_from_slice(&a.ip().octets()),
        SocketAddr::V6(a) => out.extend_from_slice(&a.ip().octets()),
    }
    out.extend_from_slice(&addr.port().to_be_bytes());
}

/// `addr` with an IPv4-mapped IPv6 address turned into plain IPv4.
pub fn canonical(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(a) => match a.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), a.port()),
            None => addr,
        },
        SocketAddr::V4(_) => addr,
    }
}

/// Encode a compact node string. Nodes of the wrong family for the string are skipped.
pub fn encode_compact_nodes<'a>(
    nodes: impl IntoIterator<Item = &'a NodeInfo>,
    v6: bool,
) -> Vec<u8> {
    let mut out = Vec::new();
    for n in nodes {
        if canonical(n.addr).is_ipv6() == v6 {
            out.extend_from_slice(n.id.as_bytes());
            encode_compact_addr(&n.addr, &mut out);
        }
    }
    out
}

impl Message {
    /// Encode this message. Lists are written as given; the caller keeps them within a
    /// UDP datagram (the node caps `values` and `nodes` well below that).
    pub fn encode(&self) -> Vec<u8> {
        let mut root = OwnedValue::dict().insert("t", self.transaction_id.clone());
        if let Some(v) = &self.version {
            root = root.insert("v", v.clone());
        }
        if self.read_only {
            root = root.insert("ro", 1i64);
        }
        if let Some(ip) = &self.ip {
            let mut b = Vec::with_capacity(18);
            encode_compact_addr(ip, &mut b);
            root = root.insert("ip", b);
        }
        match &self.body {
            Body::Query(q) => {
                root = root
                    .insert("y", "q")
                    .insert("q", q.method().to_vec())
                    .insert("a", encode_query_args(q));
            }
            Body::Response(r) => {
                root = root.insert("y", "r").insert("r", encode_response(r));
            }
            Body::Error { code, message } => {
                root = root.insert("y", "e").insert(
                    "e",
                    vec![OwnedValue::Int(*code), OwnedValue::from(message.as_str())],
                );
            }
        }
        root.to_bytes()
    }
}

fn want_value(want: Want) -> Option<OwnedValue> {
    if !want.is_specified() {
        return None;
    }
    let mut list = Vec::new();
    if want.v4 {
        list.push(OwnedValue::from("n4"));
    }
    if want.v6 {
        list.push(OwnedValue::from("n6"));
    }
    Some(OwnedValue::List(list))
}

fn encode_query_args(q: &Query) -> OwnedValue {
    let mut a = OwnedValue::dict();
    if let Some(id) = q.sender_id() {
        a = a.insert("id", id.as_bytes().to_vec());
    }
    match q {
        Query::Ping { .. } | Query::Unknown { .. } => {}
        Query::FindNode { target, want, .. } => {
            a = a.insert("target", target.as_bytes().to_vec());
            if let Some(w) = want_value(*want) {
                a = a.insert("want", w);
            }
        }
        Query::GetPeers {
            info_hash,
            want,
            noseed,
            scrape,
            ..
        } => {
            a = a.insert("info_hash", info_hash.as_bytes().to_vec());
            if let Some(w) = want_value(*want) {
                a = a.insert("want", w);
            }
            if *noseed {
                a = a.insert("noseed", 1i64);
            }
            if *scrape {
                a = a.insert("scrape", 1i64);
            }
        }
        Query::AnnouncePeer {
            info_hash,
            port,
            implied_port,
            token,
            seed,
            ..
        } => {
            a = a
                .insert("info_hash", info_hash.as_bytes().to_vec())
                .insert("port", i64::from(*port))
                .insert("token", token.clone());
            if *implied_port {
                a = a.insert("implied_port", 1i64);
            }
            if *seed {
                a = a.insert("seed", 1i64);
            }
        }
    }
    a
}

fn encode_response(r: &Response) -> OwnedValue {
    let mut d = OwnedValue::dict().insert("id", r.id.as_bytes().to_vec());
    if !r.nodes.is_empty() {
        d = d.insert("nodes", encode_compact_nodes(&r.nodes, false));
    }
    if !r.nodes6.is_empty() {
        d = d.insert("nodes6", encode_compact_nodes(&r.nodes6, true));
    }
    if !r.values.is_empty() {
        let values = r
            .values
            .iter()
            .map(|a| {
                let mut b = Vec::with_capacity(18);
                encode_compact_addr(a, &mut b);
                OwnedValue::Bytes(b)
            })
            .collect::<Vec<_>>();
        d = d.insert("values", values);
    }
    if let Some(t) = &r.token {
        d = d.insert("token", t.clone());
    }
    if let Some(b) = &r.bf_seeds {
        d = d.insert("BFsd", b.to_vec());
    }
    if let Some(b) = &r.bf_peers {
        d = d.insert("BFpe", b.to_vec());
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(b: u8) -> NodeId {
        NodeId([b; 20])
    }

    fn roundtrip(m: &Message) {
        let bytes = m.encode();
        let back = decode(&bytes).unwrap();
        assert_eq!(&back, m, "{}", String::from_utf8_lossy(&bytes));
    }

    fn msg(body: Body) -> Message {
        Message {
            transaction_id: b"aa".to_vec(),
            version: None,
            read_only: false,
            ip: None,
            body,
        }
    }

    /// The examples from BEP 5, byte for byte.
    #[test]
    fn bep5_examples() {
        let ping = b"d1:ad2:id20:abcdefghij0123456789e1:q4:ping1:t2:aa1:y1:qe";
        let m = decode(ping).unwrap();
        assert_eq!(
            m.body,
            Body::Query(Query::Ping {
                id: NodeId(*b"abcdefghij0123456789")
            })
        );
        assert_eq!(m.encode(), ping);

        let pong = b"d1:rd2:id20:mnopqrstuvwxyz123456e1:t2:aa1:y1:re";
        let m = decode(pong).unwrap();
        assert_eq!(
            m.body,
            Body::Response(Response::with_id(NodeId(*b"mnopqrstuvwxyz123456")))
        );
        assert_eq!(m.encode(), pong);

        let find = b"d1:ad2:id20:abcdefghij01234567896:target20:mnopqrstuvwxyz123456e1:q9:find_node1:t2:aa1:y1:qe";
        assert_eq!(decode(find).unwrap().encode(), find);

        let get = b"d1:ad2:id20:abcdefghij01234567899:info_hash20:mnopqrstuvwxyz123456e1:q9:get_peers1:t2:aa1:y1:qe";
        assert_eq!(decode(get).unwrap().encode(), get);

        let values = b"d1:rd2:id20:abcdefghij01234567895:token8:aoeusnth6:valuesl6:axje.u6:idhtnmee1:t2:aa1:y1:re";
        let m = decode(values).unwrap();
        let Body::Response(r) = &m.body else {
            panic!("not a response")
        };
        assert_eq!(r.values.len(), 2);
        assert_eq!(r.token.as_deref(), Some(&b"aoeusnth"[..]));
        assert_eq!(m.encode(), values);

        let announce = b"d1:ad2:id20:abcdefghij012345678912:implied_porti1e9:info_hash20:mnopqrstuvwxyz1234564:porti6881e5:token8:aoeusnthe1:q13:announce_peer1:t2:aa1:y1:qe";
        let m = decode(announce).unwrap();
        assert!(matches!(
            m.body,
            Body::Query(Query::AnnouncePeer {
                port: 6881,
                implied_port: true,
                ..
            })
        ));
        assert_eq!(m.encode(), announce);

        let err = b"d1:eli201e23:A Generic Error Ocurrede1:t2:aa1:y1:ee";
        let m = decode(err).unwrap();
        assert_eq!(
            m.body,
            Body::Error {
                code: 201,
                message: "A Generic Error Ocurred".into()
            }
        );
        assert_eq!(m.encode(), err);
    }

    #[test]
    fn roundtrip_all_kinds() {
        roundtrip(&msg(Body::Query(Query::FindNode {
            id: id(1),
            target: id(2),
            want: Want { v4: true, v6: true },
        })));
        roundtrip(&msg(Body::Query(Query::GetPeers {
            id: id(1),
            info_hash: id(2),
            want: Want {
                v4: false,
                v6: true,
            },
            noseed: true,
            scrape: true,
        })));
        roundtrip(&msg(Body::Query(Query::AnnouncePeer {
            id: id(1),
            info_hash: id(2),
            port: 51413,
            implied_port: false,
            token: vec![1, 2, 3],
            seed: true,
        })));
        let mut r = Response::with_id(id(9));
        r.nodes = vec![NodeInfo {
            id: id(3),
            addr: "1.2.3.4:5".parse().unwrap(),
        }];
        r.nodes6 = vec![NodeInfo {
            id: id(4),
            addr: "[2001:db8::1]:6881".parse().unwrap(),
        }];
        r.values = vec![
            "5.6.7.8:9".parse().unwrap(),
            "[2001:db8::2]:7".parse().unwrap(),
        ];
        r.token = Some(b"tok".to_vec());
        r.bf_seeds = Some(Box::new([0x55; BLOOM_LEN]));
        r.bf_peers = Some(Box::new([0xaa; BLOOM_LEN]));
        let mut m = msg(Body::Response(r));
        m.ip = Some("9.9.9.9:1234".parse().unwrap());
        m.read_only = true;
        m.version = Some(b"AT01".to_vec());
        roundtrip(&m);
    }

    #[test]
    fn unknown_method_is_kept() {
        let raw = b"d1:ad2:id20:abcdefghij0123456789e1:q8:vote_foo1:t2:xy1:y1:qe";
        let m = decode(raw).unwrap();
        assert_eq!(
            m.body,
            Body::Query(Query::Unknown {
                id: Some(NodeId(*b"abcdefghij0123456789")),
                method: b"vote_foo".to_vec()
            })
        );
    }

    #[test]
    fn bad_query_args_report_transaction_id() {
        let raw = b"d1:ad2:id3:abce1:q4:ping1:t2:zz1:y1:qe";
        let e = decode(raw).unwrap_err();
        assert_eq!(e.transaction_id.as_deref(), Some(&b"zz"[..]));
    }

    #[test]
    fn rejects_garbage() {
        for raw in [
            &b""[..],
            b"le",
            b"i5e",
            b"d1:y1:qe",
            b"d1:t2:aa1:y1:xe",
            b"d1:t2:aa1:y1:re",
            b"d1:rd2:id20:abcdefghij01234567895:nodes3:abce1:t2:aa1:y1:re",
            b"d1:t17:aaaaaaaaaaaaaaaaa1:y1:qe",
        ] {
            assert!(decode(raw).is_err(), "{}", String::from_utf8_lossy(raw));
        }
    }

    #[test]
    fn skips_bad_values_and_zero_ports() {
        let mut r = Response::with_id(id(1));
        r.values = vec!["1.2.3.4:0".parse().unwrap(), "1.2.3.4:5".parse().unwrap()];
        let bytes = msg(Body::Response(r)).encode();
        let Body::Response(back) = decode(&bytes).unwrap().body else {
            panic!()
        };
        assert_eq!(
            back.values,
            vec!["1.2.3.4:5".parse::<SocketAddr>().unwrap()]
        );
    }

    #[test]
    fn mapped_addresses_encode_as_v4() {
        let a: SocketAddr = "[::ffff:1.2.3.4]:80".parse().unwrap();
        let mut b = Vec::new();
        encode_compact_addr(&a, &mut b);
        assert_eq!(b, [1, 2, 3, 4, 0, 80]);
    }
}

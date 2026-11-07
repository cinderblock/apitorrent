//! What the DHT persists between runs: its node ID and the good nodes from its routing
//! tables, as an opaque, versioned byte string that the core stores.
//!
//! The encoding is a bencoded dictionary (`v`: format version, `id`: node ID, `n4` and
//! `n6`: compact node strings), so a future version can add keys without breaking old
//! state. Decoding is as defensive as the network decoder.

use apitorrent_bencode::OwnedValue;

use crate::id::NodeId;
use crate::krpc::{
    COMPACT_NODE_V4_LEN, COMPACT_NODE_V6_LEN, NodeInfo, encode_compact_nodes, parse_compact_nodes,
};

/// The current format version.
const FORMAT_VERSION: i64 = 1;

/// The most nodes per address family that are saved or restored.
pub const MAX_SAVED_NODES: usize = 64;

/// A DHT's saved state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedState {
    /// The node ID to keep using. Keeping it lets other nodes' routing tables (and the
    /// peers they stored with us) stay useful across restarts.
    pub id: [u8; 20],
    /// Good IPv4 nodes, most recently heard from first.
    pub nodes_v4: Vec<NodeInfo>,
    /// Good IPv6 nodes, most recently heard from first.
    pub nodes_v6: Vec<NodeInfo>,
}

/// Why saved state could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid saved DHT state: {0}")]
pub struct StateError(&'static str);

impl PersistedState {
    pub fn to_bytes(&self) -> Vec<u8> {
        OwnedValue::dict()
            .insert("v", FORMAT_VERSION)
            .insert("id", self.id.to_vec())
            .insert(
                "n4",
                encode_compact_nodes(self.nodes_v4.iter().take(MAX_SAVED_NODES), false),
            )
            .insert(
                "n6",
                encode_compact_nodes(self.nodes_v6.iter().take(MAX_SAVED_NODES), true),
            )
            .to_bytes()
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, StateError> {
        let value = apitorrent_bencode::decode(bytes).map_err(|_| StateError("not bencode"))?;
        let d = value.as_dict().ok_or(StateError("not a dictionary"))?;
        match d.get_int(b"v") {
            Some(FORMAT_VERSION) => {}
            Some(_) => return Err(StateError("unsupported version")),
            None => return Err(StateError("missing version")),
        }
        let id = d
            .get_bytes(b"id")
            .and_then(NodeId::from_slice)
            .ok_or(StateError("missing or malformed id"))?;
        let nodes = |key: &[u8], len| -> Result<Vec<NodeInfo>, StateError> {
            match d.get_bytes(key) {
                Some(b) => {
                    let mut v = parse_compact_nodes(b, len).map_err(StateError)?;
                    v.truncate(MAX_SAVED_NODES);
                    Ok(v)
                }
                None => Ok(Vec::new()),
            }
        };
        Ok(Self {
            id: id.0,
            nodes_v4: nodes(b"n4", COMPACT_NODE_V4_LEN)?,
            nodes_v6: nodes(b"n6", COMPACT_NODE_V6_LEN)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let s = PersistedState {
            id: [7; 20],
            nodes_v4: vec![NodeInfo {
                id: NodeId([1; 20]),
                addr: "1.2.3.4:6881".parse().unwrap(),
            }],
            nodes_v6: vec![NodeInfo {
                id: NodeId([2; 20]),
                addr: "[2001:db8::5]:6881".parse().unwrap(),
            }],
        };
        assert_eq!(PersistedState::from_bytes(&s.to_bytes()).unwrap(), s);
    }

    #[test]
    fn rejects_bad_state() {
        assert!(PersistedState::from_bytes(b"").is_err());
        assert!(PersistedState::from_bytes(b"d1:vi2ee").is_err());
        assert!(PersistedState::from_bytes(b"d2:id3:abc1:vi1ee").is_err());
        assert!(
            PersistedState::from_bytes(b"d2:id20:aaaaaaaaaaaaaaaaaaaa2:n43:abc1:vi1ee").is_err()
        );
        assert!(PersistedState::from_bytes(b"d2:id20:aaaaaaaaaaaaaaaaaaaa1:vi1ee").is_ok());
    }
}

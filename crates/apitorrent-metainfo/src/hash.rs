use std::fmt;
use std::str::FromStr;

use sha1::Digest as _;

/// A SHA-1 digest: a v1 info-hash, or a v1 piece hash.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sha1Hash(pub [u8; 20]);

/// A SHA-256 digest: a v2 info-hash, a merkle node or a `pieces root`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sha256Hash(pub [u8; 32]);

impl Sha1Hash {
    pub fn digest(data: &[u8]) -> Self {
        Self(sha1::Sha1::digest(data).into())
    }

    pub fn from_slice(b: &[u8]) -> Option<Self> {
        b.try_into().ok().map(Self)
    }

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl Sha256Hash {
    pub const ZERO: Self = Self([0; 32]);

    pub fn digest(data: &[u8]) -> Self {
        Self(sha2::Sha256::digest(data).into())
    }

    /// The hash of two concatenated child nodes, as in a BEP 52 merkle tree.
    pub fn pair(left: &Sha256Hash, right: &Sha256Hash) -> Self {
        let mut h = sha2::Sha256::new();
        h.update(left.0);
        h.update(right.0);
        Self(h.finalize().into())
    }

    pub fn from_slice(b: &[u8]) -> Option<Self> {
        b.try_into().ok().map(Self)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0; 32]
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// The first 20 bytes. BitTorrent v2 uses the truncated hash wherever a v1 protocol
    /// field has room for only 20 bytes: the handshake, DHT and tracker announces.
    pub fn truncated(&self) -> Sha1Hash {
        let mut out = [0; 20];
        out.copy_from_slice(&self.0[..20]);
        Sha1Hash(out)
    }
}

macro_rules! hex_impls {
    ($t:ty, $len:expr) => {
        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                for b in &self.0 {
                    write!(f, "{b:02x}")?;
                }
                Ok(())
            }
        }

        impl fmt::Debug for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({self})", stringify!($t))
            }
        }

        impl FromStr for $t {
            type Err = ParseHashError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let mut out = [0u8; $len];
                hex::decode_to_slice(s, &mut out).map_err(|_| ParseHashError)?;
                Ok(Self(out))
            }
        }
    };
}

hex_impls!(Sha1Hash, 20);
hex_impls!(Sha256Hash, 32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not a valid hex digest of the expected length")]
pub struct ParseHashError;

/// The identity of a torrent: its v1 info-hash, its v2 info-hash, or both (a hybrid torrent).
///
/// At least one is always present. Two `InfoHashes` refer to the same torrent if they share
/// any hash; see [`InfoHashes::matches`].
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct InfoHashes {
    v1: Option<Sha1Hash>,
    v2: Option<Sha256Hash>,
}

impl InfoHashes {
    pub fn v1(hash: Sha1Hash) -> Self {
        Self {
            v1: Some(hash),
            v2: None,
        }
    }

    pub fn v2(hash: Sha256Hash) -> Self {
        Self {
            v1: None,
            v2: Some(hash),
        }
    }

    pub fn hybrid(v1: Sha1Hash, v2: Sha256Hash) -> Self {
        Self {
            v1: Some(v1),
            v2: Some(v2),
        }
    }

    /// Build from optional parts; `None` if both are missing.
    pub fn new(v1: Option<Sha1Hash>, v2: Option<Sha256Hash>) -> Option<Self> {
        (v1.is_some() || v2.is_some()).then_some(Self { v1, v2 })
    }

    pub fn get_v1(&self) -> Option<Sha1Hash> {
        self.v1
    }

    pub fn get_v2(&self) -> Option<Sha256Hash> {
        self.v2
    }

    pub fn is_hybrid(&self) -> bool {
        self.v1.is_some() && self.v2.is_some()
    }

    /// The hash used in 20-byte protocol fields for the v1 swarm, or for the v2 swarm when
    /// there is no v1 hash.
    pub fn primary_short(&self) -> Sha1Hash {
        match (self.v1, self.v2) {
            (Some(v1), _) => v1,
            (None, Some(v2)) => v2.truncated(),
            (None, None) => unreachable!("InfoHashes always holds at least one hash"),
        }
    }

    /// Whether the two refer to the same torrent. A magnet link may carry only one hash of a
    /// hybrid torrent.
    pub fn matches(&self, other: &InfoHashes) -> bool {
        matches!((self.v1, other.v1), (Some(a), Some(b)) if a == b)
            || matches!((self.v2, other.v2), (Some(a), Some(b)) if a == b)
    }

    /// Whether `short` (from a handshake, DHT or tracker) identifies this torrent, either as
    /// the v1 hash or as the truncated v2 hash.
    pub fn matches_short(&self, short: &Sha1Hash) -> bool {
        self.v1.as_ref() == Some(short) || self.v2.map(|h| h.truncated()).as_ref() == Some(short)
    }

    /// A stable text form: the v2 hash if there is one (it is the stronger identity),
    /// otherwise the v1 hash.
    pub fn id_string(&self) -> String {
        match (self.v1, self.v2) {
            (_, Some(v2)) => v2.to_hex(),
            (Some(v1), None) => v1.to_hex(),
            (None, None) => unreachable!("InfoHashes always holds at least one hash"),
        }
    }
}

impl fmt::Debug for InfoHashes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("InfoHashes");
        if let Some(v1) = &self.v1 {
            s.field("v1", &format_args!("{v1}"));
        }
        if let Some(v2) = &self.v2 {
            s.field("v2", &format_args!("{v2}"));
        }
        s.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let h: Sha1Hash = "b61560c2918f463768cd122b6d2fdd47b77bdb35".parse().unwrap();
        assert_eq!(h.to_string(), "b61560c2918f463768cd122b6d2fdd47b77bdb35");
        assert!("b61560c2".parse::<Sha1Hash>().is_err());
        assert!(
            "zz1560c2918f463768cd122b6d2fdd47b77bdb35"
                .parse::<Sha1Hash>()
                .is_err()
        );
    }

    #[test]
    fn matching() {
        let v1 = Sha1Hash([1; 20]);
        let v2 = Sha256Hash([2; 32]);
        let hybrid = InfoHashes::hybrid(v1, v2);
        assert!(hybrid.matches(&InfoHashes::v1(v1)));
        assert!(hybrid.matches(&InfoHashes::v2(v2)));
        assert!(!InfoHashes::v1(v1).matches(&InfoHashes::v2(v2)));
        assert!(hybrid.matches_short(&v1));
        assert!(hybrid.matches_short(&v2.truncated()));
        assert_eq!(InfoHashes::v2(v2).primary_short(), v2.truncated());
    }
}

//! Node IDs: the 160-bit XOR metric of [BEP 5], and the secure node IDs of [BEP 42].
//!
//! [BEP 5]: https://www.bittorrent.org/beps/bep_0005.html
//! [BEP 42]: https://www.bittorrent.org/beps/bep_0042.html

use std::fmt;
use std::net::IpAddr;

use rand::{Rng, RngExt};

/// The length of a node ID or info-hash in bytes.
pub const ID_LEN: usize = 20;

/// The number of bits in a node ID.
pub const ID_BITS: usize = ID_LEN * 8;

/// A DHT node ID, or an info-hash used as a lookup target. Both live in the same 160-bit
/// space and are compared with the XOR metric.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct NodeId(pub [u8; ID_LEN]);

impl NodeId {
    /// An ID from exactly 20 bytes, or `None` for any other length.
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(Self)
    }

    /// A uniformly random ID.
    pub fn random(rng: &mut impl Rng) -> Self {
        let mut id = [0u8; ID_LEN];
        rng.fill(&mut id);
        Self(id)
    }

    pub fn as_bytes(&self) -> &[u8; ID_LEN] {
        &self.0
    }

    /// The XOR distance to `other`. Distances compare as big-endian integers, which is how
    /// the derived `Ord` on `NodeId` compares them.
    #[must_use]
    pub fn distance(&self, other: &NodeId) -> NodeId {
        let mut d = [0u8; ID_LEN];
        for (i, out) in d.iter_mut().enumerate() {
            *out = self.0[i] ^ other.0[i];
        }
        NodeId(d)
    }

    /// The number of leading bits this ID shares with `other`: 160 when they are equal.
    pub fn common_prefix_len(&self, other: &NodeId) -> usize {
        for (i, (a, b)) in self.0.iter().zip(other.0.iter()).enumerate() {
            let x = a ^ b;
            if x != 0 {
                return i * 8 + x.leading_zeros() as usize;
            }
        }
        ID_BITS
    }

    /// The value of bit `index`, counting from the most significant bit.
    pub fn bit(&self, index: usize) -> bool {
        self.0[index / 8] & (0x80 >> (index % 8)) != 0
    }

    /// A random ID that shares exactly `prefix_len` leading bits with `self` (all 160 bits
    /// when `prefix_len >= 160`). Used to refresh the bucket at that depth.
    #[must_use]
    pub fn random_with_prefix(&self, prefix_len: usize, rng: &mut impl Rng) -> NodeId {
        if prefix_len >= ID_BITS {
            return *self;
        }
        let mut out = NodeId::random(rng).0;
        let full = prefix_len / 8;
        out[..full].copy_from_slice(&self.0[..full]);
        let rem = prefix_len % 8;
        // Keep `rem` bits of byte `full` from self, flip the next bit, randomise the rest.
        let keep_mask: u8 = if rem == 0 { 0 } else { 0xff << (8 - rem) };
        let flip: u8 = 0x80 >> rem;
        let random_mask: u8 = !keep_mask & !flip;
        out[full] = (self.0[full] & keep_mask) | (!self.0[full] & flip) | (out[full] & random_mask);
        NodeId(out)
    }

    /// A [BEP 42] secure ID for a node whose external address is `ip`. The first 21 bits
    /// are derived from the address, and the last byte holds the random seed `r`.
    ///
    /// [BEP 42]: https://www.bittorrent.org/beps/bep_0042.html
    pub fn secure(ip: IpAddr, rng: &mut impl Rng) -> NodeId {
        let rand_byte: u8 = rng.random();
        let mut id = NodeId::random(rng).0;
        let crc = secure_crc(ip, rand_byte & 0x07);
        let [c0, c1, c2, _] = crc.to_be_bytes();
        id[0] = c0;
        id[1] = c1;
        id[2] = (c2 & 0xf8) | (id[2] & 0x07);
        id[ID_LEN - 1] = rand_byte;
        NodeId(id)
    }

    /// Whether this ID is a valid [BEP 42] ID for a node at `ip`. Addresses that BEP 42
    /// exempts (local, private and link-local ranges) accept any ID.
    ///
    /// [BEP 42]: https://www.bittorrent.org/beps/bep_0042.html
    pub fn is_secure_for(&self, ip: IpAddr) -> bool {
        if is_exempt(ip) {
            return true;
        }
        self.matches_secure_prefix(ip)
    }

    /// Whether the first 21 bits match `ip`, without the exemption for local addresses.
    pub(crate) fn matches_secure_prefix(&self, ip: IpAddr) -> bool {
        let crc = secure_crc(ip, self.0[ID_LEN - 1] & 0x07);
        let [c0, c1, c2, _] = crc.to_be_bytes();
        self.0[0] == c0 && self.0[1] == c1 && (self.0[2] & 0xf8) == (c2 & 0xf8)
    }
}

/// The CRC32-C that BEP 42 derives the ID prefix from: the masked address, with the random
/// seed `r` (0 to 7) in the top three bits of its first byte.
fn secure_crc(ip: IpAddr, r: u8) -> u32 {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let mask: [u8; 4] = [0x03, 0x0f, 0x3f, 0xff];
            let mut b = v4.octets();
            for (x, m) in b.iter_mut().zip(mask) {
                *x &= m;
            }
            b[0] |= r << 5;
            crc32c(&b)
        }
        IpAddr::V6(v6) => {
            let mask: [u8; 8] = [0x01, 0x03, 0x07, 0x0f, 0x1f, 0x3f, 0x7f, 0xff];
            let o = v6.octets();
            let mut b = [0u8; 8];
            for i in 0..8 {
                b[i] = o[i] & mask[i];
            }
            b[0] |= r << 5;
            crc32c(&b)
        }
    }
}

/// Whether BEP 42 exempts `ip` from the ID restriction: loopback, private, link-local and
/// unique-local ranges. These are also exempt from the one-node-per-IP routing rule, so a
/// DHT on a LAN or on loopback works.
pub fn is_exempt(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            let seg0 = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || (seg0 & 0xfe00) == 0xfc00 // unique local
                || (seg0 & 0xffc0) == 0xfe80 // link local
        }
    }
}

/// CRC32-C (Castagnoli), as BEP 42 specifies. Bitwise, because it runs only when an ID is
/// generated or checked.
fn crc32c(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0x82F6_3B78 & mask);
        }
    }
    !crc
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId(")?;
        for b in &self.0 {
            write!(f, "{b:02x}")?;
        }
        write!(f, ")")
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in &self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl From<[u8; ID_LEN]> for NodeId {
    fn from(b: [u8; ID_LEN]) -> Self {
        Self(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn hex20(s: &str) -> NodeId {
        let mut out = [0u8; 20];
        for (i, o) in out.iter_mut().enumerate() {
            *o = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap();
        }
        NodeId(out)
    }

    #[test]
    fn crc32c_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    /// The test vectors from BEP 42.
    #[test]
    fn bep42_vectors() {
        let cases = [
            ("124.31.75.21", "5fbfbff10c5d6a4ec8a88e4c6ab4c28b95eee401"),
            ("21.75.31.124", "5a3ce9c14e7a08645677bbd1cfe7d8f956d53216"),
            ("65.23.51.170", "a5d43220bc8f112a3d426c84764f8c2a1150e616"),
            ("84.124.73.14", "1b0321dd1bb1fe518101ceef99462b947a01ff41"),
            ("43.213.53.83", "e56f6cbf5b7c4be0237986d5243b87aa6d51305a"),
        ];
        for (ip, id) in cases {
            let ip: IpAddr = ip.parse().unwrap();
            let id = hex20(id);
            assert!(id.is_secure_for(ip), "{ip} {id}");
            // A different address does not validate.
            assert!(!id.is_secure_for(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        }
    }

    #[test]
    fn generated_secure_ids_validate() {
        let mut rng = StdRng::seed_from_u64(7);
        for _ in 0..200 {
            let v4 = IpAddr::V4(Ipv4Addr::from(rng.random::<u32>() | 0x0100_0000));
            let id = NodeId::secure(v4, &mut rng);
            assert!(id.matches_secure_prefix(v4));
            let v6 = IpAddr::V6(Ipv6Addr::from(rng.random::<u128>()));
            let id = NodeId::secure(v6, &mut rng);
            assert!(id.matches_secure_prefix(v6));
        }
    }

    #[test]
    fn exemptions() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.9",
            "172.16.4.4",
            "169.254.1.1",
            "::1",
        ] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(is_exempt(ip), "{ip}");
            assert!(NodeId([0xab; 20]).is_secure_for(ip));
        }
        for ip in ["8.8.8.8", "2001:db8::1", "::ffff:8.8.8.8"] {
            assert!(!is_exempt(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn distance_and_prefix() {
        let a = NodeId([0; 20]);
        let mut b = [0u8; 20];
        b[2] = 0x10;
        let b = NodeId(b);
        assert_eq!(a.common_prefix_len(&b), 19);
        assert_eq!(a.common_prefix_len(&a), 160);
        assert_eq!(a.distance(&b), b);
        assert!(a.distance(&b) < NodeId([0xff; 20]));
        assert!(b.bit(19));
        assert!(!b.bit(18));
    }

    #[test]
    fn random_with_prefix_has_exact_prefix() {
        let mut rng = StdRng::seed_from_u64(1);
        let me = NodeId::random(&mut rng);
        for p in 0..160 {
            let id = me.random_with_prefix(p, &mut rng);
            assert_eq!(me.common_prefix_len(&id), p, "prefix {p}");
        }
        assert_eq!(me.random_with_prefix(160, &mut rng), me);
    }
}

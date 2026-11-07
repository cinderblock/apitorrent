//! The cryptographic pieces of MSE: the Diffie-Hellman group, the key derivations and RC4.
//!
//! MSE uses a fixed 768-bit safe prime (the Oakley group 1 prime from RFC 2409) with
//! generator 2. Private keys are 160 bits, as the spec recommends. Exponentiation runs in
//! constant time in Montgomery form, so the private key never steers a branch or a memory
//! access.

use std::fmt;
use std::sync::LazyLock;

use apitorrent_metainfo::Sha1Hash;
use crypto_bigint::modular::{FixedMontyForm, FixedMontyParams};
use crypto_bigint::{Odd, U768};
use rc4::{KeyInit, Rc4, StreamCipher};
use sha1::{Digest, Sha1};

/// The length of a public key or a shared secret on the wire: 768 bits.
pub const KEY_LEN: usize = 96;

/// The length of a private key: 160 bits.
pub const PRIVATE_KEY_LEN: usize = 20;

/// The prime P, big-endian hex.
const PRIME_HEX: &str = "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74\
                         020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F1437\
                         4FE1356D6D51C245E485B576625E7EC6F44C42E9A63A36210000000000090563";

/// The prime P.
pub const PRIME: U768 = U768::from_be_hex(PRIME_HEX);

/// The generator G.
pub const GENERATOR: u64 = 2;

/// RC4 keystream bytes thrown away before use, against the known biases at its start.
pub const RC4_DISCARD: usize = 1024;

static PARAMS: LazyLock<FixedMontyParams<{ U768::LIMBS }>> =
    LazyLock::new(|| FixedMontyParams::new_vartime(Odd::new(PRIME).expect("the MSE prime is odd")));

/// A Diffie-Hellman public key, as 96 big-endian bytes.
pub type PublicKey = [u8; KEY_LEN];

/// The shared secret S, as 96 big-endian bytes.
pub type SharedSecret = [u8; KEY_LEN];

/// One side's Diffie-Hellman key pair.
#[derive(Clone)]
pub struct KeyPair {
    private: U768,
    public: PublicKey,
}

impl fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyPair")
            .field("public", &Hex(&self.public))
            .finish_non_exhaustive()
    }
}

impl KeyPair {
    /// Derive the key pair for a 160-bit private key (big-endian).
    pub fn from_private(private: &[u8; PRIVATE_KEY_LEN]) -> Self {
        let mut wide = [0u8; KEY_LEN];
        wide[KEY_LEN - PRIVATE_KEY_LEN..].copy_from_slice(private);
        let private = U768::from_be_slice(&wide);
        let base = FixedMontyForm::new(&U768::from_u64(GENERATOR), &PARAMS);
        let public = to_bytes(&pow(&base, &private));
        Self { private, public }
    }

    /// A fresh key pair from a cryptographically secure generator.
    pub fn generate<R: rand::CryptoRng + ?Sized>(rng: &mut R) -> Self {
        let mut private = [0u8; PRIVATE_KEY_LEN];
        rng.fill_bytes(&mut private);
        Self::from_private(&private)
    }

    pub fn public(&self) -> &PublicKey {
        &self.public
    }

    /// The shared secret with the other side's public key, or `None` when that key is
    /// degenerate (0, 1, P - 1, or not below P), which would force a guessable secret.
    pub fn shared_secret(&self, theirs: &PublicKey) -> Option<SharedSecret> {
        let y = U768::from_be_slice(theirs);
        let p_minus_one = PRIME.wrapping_sub(&U768::ONE);
        if y <= U768::ONE || y >= p_minus_one {
            return None;
        }
        let base = FixedMontyForm::new(&y, &PARAMS);
        Some(to_bytes(&pow(&base, &self.private)))
    }
}

fn pow(base: &FixedMontyForm<{ U768::LIMBS }>, exp: &U768) -> U768 {
    // The exponent is at most 160 bits; bounding it keeps the ladder short and still
    // constant-time over those 160 bits.
    #[allow(clippy::cast_possible_truncation)]
    let bits = (PRIVATE_KEY_LEN * 8) as u32;
    base.pow_bounded_exp(exp, bits).retrieve()
}

fn to_bytes(n: &U768) -> [u8; KEY_LEN] {
    let mut out = [0u8; KEY_LEN];
    out.copy_from_slice(&n.to_be_bytes());
    out
}

/// SHA-1 over the concatenation of `parts`.
pub fn hash(parts: &[&[u8]]) -> [u8; 20] {
    let mut h = Sha1::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// `HASH('req1', S)`: the receiver's synchronisation marker.
pub fn req1(secret: &SharedSecret) -> [u8; 20] {
    hash(&[b"req1", secret])
}

/// `HASH('req2', SKEY)`: what identifies a torrent without revealing its info-hash.
/// Receivers index their torrents by this value.
pub fn req2(info_hash: &Sha1Hash) -> [u8; 20] {
    hash(&[b"req2", info_hash.as_bytes()])
}

/// `HASH('req3', S)`: the mask over `req2`.
pub fn req3(secret: &SharedSecret) -> [u8; 20] {
    hash(&[b"req3", secret])
}

/// `HASH('req2', SKEY) xor HASH('req3', S)`, as the initiator sends it.
pub fn masked_req2(info_hash: &Sha1Hash, secret: &SharedSecret) -> [u8; 20] {
    xor20(&req2(info_hash), &req3(secret))
}

pub(crate) fn xor20(a: &[u8; 20], b: &[u8; 20]) -> [u8; 20] {
    std::array::from_fn(|i| a[i] ^ b[i])
}

/// The initiator's RC4 key, `HASH('keyA', S, SKEY)`.
pub fn key_a(secret: &SharedSecret, info_hash: &Sha1Hash) -> [u8; 20] {
    hash(&[b"keyA", secret, info_hash.as_bytes()])
}

/// The receiver's RC4 key, `HASH('keyB', S, SKEY)`.
pub fn key_b(secret: &SharedSecret, info_hash: &Sha1Hash) -> [u8; 20] {
    hash(&[b"keyB", secret, info_hash.as_bytes()])
}

/// One direction of an RC4 stream, with the first 1024 keystream bytes already discarded.
///
/// Encryption and decryption are the same operation: XOR with the keystream.
pub struct Rc4Cipher(Rc4);

impl fmt::Debug for Rc4Cipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Rc4Cipher(..)")
    }
}

impl Rc4Cipher {
    pub fn new(key: &[u8; 20]) -> Self {
        let mut rc4 = Rc4::new_from_slice(key).expect("RC4 takes 20-byte keys");
        let mut discard = [0u8; RC4_DISCARD];
        rc4.apply_keystream(&mut discard);
        Self(rc4)
    }

    /// XOR `buf` with the next `buf.len()` keystream bytes.
    pub fn apply(&mut self, buf: &mut [u8]) {
        self.0.apply_keystream(buf);
    }
}

/// The two directions of an encrypted connection.
#[derive(Debug)]
pub struct Ciphers {
    /// Encrypts what we send.
    pub encrypt: Rc4Cipher,
    /// Decrypts what we receive.
    pub decrypt: Rc4Cipher,
}

impl Ciphers {
    /// The ciphers for the side that opened the connection.
    pub fn initiator(secret: &SharedSecret, info_hash: &Sha1Hash) -> Self {
        Self {
            encrypt: Rc4Cipher::new(&key_a(secret, info_hash)),
            decrypt: Rc4Cipher::new(&key_b(secret, info_hash)),
        }
    }

    /// The ciphers for the side that accepted the connection.
    pub fn receiver(secret: &SharedSecret, info_hash: &Sha1Hash) -> Self {
        Self {
            encrypt: Rc4Cipher::new(&key_b(secret, info_hash)),
            decrypt: Rc4Cipher::new(&key_a(secret, info_hash)),
        }
    }
}

struct Hex<'a>(&'a [u8]);

impl fmt::Debug for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_key_one_gives_the_generator() {
        let mut one = [0u8; PRIVATE_KEY_LEN];
        one[PRIVATE_KEY_LEN - 1] = 1;
        let pair = KeyPair::from_private(&one);
        let mut expected = [0u8; KEY_LEN];
        expected[KEY_LEN - 1] = 2;
        assert_eq!(pair.public(), &expected);
    }

    #[test]
    fn both_sides_agree() {
        let mut rng = rand::rng();
        let a = KeyPair::generate(&mut rng);
        let b = KeyPair::generate(&mut rng);
        assert_eq!(
            a.shared_secret(b.public()).unwrap(),
            b.shared_secret(a.public()).unwrap()
        );
    }

    #[test]
    fn degenerate_public_keys_are_refused() {
        let pair = KeyPair::generate(&mut rand::rng());
        let mut k = [0u8; KEY_LEN];
        assert!(pair.shared_secret(&k).is_none());
        k[KEY_LEN - 1] = 1;
        assert!(pair.shared_secret(&k).is_none());
        let p = to_bytes(&PRIME);
        assert!(pair.shared_secret(&p).is_none());
        let p1 = to_bytes(&PRIME.wrapping_sub(&U768::ONE));
        assert!(pair.shared_secret(&p1).is_none());
        assert!(pair.shared_secret(&[0xff; KEY_LEN]).is_none());
        k[KEY_LEN - 1] = 2;
        assert!(pair.shared_secret(&k).is_some());
    }

    #[test]
    fn rc4_is_symmetric() {
        let key = [7u8; 20];
        let mut data = *b"hello, encrypted world";
        Rc4Cipher::new(&key).apply(&mut data);
        assert_ne!(&data, b"hello, encrypted world");
        Rc4Cipher::new(&key).apply(&mut data);
        assert_eq!(&data, b"hello, encrypted world");
    }
}

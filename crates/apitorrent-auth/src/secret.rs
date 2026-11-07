//! Random secrets and their digests.
//!
//! Every secret we hand out (session ids, API tokens, the setup token) has at least 160 bits
//! from the OS random source, and only its SHA-256 digest is stored. A fast hash is right
//! here: with that much entropy there is nothing for a slow hash to protect.

use data_encoding::{BASE32_NOPAD, BASE64URL_NOPAD};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub(crate) type Digest32 = [u8; 32];

pub(crate) fn random<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).expect("the OS random source is available");
    buf
}

pub(crate) fn sha256(data: &[u8]) -> Digest32 {
    Sha256::digest(data).into()
}

/// A 256-bit secret, base64url without padding (43 characters).
pub(crate) fn token256() -> String {
    BASE64URL_NOPAD.encode(&random::<32>())
}

/// A 128-bit public identifier in hex.
pub(crate) fn public_id() -> String {
    hex::encode(random::<16>())
}

/// A 160-bit token that is easy to read off a console: lowercase base32 in groups of four.
pub(crate) fn readable_token() -> String {
    let raw = BASE32_NOPAD.encode(&random::<20>()).to_ascii_lowercase();
    let chars: Vec<char> = raw.chars().collect();
    chars
        .chunks(4)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

/// Undo [`readable_token`]'s formatting so pasted variants still match.
pub(crate) fn normalize_readable(token: &str) -> String {
    token
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

pub(crate) fn digests_equal(a: &Digest32, b: &Digest32) -> bool {
    a.ct_eq(b).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes() {
        assert_eq!(token256().len(), 43);
        assert_eq!(public_id().len(), 32);
        let t = readable_token();
        assert_eq!(t.len(), 32 + 7, "{t}");
        assert_eq!(normalize_readable(&t.to_uppercase()).len(), 32);
        assert_ne!(token256(), token256());
    }
}

//! The update signing key and its signatures: minisign, Ed25519.
//!
//! Two encodings are accepted for both keys and signatures:
//!
//! - **Tauri's**: base64 of the whole minisign file. `tauri signer generate` writes keys
//!   this way and `tauri signer sign` writes signatures (`<file>.sig`) this way, and the
//!   desktop app's updater reads the same key.
//! - **minisign's own**: the two-line `untrusted comment: …` file (signatures have four
//!   lines), or a bare base64 public key line.
//!
//! Only prehashed signatures (`ED`, what every current minisign and Tauri make) verify;
//! the legacy algorithm is refused.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

include!(concat!(env!("OUT_DIR"), "/embedded_key.rs"));

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyError {
    #[error("not a minisign public key: {0}")]
    BadKey(String),
    #[error("not a minisign signature: {0}")]
    BadSignature(String),
    #[error("the signature was made with a different key")]
    WrongKey,
    #[error("the signature doesn't match the data")]
    Invalid,
    #[error("the signature uses minisign's legacy algorithm, which isn't accepted")]
    Legacy,
}

/// A public key that update signatures must verify against.
#[derive(Clone)]
pub struct UpdateKey {
    key: minisign_verify::PublicKey,
    /// The text it was parsed from, for display and for the desktop updater.
    text: String,
}

impl std::fmt::Debug for UpdateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateKey")
            .field("text", &self.text)
            .finish_non_exhaustive()
    }
}

/// `text` itself if it looks like a minisign file, else its base64 decoding if that does.
fn unwrap_tauri(text: &str) -> Option<String> {
    let text = text.trim();
    if text.starts_with("untrusted comment:") {
        return Some(text.to_owned());
    }
    let compact: String = text.split_whitespace().collect();
    let decoded = STANDARD.decode(compact).ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    decoded
        .trim_start()
        .starts_with("untrusted comment:")
        .then_some(decoded)
}

impl UpdateKey {
    /// The key compiled in from `release/update-signing.pub`, if the build had one.
    ///
    /// # Panics
    ///
    /// If the compiled-in key doesn't parse, which a test catches before release.
    pub fn embedded() -> Option<Self> {
        EMBEDDED_KEY.map(|k| {
            Self::parse(k).expect("release/update-signing.pub holds a minisign public key")
        })
    }

    /// Parse a public key in either encoding (see the module docs).
    pub fn parse(text: &str) -> Result<Self, KeyError> {
        let key = match unwrap_tauri(text) {
            Some(file) => minisign_verify::PublicKey::decode(&file),
            None => minisign_verify::PublicKey::from_base64(text.trim()),
        }
        .map_err(|e| KeyError::BadKey(e.to_string()))?;
        Ok(Self {
            key,
            text: text.trim().to_owned(),
        })
    }

    /// The key as it was given.
    pub fn as_text(&self) -> &str {
        &self.text
    }

    /// Check `signature` (either encoding) over `data`.
    pub fn verify(&self, data: &[u8], signature: &str) -> Result<(), KeyError> {
        let file = unwrap_tauri(signature)
            .ok_or_else(|| KeyError::BadSignature("no `untrusted comment:` line".to_owned()))?;
        let sig = minisign_verify::Signature::decode(&file)
            .map_err(|e| KeyError::BadSignature(e.to_string()))?;
        self.key.verify(data, &sig, false).map_err(|e| match e {
            minisign_verify::Error::UnexpectedKeyId => KeyError::WrongKey,
            minisign_verify::Error::UnexpectedAlgorithm => KeyError::Legacy,
            _ => KeyError::Invalid,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A throwaway key pair, as Tauri's tools encode them.
    pub(crate) struct TestKey {
        pub(crate) sk: minisign::SecretKey,
        pub(crate) pk: minisign::PublicKey,
    }

    impl TestKey {
        pub(crate) fn new() -> Self {
            let minisign::KeyPair { pk, sk } =
                minisign::KeyPair::generate_unencrypted_keypair().unwrap();
            Self { sk, pk }
        }

        /// The public key file, base64-wrapped like `tauri signer generate` writes it.
        pub(crate) fn tauri_public(&self) -> String {
            STANDARD.encode(self.pk.to_box().unwrap().into_string())
        }

        /// A signature like `tauri signer sign` writes it.
        pub(crate) fn sign(&self, data: &[u8]) -> String {
            let sig = minisign::sign(Some(&self.pk), &self.sk, data, None, None).unwrap();
            STANDARD.encode(sig.into_string())
        }
    }

    #[test]
    fn good_signature_verifies_in_both_encodings() {
        let k = TestKey::new();
        let key = UpdateKey::parse(&k.tauri_public()).unwrap();
        let sig = k.sign(b"hello");
        key.verify(b"hello", &sig).unwrap();
        // minisign's own encodings.
        let plain_sig = String::from_utf8(STANDARD.decode(&sig).unwrap()).unwrap();
        key.verify(b"hello", &plain_sig).unwrap();
        let plain_key = UpdateKey::parse(&k.pk.to_box().unwrap().into_string()).unwrap();
        plain_key.verify(b"hello", &sig).unwrap();
        let bare = UpdateKey::parse(&k.pk.to_base64()).unwrap();
        bare.verify(b"hello", &sig).unwrap();
    }

    #[test]
    fn bad_signatures_fail() {
        let k = TestKey::new();
        let key = UpdateKey::parse(&k.tauri_public()).unwrap();
        let sig = k.sign(b"hello");
        assert_eq!(key.verify(b"hellO", &sig), Err(KeyError::Invalid));
        let other = TestKey::new();
        assert_eq!(
            key.verify(b"hello", &other.sign(b"hello")),
            Err(KeyError::WrongKey)
        );
        assert!(matches!(
            key.verify(b"hello", "garbage"),
            Err(KeyError::BadSignature(_))
        ));
        assert!(matches!(
            key.verify(b"hello", ""),
            Err(KeyError::BadSignature(_))
        ));
        // A flipped bit in the signature itself.
        let mut file = String::from_utf8(STANDARD.decode(&sig).unwrap()).unwrap();
        let line = file.lines().nth(1).unwrap().to_owned();
        let mut raw = STANDARD.decode(&line).unwrap();
        raw[20] ^= 1;
        file = file.replace(&line, &STANDARD.encode(raw));
        assert_eq!(key.verify(b"hello", &file), Err(KeyError::Invalid));
    }

    #[test]
    fn bad_keys_are_refused() {
        assert!(UpdateKey::parse("").is_err());
        assert!(UpdateKey::parse("not a key").is_err());
        assert!(UpdateKey::parse(&STANDARD.encode("untrusted comment: x\nAAAA")).is_err());
    }

    #[test]
    fn the_compiled_in_key_parses() {
        // Only meaningful once release/update-signing.pub exists.
        let _ = UpdateKey::embedded();
    }
}

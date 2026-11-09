//! Channel signing: the instance's Ed25519 key, signing a channel document, and checking a
//! feed against a pinned publisher key.
//!
//! Ed25519 comes from `ring`, already in the tree as rustls's crypto provider: no second
//! implementation to build or audit. The signature covers the document's exact JSON bytes,
//! prefixed with a domain tag (`apitorrent-channel-v1\0`) so the key can't be made to sign
//! anything usable elsewhere. There's no canonical-JSON step to get wrong: the payload is
//! carried as base64 and verified before it is parsed.

use std::io;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair as _, UnparsedPublicKey};
use sha2::Digest as _;

use apitorrent_types::automation::{CHANNEL_FORMAT, ChannelDocument, ChannelEntry, SignedChannel};

/// What the signature covers, before the payload.
const DOMAIN: &[u8] = b"apitorrent-channel-v1\0";

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// The instance's channel signing key.
pub struct ChannelKey {
    pair: Ed25519KeyPair,
}

impl std::fmt::Debug for ChannelKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelKey")
            .field("public", &self.public_base64())
            .finish_non_exhaustive()
    }
}

impl ChannelKey {
    /// Load the key from `path` (PKCS#8 v2), or make one and save it there, owner-only.
    pub fn load_or_create(path: &Path) -> io::Result<Self> {
        match std::fs::read(path) {
            Ok(der) => Self::from_pkcs8(&der).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {e}", path.display()),
                )
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let rng = ring::rand::SystemRandom::new();
                let doc = Ed25519KeyPair::generate_pkcs8(&rng)
                    .map_err(|_| io::Error::other("could not generate the channel key"))?;
                write_private(path, doc.as_ref())?;
                tracing::info!(path = %path.display(), "made the channel signing key");
                Self::from_pkcs8(doc.as_ref()).map_err(io::Error::other)
            }
            Err(e) => Err(e),
        }
    }

    pub fn from_pkcs8(der: &[u8]) -> Result<Self, String> {
        Ed25519KeyPair::from_pkcs8(der)
            .map(|pair| Self { pair })
            .map_err(|e| format!("not an Ed25519 key: {e}"))
    }

    pub fn public_base64(&self) -> String {
        B64.encode(self.pair.public_key().as_ref())
    }

    /// Sign a document.
    pub fn sign(&self, doc: &ChannelDocument) -> SignedChannel {
        let payload = serde_json::to_vec(doc).expect("documents serialise");
        let mut msg = DOMAIN.to_vec();
        msg.extend_from_slice(&payload);
        SignedChannel {
            format: CHANNEL_FORMAT,
            payload: B64.encode(&payload),
            signature: B64.encode(self.pair.sign(&msg).as_ref()),
            public_key: self.public_base64(),
        }
    }
}

/// Write a secret file so only its owner can read it, replacing it atomically.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp: PathBuf = path.with_extension("tmp");
    {
        use io::Write as _;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    #[error("the feed is malformed: {0}")]
    Malformed(String),
    #[error(
        "the feed is signed by a different key than the one pinned for this subscription \
         ({presented}); if the publisher really changed its key, subscribe again"
    )]
    WrongKey { presented: String },
    #[error("the feed's signature doesn't verify")]
    BadSignature,
    #[error("the feed format {0} isn't supported")]
    Format(u32),
}

fn decode_key(b64: &str) -> Result<[u8; 32], VerifyError> {
    let raw = B64
        .decode(b64.trim())
        .map_err(|e| VerifyError::Malformed(format!("public key: {e}")))?;
    raw.try_into()
        .map_err(|_| VerifyError::Malformed("an Ed25519 public key is 32 bytes".to_owned()))
}

/// Check `feed` against `pinned` (base64), or, with nothing pinned yet, against the key it
/// names. Returns the document and the key that verified it (base64).
pub fn verify(
    feed: &SignedChannel,
    pinned: Option<&str>,
) -> Result<(ChannelDocument, String), VerifyError> {
    if feed.format != CHANNEL_FORMAT {
        return Err(VerifyError::Format(feed.format));
    }
    let presented = decode_key(&feed.public_key)?;
    let key = match pinned {
        Some(p) => {
            let pinned = decode_key(p)?;
            if pinned != presented {
                return Err(VerifyError::WrongKey {
                    presented: feed.public_key.clone(),
                });
            }
            pinned
        }
        None => presented,
    };
    let payload = B64
        .decode(feed.payload.trim())
        .map_err(|e| VerifyError::Malformed(format!("payload: {e}")))?;
    let signature = B64
        .decode(feed.signature.trim())
        .map_err(|e| VerifyError::Malformed(format!("signature: {e}")))?;
    let mut msg = DOMAIN.to_vec();
    msg.extend_from_slice(&payload);
    UnparsedPublicKey::new(&ED25519, key)
        .verify(&msg, &signature)
        .map_err(|_| VerifyError::BadSignature)?;
    let doc: ChannelDocument = serde_json::from_slice(&payload)
        .map_err(|e| VerifyError::Malformed(format!("document: {e}")))?;
    if doc.format != CHANNEL_FORMAT {
        return Err(VerifyError::Format(doc.format));
    }
    Ok((doc, B64.encode(key)))
}

/// A hash of what a channel lists, to tell when its revision must move.
pub fn entries_hash(entries: &[ChannelEntry]) -> Vec<u8> {
    let json = serde_json::to_vec(entries).expect("entries serialise");
    sha2::Sha256::digest(json).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(revision: u64) -> ChannelDocument {
        ChannelDocument {
            format: CHANNEL_FORMAT,
            channel: "ubuntu".into(),
            name: "Ubuntu".into(),
            description: String::new(),
            revision,
            issued_at: 1,
            entries: vec![],
        }
    }

    #[test]
    fn signs_verifies_and_rejects_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("automation").join("channel-key.p8");
        let key = ChannelKey::load_or_create(&path).unwrap();
        let again = ChannelKey::load_or_create(&path).unwrap();
        assert_eq!(
            key.public_base64(),
            again.public_base64(),
            "the key persists"
        );

        let feed = key.sign(&doc(3));
        let (d, k) = verify(&feed, Some(&key.public_base64())).unwrap();
        assert_eq!(d.revision, 3);
        assert_eq!(k, key.public_base64());
        assert!(verify(&feed, None).is_ok(), "trust on first use");

        // A changed payload.
        let mut bad = feed.clone();
        let mut forged = doc(4);
        forged.entries = vec![];
        bad.payload = B64.encode(serde_json::to_vec(&forged).unwrap());
        assert_eq!(verify(&bad, None), Err(VerifyError::BadSignature));

        // Another key, even with a valid signature of its own.
        let other_dir = tempfile::tempdir().unwrap();
        let other = ChannelKey::load_or_create(&other_dir.path().join("k")).unwrap();
        let theirs = other.sign(&doc(9));
        assert!(matches!(
            verify(&theirs, Some(&key.public_base64())),
            Err(VerifyError::WrongKey { .. })
        ));
        // A signature over the payload without the domain tag doesn't verify.
        let raw = serde_json::to_vec(&doc(5)).unwrap();
        let mut plain = feed.clone();
        plain.payload = B64.encode(&raw);
        plain.signature = B64.encode(key.pair.sign(&raw).as_ref());
        assert_eq!(verify(&plain, None), Err(VerifyError::BadSignature));
    }
}

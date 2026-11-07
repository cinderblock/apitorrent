//! Message Stream Encryption (MSE), also called Protocol Encryption (PE): the obfuscated,
//! optionally RC4-encrypted transport that most BitTorrent clients speak.
//!
//! The protocol (the Vuze/libtorrent specification): a Diffie-Hellman key exchange over a
//! 768-bit prime with generator 2, random padding to hide message boundaries, the torrent
//! identified by `HASH('req2', info_hash)` (so a passive observer can't read it), and
//! RC4 keys derived per direction with the first 1024 keystream bytes discarded. The two
//! sides agree on carrying the payload either in the clear or RC4-encrypted.
//!
//! Two layers:
//!
//! - [`Initiator`] and [`Receiver`]: sans-IO handshake state machines. They take bytes,
//!   return bytes to send, and never read past the end of the handshake.
//! - [`MseStream`]: runs the handshake over any tokio `AsyncRead + AsyncWrite` with a
//!   timeout, then is itself `AsyncRead + AsyncWrite`. An accepted stream also recognises
//!   a plain BitTorrent handshake and passes it through when the [`Policy`] allows.
//!
//! ```no_run
//! # async fn demo(tcp: tokio::net::TcpStream, info_hash: apitorrent_metainfo::Sha1Hash,
//! #               handshake: &[u8]) -> Result<(), apitorrent_mse::MseError> {
//! use apitorrent_mse::{MseConfig, MseStream, Policy};
//! let stream = MseStream::connect(tcp, info_hash, handshake, MseConfig::new(Policy::Required))
//!     .await?;
//! # Ok(()) }
//! ```

mod crypto;
mod error;
mod handshake;
mod stream;

pub use crypto::{
    Ciphers, GENERATOR, KEY_LEN, KeyPair, PRIME, PRIVATE_KEY_LEN, PublicKey, RC4_DISCARD,
    Rc4Cipher, SharedSecret, key_a, key_b, masked_req2, req1, req2, req3,
};
pub use error::MseError;
pub use handshake::{
    BT_PREFIX, CryptoMethod, CryptoSet, Established, Incoming, Initiator, LocalSecrets,
    MAX_INITIAL_PAYLOAD, MAX_PAD, Policy, Preference, REQ1_SYNC_LIMIT, Receiver, SkeyLookup,
    SkeyTable, VC, VC_SYNC_LIMIT,
};
pub use stream::{
    DEFAULT_HANDSHAKE_TIMEOUT, Encryption, MseConfig, MseReadHalf, MseStream, MseWriteHalf,
    ReadState, WriteState,
};

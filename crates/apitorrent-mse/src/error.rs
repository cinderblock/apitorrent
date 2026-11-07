use crate::handshake::CryptoSet;

/// Why an MSE handshake failed.
#[derive(Debug, thiserror::Error)]
pub enum MseError {
    #[error("the peer's Diffie-Hellman public key is degenerate")]
    InvalidPublicKey,
    #[error("no synchronisation marker within the padding limit")]
    SyncNotFound,
    #[error("the peer asked for a torrent we don't have")]
    UnknownTorrent,
    #[error("bad verification constant (wrong key or not MSE)")]
    BadVerificationConstant,
    #[error("padding of {0} bytes is over the 512-byte limit")]
    PaddingTooLong(usize),
    #[error("no acceptable crypto method in the offer {:#x}", offered.bits())]
    NoCommonMethod { offered: CryptoSet },
    #[error("the peer selected crypto method {0:#x}, which we didn't offer")]
    InvalidSelection(u32),
    #[error("the initial payload of {0} bytes is over the 65535-byte limit")]
    InitialPayloadTooLong(usize),
    #[error("a plaintext handshake was refused because encryption is required")]
    PlaintextRefused,
    #[error("an encrypted handshake was refused because encryption is disabled")]
    EncryptionRefused,
    #[error("the handshake already failed")]
    AlreadyFailed,
    #[error("the handshake timed out")]
    Timeout,
    #[error("the connection closed during the handshake")]
    Closed,
    #[error("I/O error during the handshake: {0}")]
    Io(#[from] std::io::Error),
}

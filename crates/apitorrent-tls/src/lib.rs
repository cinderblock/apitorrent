//! Server certificates for apitorrent's built-in TLS.
//!
//! Everything here is about *which certificate the daemon presents*; the HTTP side lives in
//! `apitorrent-server`. A [`CertStore`] holds the current certificate and answers rustls's
//! SNI callback, so a new certificate takes effect for the next handshake with no restart
//! and no dropped connections. Certificates come from one of three places:
//!
//! - **Bring your own** ([`load_files`], [`spawn_file_watcher`]): a PEM chain and key on
//!   disk, polled for changes. A replacement that fails to load is refused and logged; the
//!   previous certificate keeps serving.
//! - **Self-signed** ([`ensure_self_signed`]): for LAN use with pinning. The key persists in
//!   the data directory so the pairing code stays the same across restarts and renewals.
//! - **ACME** ([`acme`]): RFC 8555 with HTTP-01, TLS-ALPN-01, or DNS-01 through a
//!   [`acme::DnsProvider`] plugin.
//!
//! # Crypto provider
//!
//! rustls runs on **ring**, passed explicitly everywhere (see [`provider`]). ring builds
//! with only a C compiler, including on Windows MSVC; aws-lc-rs would need `cmake` (and
//! `nasm`) there. Passing the provider explicitly also means nothing here depends on, or
//! changes, rustls's process-wide default.
//!
//! # Pairing
//!
//! A client that can't verify the certificate through a CA pins the SHA-256 of the
//! server's public key (its `SubjectPublicKeyInfo`) instead, after the user compares a
//! short pairing code out of band ([`CertInfo::pairing_code`]). Nothing in apitorrent ever
//! accepts an unverified certificate.

pub mod acme;
mod cert;
mod files;
mod fsutil;
mod self_signed;
mod store;

use std::path::PathBuf;
use std::sync::Arc;

use rustls::crypto::CryptoProvider;

pub use cert::{CertInfo, LoadedCert, load_pem, pairing_code, read_pem_certificates};
pub use files::{load_files, spawn_file_watcher};
pub use rustls::pki_types::CertificateDer;
pub use self_signed::{SELF_SIGNED_VALIDITY, ensure_self_signed, spawn_self_signed_renewer};
pub use store::{ACME_TLS_ALPN, ActiveCert, CertSource, CertStore};

/// The crypto provider for every TLS configuration apitorrent builds.
pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0}")]
    Pem(String),
    #[error("invalid certificate: {0}")]
    Certificate(String),
    #[error("the private key does not belong to the certificate")]
    KeyMismatch,
    #[error("TLS configuration: {0}")]
    Rustls(#[from] rustls::Error),
    #[error("certificate generation: {0}")]
    Generate(#[from] rcgen::Error),
}

impl TlsError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

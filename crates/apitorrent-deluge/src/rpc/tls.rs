//! TLS to Deluge's daemon, trusting exactly the certificate in its config directory.
//!
//! `deluged` serves a self-signed certificate it made on first start,
//! `<config_dir>/ssl/daemon.cert` (`deluge/crypto_utils.py`): RSA 2048, SHA-256, subject
//! `CN=Deluge Daemon`, valid three years and never renewed while the file exists. Deluge's
//! own client checks nothing. We pin it instead: the server is accepted only if the
//! certificate it presents is byte for byte the one in the file, and the handshake signature
//! is then verified against that certificate's key. There is no path that accepts a
//! certificate unverified.
//!
//! Two things about Deluge's certificate rule out rustls' usual verifier:
//!
//! - It is an X.509 **v1** certificate (pyOpenSSL never calls `set_version`), with serial
//!   number 0. webpki only parses v3, so `rustls::crypto::verify_tls1x_signature`, which
//!   parses the certificate with webpki, rejects it. We take the key from the pinned
//!   certificate with `x509-parser` and verify the handshake signature against that key as a
//!   raw public key.
//! - It expires after three years and Deluge keeps serving it. As with any pinned key, the
//!   dates and the host name are not checked: the pinned bytes are the identity.
//!
//! Deluge (Twisted, pyOpenSSL) offers TLS 1.2 and 1.3 with ECDHE AES-GCM and `ChaCha20`
//! (`TLS_CIPHERS` in `crypto_utils.py`); rustls' defaults cover both.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, SubjectPublicKeyInfoDer, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, Error, SignatureScheme};

/// The daemon's certificate, read from the config directory.
#[derive(Clone)]
pub struct PinnedCert {
    cert: CertificateDer<'static>,
    spki: SubjectPublicKeyInfoDer<'static>,
}

impl fmt::Debug for PinnedCert {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PinnedCert")
            .field("der_len", &self.cert.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CertError {
    #[error("reading {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("{0} holds no PEM certificate")]
    NoCertificate(std::path::PathBuf),
    #[error("the daemon's certificate can't be parsed: {0}")]
    Parse(String),
}

impl PinnedCert {
    /// `<config_dir>/ssl/daemon.cert`.
    pub fn from_config_dir(config_dir: &Path) -> Result<Self, CertError> {
        Self::from_pem_file(&config_dir.join("ssl").join("daemon.cert"))
    }

    /// The first certificate in a PEM file.
    pub fn from_pem_file(path: &Path) -> Result<Self, CertError> {
        let pem = std::fs::read(path).map_err(|source| CertError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let cert = CertificateDer::from_pem_slice(&pem)
            .map_err(|_| CertError::NoCertificate(path.to_path_buf()))?;
        Self::from_der(cert)
    }

    pub fn from_der(cert: CertificateDer<'static>) -> Result<Self, CertError> {
        let (rest, parsed) = x509_parser::parse_x509_certificate(&cert)
            .map_err(|e| CertError::Parse(e.to_string()))?;
        if !rest.is_empty() {
            return Err(CertError::Parse(
                "trailing bytes after the certificate".into(),
            ));
        }
        let spki = SubjectPublicKeyInfoDer::from(parsed.tbs_certificate.subject_pki.raw.to_vec());
        Ok(Self { cert, spki })
    }

    pub fn der(&self) -> &CertificateDer<'static> {
        &self.cert
    }
}

/// The rustls client config for one daemon.
pub fn client_config(pin: &PinnedCert) -> rustls::ClientConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PinnedCertVerifier {
        pin: pin.clone(),
        algorithms: provider.signature_verification_algorithms,
    });
    rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default versions")
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth()
}

/// The presented certificate wasn't the pinned one.
pub struct NotThePinnedCert;

impl fmt::Display for NotThePinnedCert {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "the daemon presented a certificate other than its config directory's \
             ssl/daemon.cert",
        )
    }
}

/// The same as `Display`: rustls shows its `Other` certificate errors with `Debug`.
impl fmt::Debug for NotThePinnedCert {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for NotThePinnedCert {}

/// Accepts the pinned certificate and nothing else.
struct PinnedCertVerifier {
    pin: PinnedCert,
    algorithms: WebPkiSupportedAlgorithms,
}

impl fmt::Debug for PinnedCertVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PinnedCertVerifier").finish_non_exhaustive()
    }
}

impl PinnedCertVerifier {
    /// Verify a handshake signature against the pinned key. For TLS 1.2 a scheme can map to
    /// several algorithms (as `rustls::crypto::verify_tls12_signature` does, try each).
    fn verify(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
        first_only: bool,
    ) -> Result<HandshakeSignatureValid, Error> {
        // verify_server_cert already refused any other certificate; this is belt and braces.
        if cert.as_ref() != self.pin.cert.as_ref() {
            return Err(not_pinned());
        }
        let key = webpki::RawPublicKeyEntity::try_from(&self.pin.spki)
            .map_err(|e| Error::General(format!("pinned key: {e}")))?;
        let algs = self
            .algorithms
            .mapping
            .iter()
            .find(|(scheme, _)| *scheme == dss.scheme)
            .map(|(_, algs)| *algs)
            .ok_or(Error::PeerMisbehaved(
                rustls::PeerMisbehaved::SignedHandshakeWithUnadvertisedSigScheme,
            ))?;
        let algs = if first_only { &algs[..1] } else { algs };
        let mut last = None;
        for alg in algs {
            match key.verify_signature(*alg, message, dss.signature()) {
                Ok(()) => return Ok(HandshakeSignatureValid::assertion()),
                Err(e) => last = Some(e),
            }
        }
        Err(Error::InvalidCertificate(match last {
            Some(webpki::Error::InvalidSignatureForPublicKey) => CertificateError::BadSignature,
            _ => CertificateError::UnsupportedSignatureAlgorithmForPublicKeyContext {
                signature_algorithm_id: Vec::new(),
                public_key_algorithm_id: Vec::new(),
            },
        }))
    }
}

/// TLS 1.3 drops PKCS#1 v1.5 and SHA-1 signatures (RFC 8446 4.2.3). rustls' own check is
/// private.
fn supported_in_tls13(scheme: SignatureScheme) -> bool {
    matches!(
        scheme,
        SignatureScheme::ECDSA_NISTP256_SHA256
            | SignatureScheme::ECDSA_NISTP384_SHA384
            | SignatureScheme::ECDSA_NISTP521_SHA512
            | SignatureScheme::RSA_PSS_SHA256
            | SignatureScheme::RSA_PSS_SHA384
            | SignatureScheme::RSA_PSS_SHA512
            | SignatureScheme::ED25519
            | SignatureScheme::ED448
    )
}

fn not_pinned() -> Error {
    Error::InvalidCertificate(CertificateError::Other(rustls::OtherError(Arc::new(
        NotThePinnedCert,
    ))))
}

impl ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        if end_entity.as_ref() == self.pin.cert.as_ref() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(not_pinned())
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.verify(message, cert, dss, false)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        if !supported_in_tls13(dss.scheme) {
            return Err(Error::PeerMisbehaved(
                rustls::PeerMisbehaved::SignedHandshakeWithUnadvertisedSigScheme,
            ));
        }
        self.verify(message, cert, dss, true)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests;

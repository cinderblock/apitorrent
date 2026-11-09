//! How a client decides to trust a server's certificate.
//!
//! There are exactly three ways, and none of them skips verification:
//!
//! 1. [`Trust::Os`]: the operating system's verifier (`rustls-platform-verifier`), so a
//!    public CA or a CA the user installed system-wide.
//! 2. [`Trust::PrivateCa`]: only the CA certificates imported into this profile, checked
//!    with webpki (chain, validity dates, host name).
//! 3. [`Trust::Pinned`]: the server's public key, pinned by its SPKI SHA-256 after the user
//!    compared it with the pairing code the daemon shows. The [`PinnedVerifier`] accepts the
//!    pinned key, or else a chain the OS verifier accepts, and nothing else.
//!
//! In every case the TLS handshake signature is verified against the certificate's key, so
//! a server must hold the private key of whatever it presents.

use std::fmt;
use std::sync::{Arc, Mutex};

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, OtherError, RootCertStore};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// A SHA-256 digest of a certificate's DER-encoded `SubjectPublicKeyInfo`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpkiHash(pub [u8; 32]);

impl SpkiHash {
    /// The digest of the `SubjectPublicKeyInfo` inside `cert`.
    pub fn of_certificate(cert: &[u8]) -> Result<Self, TrustError> {
        let (rest, parsed) = x509_parser::parse_x509_certificate(cert)
            .map_err(|e| TrustError::BadCertificate(e.to_string()))?;
        if !rest.is_empty() {
            return Err(TrustError::BadCertificate(
                "trailing bytes after the certificate".into(),
            ));
        }
        Ok(Self::of_spki(parsed.tbs_certificate.subject_pki.raw))
    }

    /// The digest of a DER-encoded `SubjectPublicKeyInfo`.
    pub fn of_spki(spki: &[u8]) -> Self {
        Self(Sha256::digest(spki).into())
    }

    /// Lowercase hex, the canonical stored form.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Parse a pairing code or a stored pin.
    ///
    /// Accepts 64 hex digits (any case, with spaces, colons or dashes between groups), or
    /// the 32 bytes in base64 or base64url, padded or not. An optional `sha256/` or
    /// `sha256:` prefix (as in HPKP and `curl --pinnedpubkey`) is ignored.
    pub fn parse(code: &str) -> Result<Self, TrustError> {
        let code = code.trim();
        let code = ["sha256/", "sha256:", "SHA256/", "SHA256:"]
            .iter()
            .find_map(|p| code.strip_prefix(p))
            .unwrap_or(code)
            .trim();
        let compact: String = code
            .chars()
            .filter(|c| !c.is_whitespace() && *c != ':' && *c != '-')
            .collect();
        if compact.len() == 64 && compact.chars().all(|c| c.is_ascii_hexdigit()) {
            let mut out = [0; 32];
            hex::decode_to_slice(&compact, &mut out).expect("checked to be hex");
            return Ok(Self(out));
        }
        // Dashes are separators for hex, but a base64url alphabet character, so base64 is
        // tried on the code as given (minus whitespace).
        let b64: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        let trimmed = b64.trim_end_matches('=');
        for encoding in [
            &data_encoding::BASE64_NOPAD,
            &data_encoding::BASE64URL_NOPAD,
        ] {
            if let Ok(bytes) = encoding.decode(trimmed.as_bytes())
                && let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice())
            {
                return Ok(Self(arr));
            }
        }
        Err(TrustError::BadPin)
    }
}

impl fmt::Debug for SpkiHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SpkiHash({})", self.to_hex())
    }
}

impl fmt::Display for SpkiHash {
    /// Hex in groups of four, the way a user compares it by eye.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hex = self.to_hex();
        for (i, chunk) in hex.as_bytes().chunks(4).enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            f.write_str(std::str::from_utf8(chunk).expect("hex is ASCII"))?;
        }
        Ok(())
    }
}

impl Serialize for SpkiHash {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for SpkiHash {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// How a profile trusts its server. Serialised into the profile file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Trust {
    /// The operating system's verifier and trust store.
    Os,
    /// Only these CA certificates (PEM, one or more).
    #[serde(rename_all = "camelCase")]
    PrivateCa { ca_pem: String },
    /// The server's key, compared with its pairing code out of band.
    #[serde(rename_all = "camelCase")]
    Pinned { spki_sha256: SpkiHash },
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum TrustError {
    #[error(
        "not a pairing code: expected the SHA-256 of the server's public key, as 64 hex digits \
         or base64"
    )]
    BadPin,
    #[error("the CA file contains no certificates")]
    NoCaCertificates,
    #[error("a CA certificate could not be read: {0}")]
    BadCa(String),
    #[error("not a valid certificate: {0}")]
    BadCertificate(String),
    #[error("TLS setup failed: {0}")]
    Tls(String),
}

/// The ring provider, the one this workspace builds with (no `CMake` or `NASM` on Windows). It
/// is passed explicitly, so nothing depends on a process-wide default provider.
pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The verifier for a trust setting.
pub fn verifier(trust: &Trust) -> Result<Arc<dyn ServerCertVerifier>, TrustError> {
    let provider = provider();
    Ok(match trust {
        Trust::Os => os_verifier(provider)?,
        Trust::PrivateCa { ca_pem } => {
            let mut roots = RootCertStore::empty();
            for cert in CertificateDer::pem_slice_iter(ca_pem.as_bytes()) {
                let cert = cert.map_err(|e| TrustError::BadCa(e.to_string()))?;
                roots
                    .add(cert)
                    .map_err(|e| TrustError::BadCa(e.to_string()))?;
            }
            if roots.is_empty() {
                return Err(TrustError::NoCaCertificates);
            }
            WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider)
                .build()
                .map_err(|e| TrustError::Tls(e.to_string()))?
        }
        Trust::Pinned { spki_sha256 } => Arc::new(PinnedVerifier::new(
            vec![*spki_sha256],
            Some(os_verifier(provider.clone())?),
            &provider,
        )),
    })
}

fn os_verifier(provider: Arc<CryptoProvider>) -> Result<Arc<dyn ServerCertVerifier>, TrustError> {
    Ok(Arc::new(
        rustls_platform_verifier::Verifier::new(provider)
            .map_err(|e| TrustError::Tls(e.to_string()))?,
    ))
}

/// A rustls client configuration for a trust setting: TLS 1.2 and 1.3, HTTP/1.1 (the API and
/// its WebSocket both use it), no client certificate.
pub fn client_config(trust: &Trust) -> Result<Arc<ClientConfig>, TrustError> {
    config_with(verifier(trust)?)
}

pub(crate) fn config_with(
    verifier: Arc<dyn ServerCertVerifier>,
) -> Result<Arc<ClientConfig>, TrustError> {
    let mut config = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| TrustError::Tls(e.to_string()))?
        // `dangerous()` is only rustls's name for "a verifier of your own"; every verifier
        // here verifies.
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// The server presented a key that isn't pinned, and its chain didn't verify either.
///
/// Carried inside the rustls error, so a caller can show the user what the server
/// actually presented (to compare with the pairing code) without trusting it.
#[derive(Debug, Clone)]
pub struct PinMismatch {
    pub presented: SpkiHash,
    /// Why the fallback chain verification failed, when there is one.
    pub chain_error: Option<String>,
}

impl fmt::Display for PinMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the server's key ({}) is not the pinned key",
            self.presented
        )?;
        if let Some(e) = &self.chain_error {
            write!(f, ", and its certificate is not trusted either: {e}")?;
        }
        Ok(())
    }
}

impl std::error::Error for PinMismatch {}

/// Accepts a server whose end-entity certificate carries one of the pinned keys, or else
/// whose chain the fallback verifier accepts. Nothing else gets through.
///
/// A pin identifies the server by its key, so for a pinned key the host name and the
/// certificate's dates are not checked: a self-signed certificate that has expired still
/// proves possession of the pinned key, because the handshake signature is verified
/// against it. Rotating the key needs a new pairing (or a certificate that verifies).
pub struct PinnedVerifier {
    pins: Vec<SpkiHash>,
    fallback: Option<Arc<dyn ServerCertVerifier>>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl fmt::Debug for PinnedVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PinnedVerifier")
            .field("pins", &self.pins)
            .field("fallback", &self.fallback.is_some())
            .finish_non_exhaustive()
    }
}

impl PinnedVerifier {
    pub fn new(
        pins: Vec<SpkiHash>,
        fallback: Option<Arc<dyn ServerCertVerifier>>,
        provider: &CryptoProvider,
    ) -> Self {
        Self {
            pins,
            fallback,
            algorithms: provider.signature_verification_algorithms,
        }
    }
}

fn mismatch(m: PinMismatch) -> rustls::Error {
    rustls::Error::InvalidCertificate(CertificateError::Other(OtherError(Arc::new(m))))
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let presented = SpkiHash::of_certificate(end_entity)
            .map_err(|_| rustls::Error::InvalidCertificate(CertificateError::BadEncoding))?;
        if self.pins.contains(&presented) {
            return Ok(ServerCertVerified::assertion());
        }
        let chain_error = match &self.fallback {
            Some(f) => {
                match f.verify_server_cert(
                    end_entity,
                    intermediates,
                    server_name,
                    ocsp_response,
                    now,
                ) {
                    Ok(v) => return Ok(v),
                    Err(e) => Some(e.to_string()),
                }
            }
            None => None,
        };
        Err(mismatch(PinMismatch {
            presented,
            chain_error,
        }))
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// Records the key the server presents, then defers to another verifier. Used to show the
/// user a server's key during pairing; it never makes a connection more trusted.
pub(crate) struct Recorder {
    pub(crate) inner: Arc<dyn ServerCertVerifier>,
    pub(crate) seen: Mutex<Option<SpkiHash>>,
}

impl fmt::Debug for Recorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Recorder").finish_non_exhaustive()
    }
}

impl ServerCertVerifier for Recorder {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if let Ok(h) = SpkiHash::of_certificate(end_entity) {
            *self.seen.lock().expect("not poisoned") = Some(h);
        }
        self.inner
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Find a [`PinMismatch`] inside an error chain (a rustls error wrapped in I/O, reqwest or
/// WebSocket errors).
pub fn find_pin_mismatch(err: &(dyn std::error::Error + 'static)) -> Option<PinMismatch> {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        if let Some(m) = e.downcast_ref::<PinMismatch>() {
            return Some(m.clone());
        }
        if let Some(rustls::Error::InvalidCertificate(CertificateError::Other(OtherError(inner)))) =
            e.downcast_ref::<rustls::Error>()
            && let Some(m) = inner.downcast_ref::<PinMismatch>()
        {
            return Some(m.clone());
        }
        if let Some(io) = e.downcast_ref::<std::io::Error>()
            && let Some(inner) = io.get_ref()
            && let Some(m) = find_pin_mismatch(inner)
        {
            return Some(m);
        }
        cur = e.source();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::PublicKeyData as _;

    #[test]
    fn spki_hash_matches_the_key() {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let expected = SpkiHash::of_spki(&key.subject_public_key_info());
        assert_eq!(SpkiHash::of_certificate(cert.der()).unwrap(), expected);
        // A second certificate for the same key has the same pin.
        let again = rcgen::CertificateParams::new(vec!["other.example".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        assert_eq!(SpkiHash::of_certificate(again.der()).unwrap(), expected);
    }

    #[test]
    fn garbage_is_not_a_certificate() {
        assert!(SpkiHash::of_certificate(b"not der").is_err());
    }

    #[test]
    fn pairing_codes_in_every_form() {
        let bytes: [u8; 32] = std::array::from_fn(|i| u8::try_from(i * 7 + 3).unwrap());
        let want = SpkiHash(bytes);
        let hex = hex::encode(bytes);
        let grouped = want.to_string();
        let colons = bytes
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":");
        let b64 = data_encoding::BASE64.encode(&bytes);
        let b64url = data_encoding::BASE64URL_NOPAD.encode(&bytes);
        for code in [
            hex.clone(),
            hex.to_uppercase(),
            grouped.clone(),
            colons,
            format!("  {hex}\n"),
            format!("sha256/{b64}"),
            b64.clone(),
            b64.trim_end_matches('=').to_owned(),
            b64url,
            format!("SHA256:{hex}"),
        ] {
            assert_eq!(SpkiHash::parse(&code).unwrap(), want, "{code}");
        }
        assert_eq!(grouped.split(' ').count(), 16);
        for bad in [
            "",
            "abc",
            &hex[..62],
            &format!("{hex}00"),
            "zz".repeat(32).as_str(),
        ] {
            assert!(SpkiHash::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn trust_round_trips_through_json() {
        let pin = SpkiHash([0xab; 32]);
        for t in [
            Trust::Os,
            Trust::PrivateCa {
                ca_pem: "-----BEGIN CERTIFICATE-----\n…".into(),
            },
            Trust::Pinned { spki_sha256: pin },
        ] {
            let json = serde_json::to_string(&t).unwrap();
            assert_eq!(serde_json::from_str::<Trust>(&json).unwrap(), t);
        }
        assert_eq!(
            serde_json::to_value(Trust::Pinned { spki_sha256: pin }).unwrap(),
            serde_json::json!({"kind": "pinned", "spkiSha256": "ab".repeat(32)})
        );
    }

    #[test]
    fn a_private_ca_needs_a_certificate() {
        assert!(matches!(
            verifier(&Trust::PrivateCa {
                ca_pem: "nothing here".into()
            }),
            Err(TrustError::NoCaCertificates)
        ));
    }

    fn verify(v: &dyn ServerCertVerifier, cert: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        v.verify_server_cert(
            cert,
            &[],
            &ServerName::try_from("localhost").unwrap(),
            &[],
            UnixTime::now(),
        )
        .map(|_| ())
    }

    #[test]
    fn pinned_verifier_accepts_only_the_pin() {
        let provider = provider();
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let pin = SpkiHash::of_certificate(cert.der()).unwrap();

        let v = PinnedVerifier::new(vec![pin], None, &provider);
        verify(&v, cert.der()).unwrap();

        let other_key = rcgen::KeyPair::generate().unwrap();
        let other = rcgen::CertificateParams::new(vec!["localhost".into()])
            .unwrap()
            .self_signed(&other_key)
            .unwrap();
        let err = verify(&v, other.der()).unwrap_err();
        let m = find_pin_mismatch(&err).expect("a pin mismatch");
        assert_eq!(m.presented, SpkiHash::of_certificate(other.der()).unwrap());
        assert!(m.chain_error.is_none());

        // The fallback (here: the OS) doesn't know a self-signed certificate either.
        let v = PinnedVerifier::new(
            vec![pin],
            Some(os_verifier(provider.clone()).unwrap()),
            &provider,
        );
        let err = verify(&v, other.der()).unwrap_err();
        let m = find_pin_mismatch(&err).expect("a pin mismatch");
        assert!(m.chain_error.is_some());
        verify(&v, cert.der()).unwrap();
    }

    #[test]
    fn os_trust_rejects_self_signed() {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let v = verifier(&Trust::Os).unwrap();
        assert!(verify(v.as_ref(), cert.der()).is_err());
    }
}

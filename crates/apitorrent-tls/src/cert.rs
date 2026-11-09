//! Loading PEM certificates and keys, and what we need to know about a certificate.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::sign::CertifiedKey;
use sha2::{Digest as _, Sha256};
use x509_parser::extensions::GeneralName;
use x509_parser::prelude::{FromDer as _, X509Certificate};

use crate::TlsError;

/// Facts about a leaf certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertInfo {
    /// SHA-256 of the DER `SubjectPublicKeyInfo`: what a pairing client pins.
    pub spki_sha256: [u8; 32],
    pub not_before: SystemTime,
    pub not_after: SystemTime,
    /// DNS names from the subject alternative names (wildcards included as `*.example.com`).
    pub dns_names: Vec<String>,
    pub ip_addresses: Vec<IpAddr>,
}

impl CertInfo {
    /// Parse the leaf certificate.
    pub fn from_der(der: &[u8]) -> Result<Self, TlsError> {
        let (rest, cert) =
            X509Certificate::from_der(der).map_err(|e| TlsError::Certificate(e.to_string()))?;
        if !rest.is_empty() {
            return Err(TlsError::Certificate(
                "trailing data after the certificate".into(),
            ));
        }
        let spki_sha256 = Sha256::digest(cert.public_key().raw).into();
        let validity = cert.validity();
        let mut dns_names = Vec::new();
        let mut ip_addresses = Vec::new();
        if let Some(san) = cert
            .subject_alternative_name()
            .map_err(|e| TlsError::Certificate(e.to_string()))?
        {
            for name in &san.value.general_names {
                match name {
                    GeneralName::DNSName(d) => dns_names.push(d.to_ascii_lowercase()),
                    GeneralName::IPAddress(b) => {
                        if let Ok(a) = <[u8; 4]>::try_from(*b) {
                            ip_addresses.push(IpAddr::from(a));
                        } else if let Ok(a) = <[u8; 16]>::try_from(*b) {
                            ip_addresses.push(IpAddr::from(a));
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(Self {
            spki_sha256,
            not_before: unix_time(validity.not_before.timestamp()),
            not_after: unix_time(validity.not_after.timestamp()),
            dns_names,
            ip_addresses,
        })
    }

    /// The SPKI hash as lowercase hex (64 characters).
    pub fn spki_sha256_hex(&self) -> String {
        hex::encode(self.spki_sha256)
    }

    /// The short pairing code for this certificate's key (see [`pairing_code`]).
    pub fn pairing_code(&self) -> String {
        pairing_code(&self.spki_sha256)
    }

    /// Whether the certificate is within its validity period at `now`.
    pub fn is_valid_at(&self, now: SystemTime) -> bool {
        self.not_before <= now && now < self.not_after
    }

    /// Whether the certificate names every one of `names` exactly (no wildcard matching:
    /// ACME orders ask for exactly the configured names).
    pub fn names_cover(&self, names: &[String]) -> bool {
        names
            .iter()
            .all(|n| self.dns_names.iter().any(|d| d.eq_ignore_ascii_case(n)))
    }
}

fn unix_time(secs: i64) -> SystemTime {
    match u64::try_from(secs) {
        Ok(s) => UNIX_EPOCH + Duration::from_secs(s),
        Err(_) => UNIX_EPOCH,
    }
}

/// The short code a person compares when pairing a client with this server: the first 100
/// bits of the SPKI SHA-256, as 20 characters of RFC 4648 base32 in groups of four
/// (`ABCD-EFGH-IJKL-MNOP-QRST`).
///
/// 100 bits means an attacker would need about 2^100 key generations to make a key whose
/// code matches. Compare case-insensitively, ignoring `-` and spaces; the alphabet has no
/// `0`, `1` or `8`, so those can be read as `O`, `I` and `B`.
pub fn pairing_code(spki_sha256: &[u8; 32]) -> String {
    let b32 = data_encoding::BASE32_NOPAD.encode(spki_sha256);
    b32.as_bytes()[..20]
        .chunks(4)
        .map(|c| std::str::from_utf8(c).expect("base32 is ASCII"))
        .collect::<Vec<_>>()
        .join("-")
}

/// Every `CERTIFICATE` block in a PEM file (at least one).
pub fn read_pem_certificates(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let certs = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| TlsError::Pem(e.to_string()))?;
    if certs.is_empty() {
        return Err(TlsError::Pem("no CERTIFICATE blocks".into()));
    }
    Ok(certs)
}

/// A certificate chain and its key, ready to serve.
#[derive(Debug, Clone)]
pub struct LoadedCert {
    pub key: Arc<CertifiedKey>,
    pub info: CertInfo,
}

/// Load a PEM certificate chain (leaf first) and a PEM private key (PKCS#8, PKCS#1 or
/// SEC1), checking that the key belongs to the leaf.
pub fn load_pem(chain_pem: &[u8], key_pem: &[u8]) -> Result<LoadedCert, TlsError> {
    let chain = CertificateDer::pem_slice_iter(chain_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| TlsError::Pem(format!("certificate chain: {e}")))?;
    let Some(leaf) = chain.first() else {
        return Err(TlsError::Pem(
            "no CERTIFICATE block in the certificate file".into(),
        ));
    };
    let info = CertInfo::from_der(leaf)?;
    let key = PrivateKeyDer::from_pem_slice(key_pem)
        .map_err(|e| TlsError::Pem(format!("private key: {e}")))?;
    let signing = crate::provider()
        .key_provider
        .load_private_key(key)
        .map_err(|e| TlsError::Pem(format!("private key: {e}")))?;
    let certified = CertifiedKey::new(chain, signing);
    match certified.keys_match() {
        Ok(()) => {}
        Err(rustls::Error::InconsistentKeys(rustls::InconsistentKeys::KeyMismatch)) => {
            return Err(TlsError::KeyMismatch);
        }
        // ring knows the public key of every key type it loads, so "unknown" can't happen;
        // if it ever does, refuse rather than serve a key we couldn't check.
        Err(e) => return Err(TlsError::Rustls(e)),
    }
    Ok(LoadedCert {
        key: Arc::new(certified),
        info,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_code_shape() {
        let code = pairing_code(&[0u8; 32]);
        assert_eq!(code, "AAAA-AAAA-AAAA-AAAA-AAAA");
        let code = pairing_code(&[0xff; 32]);
        assert_eq!(code, "7777-7777-7777-7777-7777");
        let mut h = [0u8; 32];
        h[0] = 0b0000_1000; // the second base32 digit's top bit
        assert_eq!(pairing_code(&h), "BAAA-AAAA-AAAA-AAAA-AAAA");
    }

    fn generate(names: &[&str]) -> (String, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let params = rcgen::CertificateParams::new(
            names.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
        )
        .unwrap();
        let cert = params.self_signed(&key).unwrap();
        (cert.pem(), key.serialize_pem())
    }

    #[test]
    fn loads_and_inspects() {
        let (cert, key) = generate(&["Example.com", "127.0.0.1", "::1"]);
        let loaded = load_pem(cert.as_bytes(), key.as_bytes()).unwrap();
        assert_eq!(loaded.info.dns_names, ["example.com"]);
        assert_eq!(
            loaded.info.ip_addresses,
            [
                "127.0.0.1".parse::<IpAddr>().unwrap(),
                "::1".parse().unwrap()
            ]
        );
        assert!(loaded.info.names_cover(&["example.com".into()]));
        assert!(!loaded.info.names_cover(&["www.example.com".into()]));
        assert_eq!(loaded.info.spki_sha256_hex().len(), 64);
        // The hash really is over the SPKI: rcgen's own encoding of the public key.
        let spki =
            rcgen::PublicKeyData::subject_public_key_info(&rcgen::KeyPair::from_pem(&key).unwrap());
        assert_eq!(
            loaded.info.spki_sha256,
            <[u8; 32]>::from(Sha256::digest(spki))
        );
    }

    #[test]
    fn refuses_mismatched_and_garbage() {
        let (cert, _) = generate(&["a.example"]);
        let (_, other_key) = generate(&["b.example"]);
        assert!(matches!(
            load_pem(cert.as_bytes(), other_key.as_bytes()),
            Err(TlsError::KeyMismatch)
        ));
        assert!(matches!(
            load_pem(b"not a pem", other_key.as_bytes()),
            Err(TlsError::Pem(_))
        ));
        let (cert, key) = generate(&["a.example"]);
        assert!(load_pem(cert.as_bytes(), b"").is_err());
        // A truncated certificate body fails to parse.
        let truncated = cert.replace(&cert[60..90], "");
        assert!(load_pem(truncated.as_bytes(), key.as_bytes()).is_err());
    }
}

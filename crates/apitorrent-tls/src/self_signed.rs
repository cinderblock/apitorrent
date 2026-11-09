//! A self-signed certificate for LAN use, trusted by clients through pairing.
//!
//! The key is generated once and kept (`key.pem`, owner-only), so the pairing code, which
//! hashes the public key, never changes on its own. The certificate around it is reissued
//! with the same key when it gets within [`RENEW_BEFORE`] of expiry or when the configured
//! names change. Browsers will show a certificate warning for it; it is meant for the
//! desktop client and CLI, which pin the key.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio::task::JoinHandle;

use crate::TlsError;
use crate::cert::{LoadedCert, load_pem};
use crate::fsutil::{read_optional, write_atomic};
use crate::store::{CertSource, CertStore};

/// How long a self-signed certificate is valid.
pub const SELF_SIGNED_VALIDITY: Duration = Duration::from_secs(365 * 24 * 3600);
/// Reissue when less than this is left.
const RENEW_BEFORE: Duration = Duration::from_secs(30 * 24 * 3600);

/// Load the self-signed certificate in `dir`, creating or reissuing it as needed so that
/// it names exactly `names` (host names and IP addresses) and has at least 30 days left.
pub fn ensure_self_signed(
    dir: &Path,
    names: &[String],
    now: SystemTime,
) -> Result<LoadedCert, TlsError> {
    let key_path = dir.join("key.pem");
    let cert_path = dir.join("cert.pem");
    let key_pem = if let Some(k) = read_optional(&key_path)? {
        String::from_utf8(k)
            .map_err(|_| TlsError::Pem(format!("{}: not PEM", key_path.display())))?
    } else {
        let key = rcgen::KeyPair::generate()?;
        let pem = key.serialize_pem();
        write_atomic(&key_path, pem.as_bytes(), true)?;
        tracing::info!(path = %key_path.display(), "generated a key for the self-signed certificate");
        pem
    };
    let wanted: BTreeSet<String> = names.iter().map(|n| normalize(n)).collect();
    if let Some(cert_pem) = read_optional(&cert_path)? {
        match load_pem(&cert_pem, key_pem.as_bytes()) {
            Ok(loaded) => {
                let have: BTreeSet<String> = loaded
                    .info
                    .dns_names
                    .iter()
                    .cloned()
                    .chain(loaded.info.ip_addresses.iter().map(IpAddr::to_string))
                    .collect();
                let fresh = loaded
                    .info
                    .not_after
                    .duration_since(now)
                    .is_ok_and(|left| left > RENEW_BEFORE)
                    && loaded.info.not_before <= now;
                if fresh && have == wanted {
                    return Ok(loaded);
                }
            }
            Err(e) => {
                tracing::warn!(path = %cert_path.display(), error = %e, "reissuing the self-signed certificate");
            }
        }
    }
    let key = rcgen::KeyPair::from_pem(&key_pem)?;
    let mut params = rcgen::CertificateParams::new(wanted.into_iter().collect::<Vec<_>>())?;
    let mut dn = rcgen::DistinguishedName::new();
    dn.push(rcgen::DnType::CommonName, "apitorrent (self-signed)");
    params.distinguished_name = dn;
    let start = now - Duration::from_secs(3600);
    params.not_before = start.into();
    params.not_after = (start + SELF_SIGNED_VALIDITY).into();
    params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let cert = params.self_signed(&key)?;
    write_atomic(&cert_path, cert.pem().as_bytes(), false)?;
    let loaded = load_pem(cert.pem().as_bytes(), key_pem.as_bytes())?;
    tracing::info!(path = %cert_path.display(), names = ?names, "issued a self-signed certificate");
    Ok(loaded)
}

fn normalize(name: &str) -> String {
    let n = name.trim().trim_start_matches('[').trim_end_matches(']');
    n.parse::<IpAddr>()
        .map_or_else(|_| n.to_ascii_lowercase(), |ip| ip.to_string())
}

/// Check the self-signed certificate daily and install a reissued one.
pub fn spawn_self_signed_renewer(
    store: CertStore,
    dir: PathBuf,
    names: Vec<String>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(24 * 3600));
        tick.tick().await;
        loop {
            tick.tick().await;
            let current = store.current().map(|c| c.info.spki_sha256);
            let not_after = store.current().map(|c| c.info.not_after);
            match ensure_self_signed(&dir, &names, SystemTime::now()) {
                Ok(loaded) => {
                    if Some(loaded.info.not_after) != not_after {
                        if current.is_some_and(|c| c != loaded.info.spki_sha256) {
                            tracing::warn!(
                                "the self-signed key changed; paired clients must pair again"
                            );
                        }
                        store.install(loaded, CertSource::SelfSigned);
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "could not renew the self-signed certificate");
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_persists_across_reissues() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let names = vec!["localhost".to_owned(), "127.0.0.1".to_owned()];
        let a = ensure_self_signed(dir.path(), &names, now).unwrap();
        assert_eq!(a.info.dns_names, ["localhost"]);
        // Same inputs: the stored certificate is reused as is.
        let b = ensure_self_signed(dir.path(), &names, now).unwrap();
        assert_eq!(a.info, b.info);
        // New names: reissued, same key.
        let names2 = vec![
            "localhost".to_owned(),
            "nas.lan".to_owned(),
            "::1".to_owned(),
        ];
        let c = ensure_self_signed(dir.path(), &names2, now).unwrap();
        assert_eq!(c.info.spki_sha256, a.info.spki_sha256);
        assert_eq!(c.info.dns_names, ["localhost", "nas.lan"]);
        // Near expiry: reissued, same key.
        let later = now + SELF_SIGNED_VALIDITY - Duration::from_secs(10 * 24 * 3600);
        let d = ensure_self_signed(dir.path(), &names2, later).unwrap();
        assert_eq!(d.info.spki_sha256, a.info.spki_sha256);
        assert!(d.info.not_after > c.info.not_after);
    }
}

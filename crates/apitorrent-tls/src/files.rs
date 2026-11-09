//! Bring-your-own certificate files, reloaded when they change.
//!
//! The files are polled rather than watched with OS notifications: polling works the same
//! on every platform and filesystem (network shares included), and it sees every way a
//! certificate gets replaced, whether rewritten in place, renamed over, or a symlink
//! repointed (certbot's `live/` directory). The files are a few kilobytes, so reading them
//! every few seconds costs nothing.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sha2::{Digest as _, Sha256};
use tokio::task::JoinHandle;

use crate::TlsError;
use crate::cert::{LoadedCert, load_pem};
use crate::store::{CertSource, CertStore};

/// Load a PEM certificate chain and key from disk.
pub fn load_files(cert: &Path, key: &Path) -> Result<LoadedCert, TlsError> {
    let chain = std::fs::read(cert).map_err(|e| TlsError::io(cert, e))?;
    let key_pem = std::fs::read(key).map_err(|e| TlsError::io(key, e))?;
    load_pem(&chain, &key_pem)
}

/// How long the files must stay unchanged before a change is acted on, so a certificate
/// and key written one after the other aren't read halfway.
const SETTLE: Duration = Duration::from_secs(1);

/// Poll `cert` and `key` every `every` and install them into `store` when their contents
/// change. A replacement that fails to load (bad PEM, wrong key, expired) is refused with
/// an error in the log, once per distinct content, and the current certificate keeps
/// serving.
pub fn spawn_file_watcher(
    store: CertStore,
    cert: PathBuf,
    key: PathBuf,
    every: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Start from what was loaded at startup, if the files still hold it.
        let mut seen = read_both(&cert, &key).await.ok().map(|(_, _, h)| h);
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut read_error_logged = false;
        loop {
            tick.tick().await;
            let (chain, key_pem, hash) = match read_both(&cert, &key).await {
                Ok(v) => {
                    read_error_logged = false;
                    v
                }
                Err(e) => {
                    if !read_error_logged {
                        tracing::error!(
                            error = %e,
                            "TLS CERTIFICATE FILES UNREADABLE; still serving the previous \
                             certificate"
                        );
                        read_error_logged = true;
                    }
                    continue;
                }
            };
            if seen == Some(hash) {
                continue;
            }
            // Wait for the writer to finish, then act only if nothing moved meanwhile.
            tokio::time::sleep(SETTLE).await;
            let Ok((chain2, key2, hash2)) = read_both(&cert, &key).await else {
                continue;
            };
            if hash2 != hash {
                continue;
            }
            seen = Some(hash);
            drop((chain, key_pem));
            match load_replacement(&chain2, &key2, SystemTime::now()) {
                Ok(loaded) => {
                    tracing::info!(
                        cert = %cert.display(),
                        names = ?loaded.info.dns_names,
                        expires = %humantime_utc(loaded.info.not_after),
                        pairing_code = %loaded.info.pairing_code(),
                        "reloaded the TLS certificate"
                    );
                    store.install(loaded, CertSource::Provided);
                }
                Err(e) => {
                    tracing::error!(
                        cert = %cert.display(),
                        key = %key.display(),
                        error = %e,
                        "THE NEW TLS CERTIFICATE WAS REFUSED; still serving the previous one. \
                         Fix the files and they will be picked up automatically."
                    );
                }
            }
        }
    })
}

/// A replacement must load, and must not already be expired or not yet valid.
fn load_replacement(chain: &[u8], key: &[u8], now: SystemTime) -> Result<LoadedCert, TlsError> {
    let loaded = load_pem(chain, key)?;
    if !loaded.info.is_valid_at(now) {
        return Err(TlsError::Certificate(format!(
            "not valid now (valid from {} until {})",
            humantime_utc(loaded.info.not_before),
            humantime_utc(loaded.info.not_after)
        )));
    }
    Ok(loaded)
}

async fn read_both(cert: &Path, key: &Path) -> Result<(Vec<u8>, Vec<u8>, [u8; 32]), TlsError> {
    let c = tokio::fs::read(cert)
        .await
        .map_err(|e| TlsError::io(cert, e))?;
    let k = tokio::fs::read(key)
        .await
        .map_err(|e| TlsError::io(key, e))?;
    let mut h = Sha256::new();
    h.update((c.len() as u64).to_le_bytes());
    h.update(&c);
    h.update(&k);
    Ok((c, k, h.finalize().into()))
}

/// RFC 3339 UTC, for logs (log timestamps are UTC too).
pub(crate) fn humantime_utc(t: SystemTime) -> String {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let secs = i64::try_from(secs).unwrap_or(i64::MAX);
    time::OffsetDateTime::from_unix_timestamp(secs)
        .ok()
        .and_then(|d| {
            d.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| secs.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_expired_replacement() {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["a.example".to_owned()]).unwrap();
        params.not_before = rcgen::date_time_ymd(2000, 1, 1);
        params.not_after = rcgen::date_time_ymd(2001, 1, 1);
        let cert = params.self_signed(&key).unwrap();
        let r = load_replacement(
            cert.pem().as_bytes(),
            key.serialize_pem().as_bytes(),
            SystemTime::now(),
        );
        assert!(matches!(r, Err(TlsError::Certificate(m)) if m.contains("not valid now")));
        assert_eq!(
            humantime_utc(SystemTime::UNIX_EPOCH + Duration::from_secs(86_400)),
            "1970-01-02T00:00:00Z"
        );
    }
}

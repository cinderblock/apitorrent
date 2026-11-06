//! TLS for HTTPS trackers.
//!
//! Certificates are verified by the operating system, through `rustls-platform-verifier`:
//! `CertGetCertificateChain` on Windows, `Security.framework` on macOS, and the system trust
//! store (with webpki) on Linux. That includes the OS's revocation checks and any
//! enterprise roots the user has installed.
//!
//! **There is no way to turn verification off.** This module builds the only TLS
//! configuration the crate uses, [`crate::TrackerConfig`] has no TLS settings, and the
//! `reqwest` client is never exposed, so nothing outside the crate can swap in a
//! permissive verifier or call `danger_accept_invalid_certs`. A tracker with a self-signed
//! or expired certificate fails with [`crate::TrackerError::Network`]; the integration test
//! `https_rejects_self_signed_certificate` checks this.

use std::sync::Arc;

use rustls::ClientConfig;
use rustls_platform_verifier::BuilderVerifierExt as _;

use crate::error::TrackerError;

/// The rustls configuration for every HTTPS tracker request.
///
/// The crypto provider is passed in explicitly (ring), so this neither depends on nor
/// changes rustls's process-wide default provider.
pub(crate) fn client_config() -> Result<ClientConfig, TrackerError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(TrackerError::network)?
        .with_platform_verifier()
        .map_err(TrackerError::network)?
        .with_no_client_auth();
    // reqwest is built without HTTP/2; announce and scrape are single small GETs.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_with_platform_verifier() {
        let config = client_config().expect("platform verifier is available");
        // SNI is needed for the server to pick the certificate the verifier then checks
        // against the hostname.
        assert!(config.enable_sni);
        assert_eq!(config.alpn_protocols, [b"http/1.1".to_vec()]);
    }
}

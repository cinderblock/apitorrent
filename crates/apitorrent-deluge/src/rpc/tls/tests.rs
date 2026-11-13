//! The fixtures under `tests/fixtures/ssl` were made by Deluge 2.1.1's own
//! `deluge.crypto_utils.generate_ssl_keys()` (pyOpenSSL 24.2): X.509 v1, serial 0, RSA 2048.
//! They are test keys only.

use std::sync::Arc;

use rustls::pki_types::PrivateKeyDer;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/ssl")
        .join(name)
}

/// Serves one certified key whatever the client asks. rustls' `with_single_cert` checks the
/// key against the certificate with webpki, which can't parse Deluge's v1 certificate.
#[derive(Debug)]
struct Fixed(Arc<CertifiedKey>);

impl ResolvesServerCert for Fixed {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
}

fn server_config(
    which: &str,
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> rustls::ServerConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cert = CertificateDer::from_pem_file(fixture(&format!("{which}-daemon.cert"))).unwrap();
    let key = PrivateKeyDer::from_pem_file(fixture(&format!("{which}-daemon.pkey"))).unwrap();
    let key = provider.key_provider.load_private_key(key).unwrap();
    let certified = Arc::new(CertifiedKey::new(vec![cert], key));
    rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)
        .unwrap()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(Fixed(certified)))
}

/// Serve `which` and connect pinning `pinned`; true if the handshake and an echo succeed.
async fn handshake(
    which: &str,
    pinned: &str,
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> Result<(), std::io::Error> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config(which, versions)));
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut tls = acceptor.accept(tcp).await?;
        let mut b = [0u8; 4];
        tls.read_exact(&mut b).await?;
        tls.write_all(&b).await?;
        tls.shutdown().await
    });
    let pin = PinnedCert::from_pem_file(&fixture(&format!("{pinned}-daemon.cert"))).unwrap();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config(&pin)));
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let result = async {
        let mut tls = connector
            .connect(ServerName::try_from("127.0.0.1").unwrap(), tcp)
            .await?;
        tls.write_all(b"ping").await?;
        let mut b = [0u8; 4];
        tls.read_exact(&mut b).await?;
        assert_eq!(&b, b"ping");
        Ok(())
    }
    .await;
    let _ = server.await;
    result
}

#[tokio::test]
async fn accepts_the_pinned_v1_certificate_over_tls13() {
    handshake("a", "a", &[&rustls::version::TLS13])
        .await
        .unwrap();
}

#[tokio::test]
async fn accepts_the_pinned_v1_certificate_over_tls12() {
    handshake("a", "a", &[&rustls::version::TLS12])
        .await
        .unwrap();
}

#[tokio::test]
async fn refuses_any_other_certificate() {
    for versions in [
        &[&rustls::version::TLS13][..],
        &[&rustls::version::TLS12][..],
    ] {
        let err = handshake("b", "a", versions).await.unwrap_err();
        let inner = err
            .get_ref()
            .and_then(|e| e.downcast_ref::<rustls::Error>())
            .expect("a rustls error");
        assert!(
            inner.to_string().contains("ssl/daemon.cert"),
            "unexpected error: {inner}"
        );
    }
}

#[test]
fn reads_a_config_dirs_certificate() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("ssl")).unwrap();
    std::fs::copy(fixture("a-daemon.cert"), dir.path().join("ssl/daemon.cert")).unwrap();
    let pin = PinnedCert::from_config_dir(dir.path()).unwrap();
    let direct = PinnedCert::from_pem_file(&fixture("a-daemon.cert")).unwrap();
    assert_eq!(pin.der(), direct.der());
    assert!(matches!(
        PinnedCert::from_config_dir(&dir.path().join("missing")),
        Err(CertError::Io { .. })
    ));
    std::fs::write(dir.path().join("ssl/daemon.cert"), "not a certificate").unwrap();
    assert!(matches!(
        PinnedCert::from_config_dir(dir.path()),
        Err(CertError::NoCertificate(_))
    ));
}

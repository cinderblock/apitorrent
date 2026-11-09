//! instant-acme's HTTP client, over our reqwest with strict TLS.
//!
//! instant-acme's built-in client (hyper-rustls) would be a second HTTP stack and would
//! pick up rustls's process-wide default provider. This one uses ring explicitly, verifies
//! the ACME server with the OS (or, for a private ACME CA, only the roots given), ignores
//! proxy environment variables, and never follows redirects (ACME doesn't use them).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::BodyExt as _;
use instant_acme::{BodyWrapper, BytesResponse, Error, HttpClient};
use rustls::pki_types::CertificateDer;
use rustls::{ClientConfig, RootCertStore};
use rustls_platform_verifier::BuilderVerifierExt as _;

use crate::provider;

#[derive(Debug, Clone)]
pub(crate) struct ReqwestClient(reqwest::Client);

impl ReqwestClient {
    /// `roots` empty: the OS verifier. Otherwise exactly those roots (a private ACME CA).
    pub(crate) fn new(roots: &[CertificateDer<'static>]) -> Result<Self, String> {
        let builder = ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?;
        let mut config = if roots.is_empty() {
            builder
                .with_platform_verifier()
                .map_err(|e| e.to_string())?
                .with_no_client_auth()
        } else {
            let mut store = RootCertStore::empty();
            for r in roots {
                store
                    .add(r.clone())
                    .map_err(|e| format!("ACME directory CA: {e}"))?;
            }
            builder
                .with_root_certificates(Arc::new(store))
                .with_no_client_auth()
        };
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        reqwest::Client::builder()
            .tls_backend_preconfigured(config)
            .redirect(reqwest::redirect::Policy::none())
            .https_only(true)
            .no_proxy()
            .referer(false)
            .user_agent(concat!("apitorrent/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map(Self)
            .map_err(|e| e.to_string())
    }
}

fn other(e: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::Other(Box::new(e))
}

impl HttpClient for ReqwestClient {
    fn request(
        &self,
        req: http::Request<BodyWrapper<Bytes>>,
    ) -> Pin<Box<dyn Future<Output = Result<BytesResponse, Error>> + Send>> {
        let client = self.0.clone();
        Box::pin(async move {
            let (parts, body) = req.into_parts();
            let body = match body.collect().await {
                Ok(c) => c.to_bytes(),
                Err(never) => match never {},
            };
            let req = reqwest::Request::try_from(http::Request::from_parts(parts, body))
                .map_err(other)?;
            let resp = client.execute(req).await.map_err(other)?;
            Ok(BytesResponse::from(http::Response::<reqwest::Body>::from(
                resp,
            )))
        })
    }
}

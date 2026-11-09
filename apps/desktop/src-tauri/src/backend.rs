//! The active connection, Remote or Local, behind one interface: API requests and the sync
//! socket.

use std::sync::Arc;

use apitorrent_client::{RawResponse, Remote, SyncSocket};
use apitorrent_types::StorageRoot;

use crate::local::LocalNode;

#[derive(Debug, Clone)]
pub enum Backend {
    Remote(Remote),
    Local(Arc<LocalNode>),
}

/// The URL used for the sync socket over the in-process connection; only its path matters,
/// and its host passes the server's loopback check.
const LOCAL_SYNC_URL: &str = "ws://localhost/api/v1/sync";

impl Backend {
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
        content_type: Option<&str>,
    ) -> Result<RawResponse, String> {
        match self {
            Self::Remote(r) => r
                .request(method, path, body, content_type)
                .await
                .map_err(|e| e.to_string()),
            Self::Local(node) => {
                if !path.starts_with("/api/") {
                    return Err(format!("not an API path: {path}"));
                }
                let r = node
                    .embedded
                    .request(method, path, body, content_type)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(RawResponse {
                    status: r.status,
                    content_type: r.content_type,
                    body: r.body,
                })
            }
        }
    }

    pub async fn sync(&self) -> Result<SyncSocket, String> {
        match self {
            Self::Remote(r) => r.sync().await.map_err(|e| e.to_string()),
            Self::Local(node) => {
                let io = node.embedded.connect().map_err(|e| e.to_string())?;
                apitorrent_client::remote::websocket_over(Box::new(io), LOCAL_SYNC_URL, None)
                    .await
                    .map_err(|e| e.to_string())
            }
        }
    }

    pub async fn roots(&self) -> Result<Vec<StorageRoot>, String> {
        if let Self::Local(node) = self {
            return Ok(node.core.storage_roots());
        }
        let resp = self.request("GET", "/api/v1/roots", None, None).await?;
        match resp.status {
            200 => serde_json::from_slice(&resp.body).map_err(|e| e.to_string()),
            // An older daemon without storage roots: nothing to verify against.
            404 => Ok(Vec::new()),
            s => Err(format!(
                "the server answered {s}: {}",
                String::from_utf8_lossy(&resp.body)
            )),
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }
}

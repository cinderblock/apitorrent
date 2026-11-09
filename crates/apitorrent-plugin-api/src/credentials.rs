//! How the host checks API tokens for plugins that sign clients in themselves.
//!
//! A compatibility layer (qBittorrent's Web API, say) has clients that log in with a
//! username and password of their own. apitorrent's answer is that the password is an
//! apitorrent API token: no new kind of credential, nothing weaker than what the rest of
//! the API takes. The plugin hands the token to the host (`tokens:verify`), which checks it
//! here and keeps the resulting session itself; the plugin only ever holds an opaque
//! session id.

use std::fmt;
use std::sync::Arc;

use apitorrent_types::plugins::PluginCaller;

use crate::AppError;

/// A live API token: whose it is, and a way to ask later whether it still is.
#[derive(Clone)]
pub struct VerifiedToken {
    pub caller: PluginCaller,
    /// The token's public id (not the token).
    pub token_id: String,
    still_valid: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl VerifiedToken {
    /// `still_valid` answers whether the token still exists and hasn't expired. It is
    /// asked on every use of a session made from it, so revoking the token ends the
    /// session at once.
    pub fn new(
        caller: PluginCaller,
        token_id: String,
        still_valid: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            caller,
            token_id,
            still_valid: Arc::new(still_valid),
        }
    }

    pub fn still_valid(&self) -> bool {
        (self.still_valid)()
    }
}

impl fmt::Debug for VerifiedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VerifiedToken")
            .field("caller", &self.caller)
            .field("token_id", &self.token_id)
            .finish_non_exhaustive()
    }
}

/// Checks API tokens. Calls may block briefly (a database lookup); the host makes them off
/// any plugin's thread.
pub trait Credentials: Send + Sync + 'static {
    /// The token's owner, if `token` is a live API token. `Ok(None)` for anything else
    /// (unknown, expired, revoked, or not a token at all).
    fn verify_token(&self, token: &str) -> Result<Option<VerifiedToken>, AppError>;
}

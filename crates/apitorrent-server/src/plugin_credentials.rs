//! API tokens checked for plugins that sign clients in themselves (`tokens:verify`).
//!
//! The qBittorrent compatibility layer's clients log in with a username and password; the
//! password is an apitorrent API token, checked here exactly as a `Bearer` token is. The
//! plugin learns the token's owner and whether it may write; the host keeps the session
//! and asks [`apitorrent_auth::Auth::still_valid`] on every use, so revoking the token (or
//! its expiry) ends the session at once.

use std::sync::Arc;

use apitorrent_auth::{Auth, Credential};
use apitorrent_plugin_api::{AppError, Credentials, PluginCaller, VerifiedToken};

use crate::AuthMode;

/// The token check for the plugin host, or `None` when authentication is off (there are no
/// tokens then, and plugins can't sign anyone in).
pub fn plugin_credentials(auth: &AuthMode) -> Option<Arc<dyn Credentials>> {
    match auth {
        AuthMode::Enabled(a) => Some(Arc::new(AuthCredentials(a.clone()))),
        AuthMode::DisabledOnLoopback => None,
    }
}

struct AuthCredentials(Auth);

impl Credentials for AuthCredentials {
    fn verify_token(
        &self,
        token: &str,
        space: apitorrent_types::spaces::SpaceId,
    ) -> Result<Option<VerifiedToken>, AppError> {
        let principal = match self.0.authenticate_token(token) {
            Ok(Some(p)) => p,
            Ok(None) => return Ok(None),
            Err(e) => {
                tracing::error!(error = %e, "could not check a token for a plugin");
                return Err(AppError::new("internal", "could not check the token"));
            }
        };
        let Credential::Token { id, .. } = &principal.credential else {
            return Ok(None);
        };
        let role = match self.0.space_role(&principal, space) {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(None),
            Err(e) => {
                tracing::error!(error = %e, "could not check a token's space for a plugin");
                return Err(AppError::new("internal", "could not check the token"));
            }
        };
        let caller = PluginCaller {
            user_id: principal.user.id.clone(),
            user_name: principal.user.name.clone(),
            role: principal.user.role,
            may_write: principal.may_write() && role >= apitorrent_types::spaces::SpaceRole::User,
        };
        let token_id = id.clone();
        let auth = self.0.clone();
        Ok(Some(VerifiedToken::new(caller, token_id, move || {
            auth.still_valid(&principal).unwrap_or_else(|e| {
                tracing::error!(error = %e, "could not recheck a plugin session's token");
                false
            })
        })))
    }
}

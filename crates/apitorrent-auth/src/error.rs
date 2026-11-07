use std::time::Duration;

/// Why an auth operation failed. The server maps each variant to a status and a stable
/// error code ([`AuthError::code`]).
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("{0}")]
    InvalidRequest(String),
    /// Deliberately vague: the client learns only that it didn't work.
    #[error("authentication failed")]
    Unauthorized,
    #[error("the setup token is wrong or has expired")]
    InvalidSetupToken,
    #[error("this instance is already set up")]
    AlreadySetUp,
    #[error("too many attempts; try again in {} seconds", .0.as_secs().max(1))]
    RateLimited(Duration),
    #[error("sign in again to confirm it's you")]
    ReauthRequired,
    #[error("{0}")]
    Forbidden(&'static str),
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Conflict(String),
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("internal error: {0}")]
    Internal(String),
}

impl AuthError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "invalid_request",
            Self::Unauthorized => "unauthorized",
            Self::InvalidSetupToken => "invalid_setup_token",
            Self::AlreadySetUp => "already_set_up",
            Self::RateLimited(_) => "rate_limited",
            Self::ReauthRequired => "reauth_required",
            Self::Forbidden(_) => "forbidden",
            Self::NotFound => "not_found",
            Self::Unavailable(_) => "unavailable",
            Self::Conflict(_) => "conflict",
            Self::Database(_) | Self::Internal(_) => "internal",
        }
    }
}

pub type Result<T, E = AuthError> = std::result::Result<T, E>;

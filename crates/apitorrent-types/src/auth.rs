//! Types of the `/api/v1/auth` endpoints: setup, login, sessions, passkeys and API tokens.
//!
//! WebAuthn options and credentials travel as the JSON the WebAuthn Level 3 spec defines
//! (`PublicKeyCredentialCreationOptionsJSON`, `RegistrationResponseJSON`, …). They are
//! opaque here (`unknown` in TypeScript); the browser library and the server's WebAuthn
//! implementation own their shape.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum Role {
    /// Everything, including managing other users (once that lands).
    Admin,
    /// Full access to torrents.
    User,
    /// Read-only access.
    Viewer,
}

/// How passwords may be used on this instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum PasswordMode {
    /// Passwords can't be set or used. Passkeys only.
    #[default]
    Off,
    /// A password alone signs in, for browsers or hosts without passkey support.
    Fallback,
    /// Users who set a password must enter it after their passkey. It never signs in alone.
    SecondFactor,
}

/// What an API token may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum TokenScope {
    /// `GET` requests and the sync WebSocket only.
    Read,
    /// Everything except managing credentials (passkeys, passwords, tokens, sessions).
    Full,
}

/// How a session was signed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum SessionMethod {
    Passkey,
    Password,
    /// A passkey followed by the second-factor password.
    PasskeyAndPassword,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct UserInfo {
    pub id: String,
    /// The login name: lowercase letters, digits, `.`, `_` and `-`.
    pub name: String,
    pub display_name: String,
    pub role: Role,
    pub has_password: bool,
}

/// The caller's current cookie session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SessionState {
    pub method: SessionMethod,
    #[ts(type = "number")]
    pub created_at: i64,
    /// The absolute expiry. The session also ends after a period without requests.
    #[ts(type = "number")]
    pub expires_at: i64,
    /// Until when sensitive account changes are allowed without signing in again.
    #[ts(type = "number")]
    pub reauth_until: i64,
}

/// `GET /api/v1/auth/status`: everything the UI needs to decide which page to show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
#[allow(clippy::struct_excessive_bools)]
pub struct AuthStatus {
    /// No user exists yet. Only the setup page works until the first passkey is enrolled.
    pub setup_required: bool,
    /// The daemon runs with `--insecure-no-auth-loopback`: every loopback request is the
    /// local admin. Account management is unavailable.
    pub auth_disabled: bool,
    /// Signed in (a full session, an API token, or the no-auth local user).
    pub user: Option<UserInfo>,
    /// Present for cookie sessions, including one waiting for its second factor.
    pub session: Option<SessionState>,
    /// The passkey worked; the password second factor is still needed.
    pub second_factor_required: bool,
    /// Whether passkeys work on the host this request came to. They need a configured
    /// relying party for the host, which an IP address can never be.
    pub passkeys_available: bool,
    /// When passkeys aren't available here, where they are (such as `http://localhost:8742`).
    pub passkey_hint: Option<String>,
    pub password_mode: PasswordMode,
    /// The request reached the daemon over plain HTTP on a non-loopback address.
    pub insecure_transport: bool,
}

/// The first half of a WebAuthn ceremony.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct CeremonyStart {
    /// Echo this back in the matching `finish` call.
    pub ceremony_id: String,
    /// `PublicKeyCredentialCreationOptionsJSON` or `PublicKeyCredentialRequestOptionsJSON`.
    #[ts(type = "unknown")]
    pub options: serde_json::Value,
}

/// `POST /api/v1/auth/setup/start`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SetupStartRequest {
    /// The one-time token the daemon printed on its console.
    pub token: String,
    pub username: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

/// `POST /api/v1/auth/setup/finish`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SetupFinishRequest {
    pub ceremony_id: String,
    /// `RegistrationResponseJSON`.
    #[ts(type = "unknown")]
    pub credential: serde_json::Value,
    #[serde(default)]
    pub passkey_name: Option<String>,
}

/// `POST /api/v1/auth/setup/password`: bootstrap with a password instead of a passkey.
/// Only in [`PasswordMode::Fallback`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SetupPasswordRequest {
    pub token: String,
    pub username: String,
    #[serde(default)]
    pub display_name: Option<String>,
    pub password: String,
}

/// Finish a login or re-authentication passkey ceremony.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PasskeyFinishRequest {
    pub ceremony_id: String,
    /// `AuthenticationResponseJSON`.
    #[ts(type = "unknown")]
    pub credential: serde_json::Value,
}

/// `POST /api/v1/auth/login/password`. In second-factor mode the session from the passkey
/// step identifies the user and `username` is ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PasswordLoginRequest {
    #[serde(default)]
    pub username: Option<String>,
    pub password: String,
}

/// `POST /api/v1/auth/reauth/password`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PasswordRequest {
    pub password: String,
}

/// `POST /api/v1/auth/passkeys/finish`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct AddPasskeyFinishRequest {
    pub ceremony_id: String,
    /// `RegistrationResponseJSON`.
    #[ts(type = "unknown")]
    pub credential: serde_json::Value,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct RenameRequest {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PasskeyInfo {
    pub id: String,
    pub name: String,
    /// The host this passkey works on.
    pub rp_id: String,
    #[ts(type = "number")]
    pub created_at: i64,
    #[ts(type = "number | null")]
    pub last_used_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct CreateTokenRequest {
    pub name: String,
    pub scope: TokenScope,
    /// Omit for a token that lasts until revoked.
    #[serde(default)]
    pub expires_in_days: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ApiTokenInfo {
    pub id: String,
    pub name: String,
    pub scope: TokenScope,
    /// The first characters of the token, to tell tokens apart.
    pub prefix: String,
    #[ts(type = "number")]
    pub created_at: i64,
    #[ts(type = "number | null")]
    pub expires_at: Option<i64>,
    #[ts(type = "number | null")]
    pub last_used_at: Option<i64>,
}

/// The response to creating a token. The secret is shown this once and never again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct CreatedToken {
    /// Send it as `Authorization: Bearer <token>`.
    pub token: String,
    pub info: ApiTokenInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SessionInfo {
    /// A public id for revoking the session. Not the cookie value.
    pub id: String,
    pub method: SessionMethod,
    #[ts(type = "number")]
    pub created_at: i64,
    #[ts(type = "number")]
    pub last_seen_at: i64,
    #[ts(type = "number")]
    pub expires_at: i64,
    pub user_agent: Option<String>,
    pub ip: Option<String>,
    /// The session making this request.
    pub current: bool,
}

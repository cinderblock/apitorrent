//! Invitations, recovery links and user management.
//!
//! An **invitation** is a one-time link that creates a user with the spaces and roles it
//! carries. A **recovery link** is built the same way for an existing user who lost their
//! credentials: whoever opens it registers new ones in place of the old. Both expire, can be
//! revoked, and are stored only as a hash of their secret. The secret travels in the link's
//! fragment (`/enroll#token=…`), so it never reaches an access log.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::auth::{PasswordMode, Role};
use crate::spaces::{SpaceId, SpaceRole};

/// How long a link lasts when the request doesn't say.
pub const DEFAULT_LINK_DAYS: u32 = 7;
/// The longest a link may last.
pub const MAX_LINK_DAYS: u32 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum EnrollmentKind {
    /// Creates a new user.
    Invitation,
    /// Replaces an existing user's sign-in credentials.
    Recovery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum EnrollmentState {
    /// Waiting to be opened.
    Pending,
    /// Used once; it works no more.
    Used,
    Revoked,
    Expired,
}

/// A space an invitation grants, with the role there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct GrantedSpace {
    #[ts(type = "number")]
    pub space: SpaceId,
    pub slug: String,
    pub name: String,
    pub role: SpaceRole,
}

/// An invitation or recovery link, as its issuers see it. The secret is never shown again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct InvitationInfo {
    pub id: String,
    pub kind: EnrollmentKind,
    pub state: EnrollmentState,
    /// The instance role the new user gets (invitations only).
    pub role: Option<Role>,
    /// The spaces the new user joins. For a space admin's view, only that space.
    pub spaces: Vec<GrantedSpace>,
    /// The user a recovery link is for, or the user an invitation created once used.
    pub user_name: Option<String>,
    /// Who issued it, if they still exist.
    pub created_by: Option<String>,
    #[ts(type = "number")]
    pub created_at: i64,
    #[ts(type = "number")]
    pub expires_at: i64,
    #[ts(type = "number | null")]
    pub used_at: Option<i64>,
    /// A note for the issuers, such as who it is for.
    pub note: Option<String>,
}

/// One space and role in a [`CreateInvitationRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct InviteSpace {
    pub slug: String,
    pub role: SpaceRole,
}

/// `POST /api/v1/invitations`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct CreateInvitationRequest {
    /// The instance role. Only a server admin may give anything but `user`.
    #[serde(default)]
    pub role: Option<Role>,
    pub spaces: Vec<InviteSpace>,
    /// 1 to 30; 7 when omitted.
    #[serde(default)]
    pub expires_in_days: Option<u32>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/v1/users/{id}/recovery`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct CreateRecoveryRequest {
    /// 1 to 30; 7 when omitted.
    #[serde(default)]
    pub expires_in_days: Option<u32>,
}

/// A new invitation or recovery link. The link is shown this once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct CreatedInvitation {
    /// `<origin>/enroll#token=<secret>`: send it to the person it is for.
    pub link: String,
    pub invitation: InvitationInfo,
}

/// The body of the public enrollment calls that take the link's secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct EnrollTokenRequest {
    pub token: String,
}

/// A space, as the enrollment page names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct EnrollSpace {
    pub name: String,
    pub role: SpaceRole,
}

/// `POST /api/v1/auth/enroll/info`: what a link does, for the page that opens it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct EnrollInfo {
    pub kind: EnrollmentKind,
    /// The account a recovery link is for; invitations let the person pick one.
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub role: Role,
    pub spaces: Vec<EnrollSpace>,
    #[ts(type = "number")]
    pub expires_at: i64,
    /// Who issued it.
    pub invited_by: Option<String>,
    /// Whether a password can stand in for a passkey here (`--passwords fallback`).
    pub password_mode: PasswordMode,
}

/// `POST /api/v1/auth/enroll/start`. A recovery link ignores `username`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct EnrollStartRequest {
    pub token: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

/// `POST /api/v1/auth/enroll/finish`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct EnrollFinishRequest {
    pub ceremony_id: String,
    /// `RegistrationResponseJSON`.
    #[ts(type = "unknown")]
    pub credential: serde_json::Value,
    #[serde(default)]
    pub passkey_name: Option<String>,
}

/// `POST /api/v1/auth/enroll/password`: enroll with a password instead of a passkey
/// (`--passwords fallback` only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct EnrollPasswordRequest {
    pub token: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub display_name: Option<String>,
    pub password: String,
}

/// A user's membership, for the Users page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct UserSpace {
    #[ts(type = "number")]
    pub space: SpaceId,
    pub slug: String,
    pub name: String,
    pub role: SpaceRole,
}

/// A user, as server admins manage them (`GET /api/v1/users`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ManagedUser {
    pub id: String,
    pub name: String,
    pub display_name: String,
    /// The instance role; `admin` is a server admin.
    pub role: Role,
    pub disabled: bool,
    #[ts(type = "number")]
    pub created_at: i64,
    pub passkeys: u32,
    pub has_password: bool,
    /// API tokens (they stop working while the user is disabled).
    pub tokens: u32,
    /// The most recent request of any of their sessions.
    #[ts(type = "number | null")]
    pub last_seen_at: Option<i64>,
    pub spaces: Vec<UserSpace>,
    /// The caller.
    pub you: bool,
}

/// `PATCH /api/v1/users/{id}`: change what is given, leave the rest.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct UpdateUserRequest {
    #[serde(default)]
    pub role: Option<Role>,
    #[serde(default)]
    pub disabled: Option<bool>,
}

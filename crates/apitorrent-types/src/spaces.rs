//! Spaces: named partitions of one daemon, each with its own storage, limits, labels,
//! automation and members. Every torrent, rule and API token belongs to one.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// A space's id. The `default` space, which holds everything from before spaces, is 1.
pub type SpaceId = i64;

/// The space that holds everything created before spaces existed.
pub const DEFAULT_SPACE: SpaceId = 1;

/// [`DEFAULT_SPACE`], for `serde(default)`.
pub fn default_space() -> SpaceId {
    DEFAULT_SPACE
}

/// The slug of [`DEFAULT_SPACE`].
pub const DEFAULT_SLUG: &str = "default";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Space {
    #[ts(type = "number")]
    pub id: SpaceId,
    /// Used in URLs (`/s/<slug>/`): lowercase letters, digits and dashes.
    pub slug: String,
    pub name: String,
    pub description: String,
    pub settings: SpaceSettings,
}

/// A space's storage and limits. Zero means "no limit of its own" (the instance's still
/// apply).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
pub struct SpaceSettings {
    /// Absolute directories this space may save into. Empty: anywhere (the `default`
    /// space's setting after the upgrade, so nothing changes for it).
    pub storage_roots: Vec<String>,
    /// Where new torrents go when the request names no folder. `None`: the instance's
    /// download directory for `default`; otherwise the first storage root, or a folder
    /// named after the space in the instance's download directory.
    pub download_dir: Option<String>,
    /// Folders by label: a torrent added with the label and no folder goes there.
    pub folder_rules: Vec<FolderRule>,
    #[ts(type = "number")]
    pub download_rate: u64,
    #[ts(type = "number")]
    pub upload_rate: u64,
    /// Open peer connections across the space's torrents.
    pub max_connections: u32,
    /// Active torrents at once, as the instance's queue counts them.
    pub max_active: u32,
    pub max_active_downloads: u32,
    pub max_active_seeds: u32,
}

/// Torrents with `label` go to `folder` (absolute, or relative to the space's download
/// directory).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct FolderRule {
    pub label: String,
    pub folder: String,
}

/// A space's role, held per space. Server admins (the instance's `admin` role) act as
/// `admin` in every space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum SpaceRole {
    Viewer,
    User,
    Admin,
}

impl SpaceRole {
    pub fn as_str(self) -> &'static str {
        match self {
            SpaceRole::Admin => "admin",
            SpaceRole::User => "user",
            SpaceRole::Viewer => "viewer",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "admin" => Some(SpaceRole::Admin),
            "user" => Some(SpaceRole::User),
            "viewer" => Some(SpaceRole::Viewer),
            _ => None,
        }
    }
}

/// A space as one caller sees it: with their role there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SpaceView {
    #[ts(type = "number")]
    pub id: SpaceId,
    pub slug: String,
    pub name: String,
    pub description: String,
    pub role: SpaceRole,
}

/// `GET /api/v1/space`: the space this request is in, and every space the caller may use
/// (for the switcher).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SpaceContext {
    pub current: SpaceView,
    pub spaces: Vec<SpaceView>,
    /// The caller administers the whole server: every space, its members, new spaces.
    pub server_admin: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct CreateSpaceRequest {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub settings: SpaceSettings,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct UpdateSpaceRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub settings: SpaceSettings,
}

/// One member of a space.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SpaceMember {
    pub user_id: String,
    pub name: String,
    pub display_name: String,
    pub role: SpaceRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SetMemberRequest {
    pub role: SpaceRole,
}

/// A user, for picking members (server admins only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct UserSummary {
    pub id: String,
    pub name: String,
    pub display_name: String,
    /// The instance role; `admin` is a server admin.
    pub role: String,
}

/// Whether `slug` may name a space: 1 to 40 of `a-z`, `0-9` and `-`, not starting or
/// ending with `-`.
pub fn valid_slug(slug: &str) -> bool {
    (1..=40).contains(&slug.len())
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !slug.starts_with('-')
        && !slug.ends_with('-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        for ok in ["default", "tv", "a-b-2", "x"] {
            assert!(valid_slug(ok), "{ok}");
        }
        for bad in ["", "-a", "a-", "A", "a b", "a/b", "é", &"a".repeat(41)] {
            assert!(!valid_slug(bad), "{bad}");
        }
    }
}

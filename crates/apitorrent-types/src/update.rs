//! Types of `/api/v1/system/update`: whether a newer apitorrent exists, and installing it.
//!
//! The daemon looks at the latest *published* GitHub release (never drafts or
//! prereleases). It installs only a build signed with apitorrent's update key, never an
//! older version, and only when an admin asks, unless [`UpdateSettings::auto_install`] is
//! on.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct UpdateSettings {
    /// Look for a new release once a day.
    pub check_daily: bool,
    /// Install a new release as soon as a check finds it (and restart). Off by default:
    /// an admin installs from the UI, the API or `apitorrentd update`.
    pub auto_install: bool,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            check_daily: true,
            auto_install: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum UpdatePhase {
    #[default]
    Idle,
    Checking,
    /// Downloading and verifying.
    Installing,
    /// The new binary is in place; the daemon is restarting into it.
    Restarting,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct UpdateRelease {
    pub version: String,
    /// The release page.
    pub url: String,
    /// The release notes (Markdown), shortened if long.
    pub notes: Option<String>,
    #[ts(type = "number | null")]
    pub published_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct UpdateStatus {
    /// The running version.
    pub current_version: String,
    /// The latest published release, as of the last check.
    pub latest: Option<UpdateRelease>,
    /// `latest` is newer than the running version.
    pub available: bool,
    /// Whether this build can install updates. Builds without the update signing key
    /// (forks, local builds) only report them.
    pub installs_enabled: bool,
    /// Why installs are off, when they are.
    pub installs_disabled_reason: Option<String>,
    #[ts(type = "number | null")]
    pub checked_ms: Option<i64>,
    /// The last check's or install's failure, cleared by the next success.
    pub last_error: Option<String>,
    pub phase: UpdatePhase,
    pub settings: UpdateSettings,
}

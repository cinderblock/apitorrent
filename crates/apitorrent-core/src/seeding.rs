//! Seed limits: when a finished torrent has shared enough.
//!
//! A torrent stops seeding at a share ratio, after a total seeding time, or both, and is
//! then paused, removed, or removed with its files. The limits come from the torrent
//! itself, else its group, else the instance. They act once: a torrent the user resumes
//! afterwards seeds on, until its own limits change.

use apitorrent_types::{GroupSettings, SeedLimits, Settings};

/// What is kept per torrent, in memory and in the `seeding` table.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SeedState {
    /// Time spent seeding, in milliseconds, across restarts (stored in whole seconds).
    pub seed_ms: u64,
    /// The torrent's own limits, overriding its group's and the instance's.
    pub limits: Option<SeedLimits>,
    /// The limits were reached and acted on.
    pub reached: bool,
}

/// The limits that apply to a torrent in `group`, given its own.
pub fn effective(settings: &Settings, group: Option<&str>, own: Option<SeedLimits>) -> SeedLimits {
    own.or_else(|| {
        let g: &GroupSettings = settings
            .groups
            .iter()
            .find(|g| Some(g.name.as_str()) == group)?;
        g.seed_limits
    })
    .unwrap_or(settings.seed_limits)
}

/// The share ratio: uploaded over downloaded, or over the size for a torrent that
/// downloaded nothing (it came from the library, or was added complete).
#[allow(clippy::cast_precision_loss)]
pub fn ratio(uploaded: u64, downloaded: u64, size: u64) -> f64 {
    let base = if downloaded > 0 { downloaded } else { size };
    if base == 0 {
        return 0.0;
    }
    uploaded as f64 / base as f64
}

/// Whether `limits` are reached.
pub fn reached(limits: &SeedLimits, ratio: f64, seed_secs: u64) -> bool {
    (limits.ratio > 0.0 && ratio >= limits.ratio)
        || (limits.seed_minutes > 0 && seed_secs >= u64::from(limits.seed_minutes) * 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use apitorrent_types::SeedLimitAction;

    fn limits(ratio: f64, seed_minutes: u32) -> SeedLimits {
        SeedLimits {
            ratio,
            seed_minutes,
            action: SeedLimitAction::Pause,
        }
    }

    #[test]
    fn the_most_specific_limits_win() {
        let mut s = crate::settings::defaults(&apitorrent_engine::SessionConfig::default());
        s.seed_limits = limits(2.0, 0);
        s.groups.push(GroupSettings {
            name: "linux".into(),
            labels: vec!["iso".into()],
            download_rate: 0,
            upload_rate: 0,
            max_connections: 0,
            seed_limits: Some(limits(0.0, 60)),
        });
        assert_eq!(effective(&s, None, None), limits(2.0, 0));
        assert_eq!(effective(&s, Some("linux"), None), limits(0.0, 60));
        assert_eq!(
            effective(&s, Some("linux"), Some(limits(5.0, 0))),
            limits(5.0, 0)
        );
        // An explicit "no limits" of its own beats both.
        assert!(!effective(&s, Some("linux"), Some(limits(0.0, 0))).active());
    }

    #[test]
    fn ratio_and_time() {
        assert!((ratio(300, 100, 1000) - 3.0).abs() < 1e-9);
        assert!(
            (ratio(500, 0, 1000) - 0.5).abs() < 1e-9,
            "by size when nothing was downloaded"
        );
        assert!(ratio(5, 0, 0).abs() < 1e-9);
        assert!(reached(&limits(1.0, 0), 1.0, 0));
        assert!(!reached(&limits(1.0, 0), 0.99, 1_000_000));
        assert!(reached(&limits(0.0, 2), 0.0, 120));
        assert!(!reached(&limits(0.0, 2), 0.0, 119));
        assert!(
            !reached(&limits(0.0, 0), 100.0, 1_000_000),
            "no limits, never"
        );
    }
}

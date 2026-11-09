//! When to (re)issue: pure functions of the clock, so they can be tested directly.

use std::time::{Duration, SystemTime};

/// Renew once this fraction of the certificate's lifetime has passed: 2/3, as Let's
/// Encrypt recommends (day 60 of 90; day 4 of a 6-day short-lived certificate).
const RENEW_AT_NUM: u32 = 2;
const RENEW_AT_DEN: u32 = 3;

/// The first retry after a failure, doubling each time.
pub(crate) const BACKOFF_START: Duration = Duration::from_secs(5 * 60);
/// The longest wait between attempts while the certificate still has time left.
pub(crate) const BACKOFF_MAX: Duration = Duration::from_secs(6 * 3600);
/// The longest wait once the certificate is close to expiry, or there is none.
pub(crate) const BACKOFF_MAX_URGENT: Duration = Duration::from_secs(3600);
/// "Close to expiry".
const URGENT: Duration = Duration::from_secs(7 * 24 * 3600);

/// What we know about the certificate on hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Held {
    pub not_before: SystemTime,
    pub not_after: SystemTime,
    /// It names exactly the configured domains.
    pub matches_config: bool,
}

/// When the next issuance attempt should start.
///
/// - No certificate, or one for other names: now (or after the backoff, if attempts have
///   been failing).
/// - Otherwise at 2/3 of its lifetime, pulled earlier by `jitter` (a fraction in `0..=1`
///   of up to 1/30 of the lifetime, so many daemons don't renew in lockstep).
/// - After `failures` consecutive failures, no sooner than `last_failure + backoff`.
pub(crate) fn next_attempt(
    held: Option<Held>,
    failures: u32,
    last_failure: Option<SystemTime>,
    jitter: f64,
) -> SystemTime {
    let due = match held {
        Some(h) if h.matches_config => {
            let lifetime = h.not_after.duration_since(h.not_before).unwrap_or_default();
            let spread = (lifetime / 30).mul_f64(jitter.clamp(0.0, 1.0));
            h.not_before + lifetime * RENEW_AT_NUM / RENEW_AT_DEN - spread
        }
        _ => SystemTime::UNIX_EPOCH,
    };
    match (failures, last_failure) {
        (0, _) | (_, None) => due,
        (n, Some(at)) => {
            let urgent = match held {
                Some(h) if h.matches_config => h
                    .not_after
                    .duration_since(at)
                    .map_or(true, |left| left < URGENT),
                _ => true,
            };
            due.max(at + backoff(n, urgent))
        }
    }
}

/// The wait after `failures` consecutive failures (at least 1).
pub(crate) fn backoff(failures: u32, urgent: bool) -> Duration {
    let cap = if urgent {
        BACKOFF_MAX_URGENT
    } else {
        BACKOFF_MAX
    };
    let shift = failures.saturating_sub(1).min(16);
    (BACKOFF_START * (1u32 << shift)).min(cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: Duration = Duration::from_secs(86_400);

    fn t(days: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH
            + DAY * u32::try_from(days).unwrap()
            + Duration::from_secs(1_000_000_000)
    }

    fn held(from: u64, to: u64) -> Held {
        Held {
            not_before: t(from),
            not_after: t(to),
            matches_config: true,
        }
    }

    #[test]
    fn renews_at_two_thirds() {
        assert_eq!(next_attempt(Some(held(0, 90)), 0, None, 0.0), t(60));
        // Full jitter pulls it 3 days (1/30 of 90) earlier.
        assert_eq!(next_attempt(Some(held(0, 90)), 0, None, 1.0), t(57));
        // A six-day certificate renews on day 4.
        assert_eq!(next_attempt(Some(held(0, 6)), 0, None, 0.0), t(4));
    }

    #[test]
    fn missing_or_mismatched_is_due_now() {
        assert_eq!(next_attempt(None, 0, None, 0.5), SystemTime::UNIX_EPOCH);
        let mut h = held(0, 90);
        h.matches_config = false;
        assert_eq!(next_attempt(Some(h), 0, None, 0.5), SystemTime::UNIX_EPOCH);
    }

    #[test]
    fn backs_off_after_failures() {
        assert_eq!(backoff(1, false), BACKOFF_START);
        assert_eq!(backoff(2, false), BACKOFF_START * 2);
        assert_eq!(backoff(5, false), BACKOFF_START * 16);
        assert_eq!(backoff(10, false), BACKOFF_MAX);
        assert_eq!(backoff(10, true), BACKOFF_MAX_URGENT);
        assert_eq!(backoff(u32::MAX, false), BACKOFF_MAX);

        // No certificate: retries are urgent (capped at an hour).
        let at = t(0);
        assert_eq!(next_attempt(None, 1, Some(at), 0.0), at + BACKOFF_START);
        assert_eq!(
            next_attempt(None, 20, Some(at), 0.0),
            at + BACKOFF_MAX_URGENT
        );

        // Renewal failing with plenty of time left: up to six hours apart.
        let h = held(0, 90);
        let failed = t(61);
        assert_eq!(
            next_attempt(Some(h), 20, Some(failed), 0.0),
            failed + BACKOFF_MAX
        );
        // Within a week of expiry: at most an hour apart.
        let failed = t(85);
        assert_eq!(
            next_attempt(Some(h), 20, Some(failed), 0.0),
            failed + BACKOFF_MAX_URGENT
        );
        // A backoff never brings renewal earlier than it was due.
        assert_eq!(next_attempt(Some(h), 1, Some(t(1)), 0.0), t(60));
    }
}

//! A token-bucket rate limiter driven by caller-supplied time, so it stays deterministic in
//! tests.

use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct RateLimiter {
    per_second: f64,
    burst: f64,
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    /// A limiter that allows `per_second` on average and up to `burst` at once. It starts
    /// full.
    pub fn new(per_second: u32, burst: u32, now: Instant) -> Self {
        let burst = f64::from(burst.max(1));
        Self {
            per_second: f64::from(per_second.max(1)),
            burst,
            tokens: burst,
            last: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + dt * self.per_second).min(self.burst);
        self.last = now;
    }

    /// Take one token if one is available.
    pub fn try_take(&mut self, now: Instant) -> bool {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// How long until a token is available (zero if one is now).
    pub fn wait_time(&mut self, now: Instant) -> Duration {
        self.refill(now);
        if self.tokens >= 1.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64((1.0 - self.tokens) / self.per_second)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bursts_then_limits() {
        let t0 = Instant::now();
        let mut r = RateLimiter::new(10, 5, t0);
        for _ in 0..5 {
            assert!(r.try_take(t0));
        }
        assert!(!r.try_take(t0));
        assert!(r.wait_time(t0) > Duration::ZERO);
        assert!(r.try_take(t0 + Duration::from_millis(100)));
        assert!(!r.try_take(t0 + Duration::from_millis(100)));
        // A long idle period refills only up to the burst.
        let later = t0 + Duration::from_secs(100);
        for _ in 0..5 {
            assert!(r.try_take(later));
        }
        assert!(!r.try_take(later));
    }
}

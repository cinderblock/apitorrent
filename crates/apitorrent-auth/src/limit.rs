//! Rate limits and lockouts for the endpoints that take credentials.
//!
//! Two mechanisms, keyed by client address and by account:
//!
//! - **A token bucket** caps how often an address may call the login and setup endpoints at
//!   all, successful or not.
//! - **Lockout** after consecutive failures, doubling from one minute up to an hour. A
//!   success resets it.
//!
//! Account lockout applies to passwords only. Passkey sign-in can't be guessed, so locking
//! it would only let an attacker lock the owner out.
//!
//! IPv6 clients are keyed by their /64, since one host usually controls a whole /64.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::clock::{Clock, ms};
use crate::error::AuthError;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Policy {
    /// Bucket refill per minute. `0.0` disables the bucket.
    pub per_minute: f64,
    pub burst: f64,
    /// Consecutive failures before a lockout.
    pub max_failures: u32,
    pub first_lockout: Duration,
    pub max_lockout: Duration,
}

/// Login and setup calls from one address.
pub(crate) const ADDRESS: Policy = Policy {
    per_minute: 30.0,
    burst: 15.0,
    max_failures: 10,
    first_lockout: Duration::from_secs(60),
    max_lockout: Duration::from_secs(3600),
};

/// Password guesses against one account, from anywhere.
pub(crate) const ACCOUNT: Policy = Policy {
    per_minute: 0.0,
    burst: 0.0,
    max_failures: 5,
    first_lockout: Duration::from_secs(60),
    max_lockout: Duration::from_secs(3600),
};

/// Entries beyond this get pruned (idle and unlocked ones first).
const MAX_ENTRIES: usize = 20_000;

#[derive(Debug, Clone, Copy)]
struct Entry {
    tokens: f64,
    refilled_at: i64,
    failures: u32,
    locked_until: i64,
    touched: i64,
}

#[derive(Debug)]
pub(crate) struct Limiter {
    clock: Arc<dyn Clock>,
    entries: Mutex<HashMap<String, Entry>>,
}

pub(crate) fn address_key(ip: Option<IpAddr>) -> String {
    match ip {
        None => "addr:unknown".to_owned(),
        Some(IpAddr::V4(v4)) => format!("addr:{v4}"),
        Some(IpAddr::V6(v6)) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                format!("addr:{v4}")
            } else {
                let s = v6.segments();
                format!("addr:{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
            }
        }
    }
}

pub(crate) fn account_key(name: &str) -> String {
    format!("account:{name}")
}

impl Limiter {
    pub(crate) fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn fresh(policy: &Policy, now: i64) -> Entry {
        Entry {
            tokens: policy.burst,
            refilled_at: now,
            failures: 0,
            locked_until: 0,
            touched: now,
        }
    }

    /// Count one attempt. Refuses if the key is locked out or out of tokens.
    pub(crate) fn check(&self, key: &str, policy: &Policy) -> Result<(), AuthError> {
        let now = self.clock.now_ms();
        let mut entries = self.entries.lock().expect("limiter lock");
        if entries.len() >= MAX_ENTRIES && !entries.contains_key(key) {
            prune(&mut entries, now);
        }
        let e = entries
            .entry(key.to_owned())
            .or_insert_with(|| Self::fresh(policy, now));
        e.touched = now;
        if e.locked_until > now {
            return Err(retry_after(e.locked_until - now));
        }
        if policy.per_minute > 0.0 {
            #[allow(clippy::cast_precision_loss)]
            let elapsed_min = (now - e.refilled_at) as f64 / 60_000.0;
            e.tokens = (e.tokens + elapsed_min * policy.per_minute).min(policy.burst);
            e.refilled_at = now;
            if e.tokens < 1.0 {
                let wait_ms = ((1.0 - e.tokens) / policy.per_minute * 60_000.0).ceil();
                #[allow(clippy::cast_possible_truncation)]
                return Err(retry_after(wait_ms as i64));
            }
            e.tokens -= 1.0;
        }
        Ok(())
    }

    /// Record a failure. Returns `true` if this one started a lockout.
    pub(crate) fn fail(&self, key: &str, policy: &Policy) -> bool {
        let now = self.clock.now_ms();
        let mut entries = self.entries.lock().expect("limiter lock");
        let e = entries
            .entry(key.to_owned())
            .or_insert_with(|| Self::fresh(policy, now));
        e.touched = now;
        e.failures += 1;
        if e.failures >= policy.max_failures {
            let doublings = (e.failures - policy.max_failures).min(16);
            let lockout = policy
                .first_lockout
                .saturating_mul(1 << doublings)
                .min(policy.max_lockout);
            e.locked_until = now + ms(lockout);
            return true;
        }
        false
    }

    pub(crate) fn succeed(&self, key: &str) {
        let mut entries = self.entries.lock().expect("limiter lock");
        if let Some(e) = entries.get_mut(key) {
            e.failures = 0;
            e.locked_until = 0;
        }
    }
}

fn retry_after(ms: i64) -> AuthError {
    AuthError::RateLimited(Duration::from_millis(u64::try_from(ms.max(1)).unwrap_or(1)))
}

fn prune(entries: &mut HashMap<String, Entry>, now: i64) {
    const IDLE_MS: i64 = 15 * 60 * 1000;
    entries.retain(|_, e| e.locked_until > now || now - e.touched < IDLE_MS);
    if entries.len() >= MAX_ENTRIES {
        // Still full: forget everything that isn't locked out. Buckets refill anyway.
        entries.retain(|_, e| e.locked_until > now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;

    #[test]
    fn bucket_and_lockout() {
        let clock = Arc::new(ManualClock::new(1_000_000));
        let l = Limiter::new(clock.clone());
        for _ in 0..15 {
            l.check("a", &ADDRESS).unwrap();
        }
        assert!(matches!(
            l.check("a", &ADDRESS),
            Err(AuthError::RateLimited(_))
        ));
        clock.advance(Duration::from_secs(2));
        l.check("a", &ADDRESS).unwrap();

        for i in 1..=5 {
            assert_eq!(l.fail("u", &ACCOUNT), i == 5);
        }
        let Err(AuthError::RateLimited(d)) = l.check("u", &ACCOUNT) else {
            panic!("locked")
        };
        assert_eq!(d, Duration::from_secs(60));
        clock.advance(Duration::from_secs(61));
        l.check("u", &ACCOUNT).unwrap();
        // The next failure doubles the lockout.
        l.fail("u", &ACCOUNT);
        let Err(AuthError::RateLimited(d)) = l.check("u", &ACCOUNT) else {
            panic!("locked")
        };
        assert_eq!(d, Duration::from_secs(120));
        clock.advance(Duration::from_secs(121));
        l.succeed("u");
        l.fail("u", &ACCOUNT);
        l.check("u", &ACCOUNT).unwrap();
    }

    #[test]
    fn ipv6_keys_by_prefix() {
        let a: IpAddr = "2001:db8:1:2:3:4:5:6".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff::1".parse().unwrap();
        assert_eq!(address_key(Some(a)), address_key(Some(b)));
        let m: IpAddr = "::ffff:192.0.2.1".parse().unwrap();
        assert_eq!(address_key(Some(m)), "addr:192.0.2.1");
    }
}

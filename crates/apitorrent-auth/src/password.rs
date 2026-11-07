//! Passwords: argon2id, with a cap on concurrent hashes.

use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

use argon2::password_hash::SaltString;
use argon2::{Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version};

use crate::error::{AuthError, Result};
use crate::secret;

/// Argon2id parameters for new hashes. Existing hashes keep the parameters they were made
/// with (they are encoded in the hash) and are upgraded on the next successful sign-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasswordCost {
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

impl Default for PasswordCost {
    /// RFC 9106's second recommended option (64 MiB, 3 passes), on one lane.
    fn default() -> Self {
        Self {
            memory_kib: 64 * 1024,
            iterations: 3,
            parallelism: 1,
        }
    }
}

pub(crate) const MIN_CHARS: usize = 12;
/// Long enough for any passphrase; short enough that hashing it costs nothing extra.
pub(crate) const MAX_BYTES: usize = 1024;

pub(crate) fn validate(password: &str) -> Result<()> {
    if password.chars().count() < MIN_CHARS {
        return Err(AuthError::InvalidRequest(format!(
            "passwords need at least {MIN_CHARS} characters"
        )));
    }
    if password.len() > MAX_BYTES {
        return Err(AuthError::InvalidRequest(format!(
            "passwords can be at most {MAX_BYTES} bytes"
        )));
    }
    Ok(())
}

fn hasher(cost: PasswordCost) -> Result<Argon2<'static>> {
    let params = Params::new(cost.memory_kib, cost.iterations, cost.parallelism, None)
        .map_err(|e| AuthError::Internal(format!("argon2 parameters: {e}")))?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

pub(crate) fn hash(password: &str, cost: PasswordCost) -> Result<String> {
    let salt = SaltString::encode_b64(&secret::random::<16>())
        .map_err(|e| AuthError::Internal(format!("salt: {e}")))?;
    let hash = hasher(cost)?
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| AuthError::Internal(format!("argon2: {e}")))?;
    Ok(hash.to_string())
}

/// Whether `password` matches `phc`. Over-long input never matches (and is never hashed).
pub(crate) fn verify(password: &str, phc: &str) -> bool {
    if password.len() > MAX_BYTES {
        return false;
    }
    let Ok(parsed) = PasswordHash::new(phc) else {
        return false;
    };
    // Verification uses the parameters recorded in the hash.
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// Spend the same time as a real verification, for users without a password, so timing
/// doesn't reveal which accounts have one.
pub(crate) fn verify_dummy(password: &str, cost: PasswordCost) {
    static DUMMY: OnceLock<String> = OnceLock::new();
    let phc = DUMMY.get_or_init(|| {
        hash(&secret::token256(), cost).unwrap_or_else(|_| String::from("$argon2id$invalid"))
    });
    let _ = verify(password, phc);
}

pub(crate) fn needs_rehash(phc: &str, cost: PasswordCost) -> bool {
    let Ok(parsed) = PasswordHash::new(phc) else {
        return true;
    };
    let Ok(params) = Params::try_from(&parsed) else {
        return true;
    };
    parsed.algorithm.as_str() != "argon2id"
        || params.m_cost() != cost.memory_kib
        || params.t_cost() != cost.iterations
        || params.p_cost() != cost.parallelism
}

/// At most a few hashes at once: each one holds tens of megabytes.
#[derive(Debug)]
pub(crate) struct Gate {
    busy: Mutex<usize>,
    freed: Condvar,
    max: usize,
}

pub(crate) struct GateGuard<'a>(&'a Gate);

impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        *self.0.busy.lock().expect("gate lock") -= 1;
        self.0.freed.notify_one();
    }
}

impl Gate {
    pub(crate) fn new(max: usize) -> Self {
        Self {
            busy: Mutex::new(0),
            freed: Condvar::new(),
            max,
        }
    }

    pub(crate) fn enter(&self) -> Result<GateGuard<'_>> {
        let busy = self.busy.lock().expect("gate lock");
        let (mut busy, timeout) = self
            .freed
            .wait_timeout_while(busy, Duration::from_secs(10), |b| *b >= self.max)
            .expect("gate lock");
        if timeout.timed_out() && *busy >= self.max {
            return Err(AuthError::RateLimited(Duration::from_secs(5)));
        }
        *busy += 1;
        Ok(GateGuard(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHEAP: PasswordCost = PasswordCost {
        memory_kib: 256,
        iterations: 1,
        parallelism: 1,
    };

    #[test]
    fn hash_and_verify() {
        let h = hash("correct horse battery", CHEAP).unwrap();
        assert!(h.starts_with("$argon2id$"));
        assert!(verify("correct horse battery", &h));
        assert!(!verify("correct horse batterz", &h));
        assert!(!verify("correct horse battery", "not a hash"));
        assert!(!needs_rehash(&h, CHEAP));
        assert!(needs_rehash(&h, PasswordCost::default()));
    }

    #[test]
    fn validation() {
        assert!(validate("short").is_err());
        assert!(validate("twelve chars").is_ok());
        assert!(validate(&"x".repeat(MAX_BYTES + 1)).is_err());
        assert!(!verify(&"x".repeat(MAX_BYTES + 1), "$argon2id$"));
    }
}

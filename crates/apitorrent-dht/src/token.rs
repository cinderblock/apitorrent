//! Write tokens ([BEP 5]): a `get_peers` response hands the querier a token tied to its IP
//! address, and an `announce_peer` must return it.
//!
//! A token is a truncated SHA-1 of a secret and the querier's IP. The secret rotates every
//! five minutes and the previous one stays valid, so a token lasts between five and ten
//! minutes, as BEP 5 recommends. Nothing is stored per querier, so a flood of `get_peers`
//! costs no memory.
//!
//! [BEP 5]: https://www.bittorrent.org/beps/bep_0005.html

use std::net::IpAddr;
use std::time::{Duration, Instant};

use rand::{Rng, RngExt};
use sha1::{Digest, Sha1};

/// How often the secret rotates.
pub const ROTATE_EVERY: Duration = Duration::from_secs(5 * 60);

/// The length of the tokens we hand out.
pub const TOKEN_LEN: usize = 8;

#[derive(Debug, Clone)]
pub struct TokenSecrets {
    current: [u8; 16],
    previous: [u8; 16],
    rotated_at: Instant,
}

impl TokenSecrets {
    pub fn new(rng: &mut impl Rng, now: Instant) -> Self {
        let mut current = [0u8; 16];
        let mut previous = [0u8; 16];
        rng.fill(&mut current);
        rng.fill(&mut previous);
        Self {
            current,
            previous,
            rotated_at: now,
        }
    }

    /// Rotate the secret if it's due.
    pub fn tick(&mut self, now: Instant, rng: &mut impl Rng) {
        if now.saturating_duration_since(self.rotated_at) >= ROTATE_EVERY {
            self.previous = self.current;
            rng.fill(&mut self.current);
            self.rotated_at = now;
        }
    }

    /// The token for a querier at `ip`.
    pub fn token_for(&self, ip: IpAddr) -> [u8; TOKEN_LEN] {
        make(&self.current, ip)
    }

    /// Whether `token` is one we issued to `ip` in the last five to ten minutes.
    pub fn validate(&self, ip: IpAddr, token: &[u8]) -> bool {
        token.len() == TOKEN_LEN
            && (ct_eq(&make(&self.current, ip), token) || ct_eq(&make(&self.previous, ip), token))
    }
}

fn make(secret: &[u8; 16], ip: IpAddr) -> [u8; TOKEN_LEN] {
    let mut h = Sha1::new();
    h.update(secret);
    match ip.to_canonical() {
        IpAddr::V4(v4) => h.update(v4.octets()),
        IpAddr::V6(v6) => h.update(v6.octets()),
    }
    let digest = h.finalize();
    let mut out = [0u8; TOKEN_LEN];
    out.copy_from_slice(&digest[..TOKEN_LEN]);
    out
}

/// Compare without an early exit, so response timing doesn't reveal how much of a guessed
/// token was right.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    #[test]
    fn tokens_rotate_and_expire() {
        let mut rng = StdRng::seed_from_u64(9);
        let t0 = Instant::now();
        let mut s = TokenSecrets::new(&mut rng, t0);
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        let other: IpAddr = "1.2.3.5".parse().unwrap();
        let tok = s.token_for(ip);
        assert!(s.validate(ip, &tok));
        assert!(!s.validate(other, &tok), "bound to the IP");
        assert!(!s.validate(ip, &tok[..4]));
        assert!(!s.validate(ip, &[0u8; TOKEN_LEN]));

        // One rotation: still valid.
        s.tick(t0 + ROTATE_EVERY, &mut rng);
        assert!(s.validate(ip, &tok));
        assert_ne!(s.token_for(ip), tok);
        // Two rotations: expired.
        s.tick(t0 + ROTATE_EVERY * 2, &mut rng);
        assert!(!s.validate(ip, &tok));
    }

    #[test]
    fn mapped_v4_matches_v4() {
        let mut rng = StdRng::seed_from_u64(1);
        let s = TokenSecrets::new(&mut rng, Instant::now());
        let a: IpAddr = "1.2.3.4".parse().unwrap();
        let b: IpAddr = "::ffff:1.2.3.4".parse().unwrap();
        assert_eq!(s.token_for(a), s.token_for(b));
    }
}

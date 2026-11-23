//! The IPs we have sent to lately, so an incoming query can be judged: one from an IP we
//! haven't sent to got through on its own, which proves the port is open to the internet.
//! One from an IP we have sent to may have come back through the hole our own packet made
//! in a NAT or firewall, which proves nothing.
//!
//! NATs keep a UDP mapping for somewhere between 30 seconds and a few minutes after the
//! last packet out. Two windows of [`WINDOW`], swapped as they fill with time, cover at
//! least that long. Entries are IPs, not ports: some NATs let in anything from an IP we
//! sent to.

use std::collections::HashSet;
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// How long each window lasts; an IP is remembered for between one and two of them.
pub const WINDOW: Duration = Duration::from_secs(10 * 60);
/// Most IPs a window holds. A full window can't vouch for anything, so queries aren't
/// judged until it has rotated out.
const MAX_PER_WINDOW: usize = 1 << 16;

#[derive(Debug)]
pub struct Contacted {
    current: HashSet<IpAddr>,
    previous: HashSet<IpAddr>,
    /// Whether each window turned anyone away for lack of room.
    current_full: bool,
    previous_full: bool,
    rotate_at: Instant,
}

impl Contacted {
    pub fn new(now: Instant) -> Self {
        Self {
            current: HashSet::new(),
            previous: HashSet::new(),
            current_full: false,
            previous_full: false,
            rotate_at: now + WINDOW,
        }
    }

    /// We sent something to `ip`.
    pub fn note(&mut self, ip: IpAddr) {
        let ip = ip.to_canonical();
        if self.current.len() < MAX_PER_WINDOW {
            self.current.insert(ip);
        } else if !self.current.contains(&ip) {
            self.current_full = true;
        }
    }

    /// Whether a query from `ip` got here without our help: `Some(true)` when we haven't
    /// sent to it lately, `None` when a full window means we can't tell.
    pub fn unsolicited(&self, ip: IpAddr) -> Option<bool> {
        if self.current_full || self.previous_full {
            return None;
        }
        let ip = ip.to_canonical();
        Some(!self.current.contains(&ip) && !self.previous.contains(&ip))
    }

    /// Forget the oldest window once its time is up.
    pub fn tick(&mut self, now: Instant) {
        if now < self.rotate_at {
            return;
        }
        self.previous = std::mem::take(&mut self.current);
        self.previous_full = std::mem::take(&mut self.current_full);
        // After a long gap both windows are stale.
        if now >= self.rotate_at + WINDOW {
            self.previous.clear();
            self.previous_full = false;
        }
        self.rotate_at = now + WINDOW;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn remembers_for_one_to_two_windows() {
        let t0 = Instant::now();
        let mut c = Contacted::new(t0);
        c.note(ip("203.0.113.1"));
        assert_eq!(c.unsolicited(ip("203.0.113.1")), Some(false));
        assert_eq!(c.unsolicited(ip("203.0.113.2")), Some(true));
        c.tick(t0 + WINDOW);
        assert_eq!(
            c.unsolicited(ip("203.0.113.1")),
            Some(false),
            "still in the old window"
        );
        c.tick(t0 + WINDOW * 2);
        assert_eq!(c.unsolicited(ip("203.0.113.1")), Some(true));
    }

    #[test]
    fn a_long_gap_forgets_both_windows() {
        let t0 = Instant::now();
        let mut c = Contacted::new(t0);
        c.note(ip("203.0.113.1"));
        c.tick(t0 + WINDOW * 3);
        assert_eq!(c.unsolicited(ip("203.0.113.1")), Some(true));
    }

    #[test]
    fn mapped_and_plain_ipv4_are_the_same_ip() {
        let mut c = Contacted::new(Instant::now());
        c.note(ip("::ffff:203.0.113.1"));
        assert_eq!(c.unsolicited(ip("203.0.113.1")), Some(false));
    }

    #[test]
    fn a_full_window_cant_tell() {
        let t0 = Instant::now();
        let mut c = Contacted::new(t0);
        for i in 0..=u32::try_from(MAX_PER_WINDOW).unwrap() {
            c.note(IpAddr::from((0x0a00_0000 + i).to_be_bytes()));
        }
        assert_eq!(c.unsolicited(ip("203.0.113.9")), None);
        c.tick(t0 + WINDOW);
        assert_eq!(
            c.unsolicited(ip("203.0.113.9")),
            None,
            "the full one is still in use"
        );
        c.tick(t0 + WINDOW * 2);
        assert_eq!(c.unsolicited(ip("203.0.113.9")), Some(true));
    }
}

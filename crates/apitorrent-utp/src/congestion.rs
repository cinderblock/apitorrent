//! Congestion control: LEDBAT ([RFC 6817]) for the window, RFC 6298 for the retransmission
//! timeout.
//!
//! LEDBAT is what makes uTP a "background" transport. It measures one-way queuing delay
//! (the delay the peer reports for our packets, minus the lowest delay seen recently) and
//! steers the window so the queue it causes at the bottleneck stays near a target, 100 ms
//! by default. When other traffic fills the queue, the delay rises and uTP backs off well
//! before loss, which TCP and CUBIC only react to.
//!
//! Growth follows RFC 6817 (at most 3000 bytes per round trip, as libutp). Decrease is
//! multiplicative, as in LEDBAT++ (draft-irtf-iccrg-ledbat-plus-plus): RFC 6817's linear
//! decrease takes many round trips to drain the queue slow start builds.
//!
//! [RFC 6817]: https://www.rfc-editor.org/rfc/rfc6817

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How many one-minute minima make up the base-delay history (RFC 6817 `BASE_HISTORY`).
const BASE_HISTORY: usize = 10;

/// How many recent samples the current-delay filter takes the minimum of
/// (`CURRENT_FILTER`).
const CURRENT_FILTER: usize = 4;

/// The most the window may grow per round trip when the delay is far below target,
/// in bytes. libutp uses 3000.
const MAX_GAIN_PER_RTT: f64 = 3000.0;

/// How much of the window to give up per round trip for each target's worth of queuing
/// delay over the target.
const DECREASE_FACTOR: f64 = 0.5;

/// The smallest window, in packets (RFC 6817 `MIN_CWND`).
const MIN_CWND_PACKETS: usize = 2;

/// The initial window, in packets.
const INITIAL_CWND_PACKETS: usize = 4;

/// Signed distance between two wrapping microsecond clocks.
fn wrapping_diff(a: u32, b: u32) -> i32 {
    #[allow(clippy::cast_possible_wrap)]
    {
        a.wrapping_sub(b) as i32
    }
}

/// One-way delay tracking: the base delay (the propagation floor) and the current delay.
#[derive(Debug, Clone)]
struct DelayTracker {
    /// Per-minute minima, newest last.
    base: VecDeque<u32>,
    minute_started: Option<Instant>,
    current: VecDeque<u32>,
}

impl DelayTracker {
    fn new() -> Self {
        Self {
            base: VecDeque::with_capacity(BASE_HISTORY),
            minute_started: None,
            current: VecDeque::with_capacity(CURRENT_FILTER),
        }
    }

    /// Record a raw sample (the peer's clock minus ours, plus the one-way delay). Its
    /// absolute value is meaningless; only differences between samples are used, and all
    /// comparisons wrap.
    fn add(&mut self, now: Instant, sample: u32) {
        let rollover = self
            .minute_started
            .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(60));
        if rollover {
            if self.base.len() == BASE_HISTORY {
                self.base.pop_front();
            }
            self.base.push_back(sample);
            self.minute_started = Some(now);
        } else if let Some(last) = self.base.back_mut()
            && wrapping_diff(sample, *last) < 0
        {
            *last = sample;
        }
        if self.current.len() == CURRENT_FILTER {
            self.current.pop_front();
        }
        self.current.push_back(sample);
    }

    fn base_delay(&self) -> Option<u32> {
        self.base
            .iter()
            .copied()
            .reduce(|a, b| if wrapping_diff(b, a) < 0 { b } else { a })
    }

    /// The queuing delay: the filtered current delay over the base, in microseconds.
    fn queuing_delay(&self) -> Option<u32> {
        let base = self.base_delay()?;
        let current = self
            .current
            .iter()
            .copied()
            .reduce(|a, b| if wrapping_diff(b, a) < 0 { b } else { a })?;
        Some(u32::try_from(wrapping_diff(current, base).max(0)).unwrap_or(0))
    }
}

/// The LEDBAT congestion window.
#[derive(Debug, Clone)]
pub struct Ledbat {
    cwnd: usize,
    mss: usize,
    max_cwnd: usize,
    target_us: u32,
    slow_start: bool,
    delay: DelayTracker,
}

impl Ledbat {
    pub fn new(mss: usize, max_cwnd: usize, target: Duration) -> Self {
        let max_cwnd = max_cwnd.max(MIN_CWND_PACKETS * mss);
        Self {
            cwnd: (INITIAL_CWND_PACKETS * mss).min(max_cwnd),
            mss,
            max_cwnd,
            target_us: u32::try_from(target.as_micros()).unwrap_or(u32::MAX).max(1),
            slow_start: true,
            delay: DelayTracker::new(),
        }
    }

    /// The congestion window, in bytes.
    pub fn window(&self) -> usize {
        self.cwnd
    }

    pub fn in_slow_start(&self) -> bool {
        self.slow_start
    }

    /// The current queuing-delay estimate.
    pub fn queuing_delay(&self) -> Option<Duration> {
        self.delay
            .queuing_delay()
            .map(|us| Duration::from_micros(u64::from(us)))
    }

    /// A delay sample: the `timestamp_difference` the peer reported in a packet.
    pub fn on_delay_sample(&mut self, now: Instant, sample: u32) {
        self.delay.add(now, sample);
    }

    /// Newly acknowledged bytes. `window_limited` says whether sending was held back by the
    /// window: an application that sends less than the window allows must not grow it.
    pub fn on_ack(&mut self, bytes_acked: usize, window_limited: bool) {
        if bytes_acked == 0 {
            return;
        }
        let Some(queuing) = self.delay.queuing_delay() else {
            return;
        };
        let target = f64::from(self.target_us);
        let off_target = ((target - f64::from(queuing)) / target).clamp(-1.0, 1.0);
        if self.slow_start {
            if f64::from(queuing) > target * 0.75 {
                self.slow_start = false;
            } else if window_limited {
                self.cwnd = (self.cwnd + bytes_acked).min(self.max_cwnd);
                return;
            }
        }
        #[allow(clippy::cast_precision_loss)]
        let (acked, cwnd) = (bytes_acked as f64, self.cwnd as f64);
        let gain = if off_target >= 0.0 {
            if !window_limited {
                return;
            }
            MAX_GAIN_PER_RTT * off_target * acked.min(cwnd) / cwnd
        } else {
            // Over target: shrink in proportion to the window, as LEDBAT++ does, so a
            // queue built in slow start drains within a few round trips. At twice the
            // target the window halves each round trip.
            DECREASE_FACTOR * off_target * acked.min(cwnd)
        };
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let next = (cwnd + gain).max(0.0) as usize;
        self.cwnd = next.clamp(MIN_CWND_PACKETS * self.mss, self.max_cwnd);
    }

    /// A packet was lost (detected by SACK or duplicate ACKs): halve the window.
    pub fn on_loss(&mut self) {
        self.slow_start = false;
        self.cwnd = (self.cwnd / 2).max(MIN_CWND_PACKETS * self.mss);
    }

    /// The retransmission timer fired: start again from one packet.
    pub fn on_timeout(&mut self) {
        self.slow_start = false;
        self.cwnd = self.mss;
    }
}

/// The round-trip time estimate and the retransmission timeout (RFC 6298).
#[derive(Debug, Clone)]
pub struct Rtt {
    srtt: Option<Duration>,
    rttvar: Duration,
    rto: Duration,
    min_rto: Duration,
    max_rto: Duration,
}

impl Rtt {
    pub fn new(initial_rto: Duration, min_rto: Duration, max_rto: Duration) -> Self {
        Self {
            srtt: None,
            rttvar: Duration::ZERO,
            rto: initial_rto.clamp(min_rto, max_rto),
            min_rto,
            max_rto,
        }
    }

    pub fn srtt(&self) -> Option<Duration> {
        self.srtt
    }

    pub fn rto(&self) -> Duration {
        self.rto
    }

    /// A round-trip sample from a packet sent exactly once (Karn's rule).
    pub fn sample(&mut self, r: Duration) {
        match self.srtt {
            None => {
                self.srtt = Some(r);
                self.rttvar = r / 2;
            }
            Some(srtt) => {
                let err = srtt.abs_diff(r);
                self.rttvar = (self.rttvar * 3 + err) / 4;
                self.srtt = Some((srtt * 7 + r) / 8);
            }
        }
        let srtt = self.srtt.expect("just set");
        let granularity = Duration::from_millis(1);
        self.rto = (srtt + (self.rttvar * 4).max(granularity)).clamp(self.min_rto, self.max_rto);
    }

    /// The timer fired: back off.
    pub fn backoff(&mut self) {
        self.rto = (self.rto * 2).min(self.max_rto);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MSS: usize = 1000;

    fn ledbat() -> Ledbat {
        Ledbat::new(MSS, 1_000_000, Duration::from_millis(100))
    }

    #[test]
    fn grows_while_below_target_and_shrinks_above() {
        let now = Instant::now();
        let mut l = ledbat();
        // Base delay 1000 us (an arbitrary clock offset), no queuing.
        l.on_delay_sample(now, 1_000);
        let start = l.window();
        for _ in 0..50 {
            l.on_ack(MSS, true);
        }
        assert!(l.window() > start, "slow start grows the window");
        // 200 ms of queuing: twice the target.
        for _ in 0..CURRENT_FILTER {
            l.on_delay_sample(now, 201_000);
        }
        let high = l.window();
        for _ in 0..50 {
            l.on_ack(MSS, true);
        }
        assert!(!l.in_slow_start());
        assert!(l.window() < high, "delay over target shrinks the window");
    }

    #[test]
    fn steady_at_target() {
        let now = Instant::now();
        let mut l = ledbat();
        l.on_delay_sample(now, 5_000);
        for _ in 0..CURRENT_FILTER {
            l.on_delay_sample(now, 105_000);
        }
        let w = l.window();
        l.on_ack(MSS, true);
        assert_eq!(l.window(), w);
    }

    #[test]
    fn application_limited_does_not_grow() {
        let now = Instant::now();
        let mut l = ledbat();
        l.on_delay_sample(now, 0);
        let w = l.window();
        for _ in 0..100 {
            l.on_ack(MSS, false);
        }
        assert_eq!(l.window(), w);
    }

    #[test]
    fn delay_comparisons_wrap() {
        let now = Instant::now();
        let mut l = ledbat();
        // The peer's clock is "behind" ours: samples near u32::MAX, then wrapping past 0.
        l.on_delay_sample(now, u32::MAX - 499);
        for _ in 0..CURRENT_FILTER {
            l.on_delay_sample(now, 9_500);
        }
        assert_eq!(l.queuing_delay(), Some(Duration::from_micros(10_000)));
    }

    #[test]
    fn base_delay_forgets_old_minutes() {
        let t0 = Instant::now();
        let mut l = ledbat();
        l.on_delay_sample(t0, 1_000);
        // The path got longer: 11 minutes later the old floor has aged out.
        for m in 1..=11 {
            l.on_delay_sample(t0 + Duration::from_secs(60 * m), 50_000);
        }
        assert_eq!(l.queuing_delay(), Some(Duration::ZERO));
    }

    #[test]
    fn loss_halves_and_timeout_resets() {
        let mut l = ledbat();
        let w = l.window();
        l.on_loss();
        assert_eq!(l.window(), (w / 2).max(2 * MSS));
        l.on_timeout();
        assert_eq!(l.window(), MSS);
    }

    #[test]
    fn rto_follows_rfc_6298() {
        let mut r = Rtt::new(
            Duration::from_secs(1),
            Duration::from_millis(200),
            Duration::from_secs(60),
        );
        r.sample(Duration::from_millis(100));
        // srtt 100, rttvar 50: rto = 100 + 4 * 50 = 300.
        assert_eq!(r.rto(), Duration::from_millis(300));
        r.backoff();
        assert_eq!(r.rto(), Duration::from_millis(600));
        for _ in 0..100 {
            r.sample(Duration::from_millis(10));
        }
        assert_eq!(r.rto(), Duration::from_millis(200));
    }
}

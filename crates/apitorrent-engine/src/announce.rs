//! When to announce to which tracker (BEP 3, with BEP 12 tiers).
//!
//! Pure scheduling state; the torrent actor runs the jobs it produces.
//!
//! - Each tier announces independently. Within a tier, trackers are tried in order (shuffled
//!   once, as BEP 12 asks); one that answers moves to the front.
//! - A tier that has failed on every tracker backs off exponentially, up to
//!   [`MAX_BACKOFF`].
//! - A tier's first successful announce carries `started`. Finishing the download sends
//!   `completed` straight away; stopping sends `stopped` to the tiers that had started.
//! - A hybrid torrent is announced under both its v1 hash and its truncated v2 hash, which
//!   are separate swarms on most trackers. Each hash is a separate lane.
//! - The tiers can change while running ([`Announcer::retarget`]). A tier that keeps one of
//!   its trackers keeps its state, so a tracker that stays never sees `stopped` and
//!   `started` again; a started tier whose trackers are all gone gets `stopped`.
//! - A tier that was told `stopped` waits for that answer (or [`STOP_WAIT`]) before it
//!   announces again, so a tracker never hears the next `started` first.
//! - uploaded and downloaded are reported per tracker session, as BEP 3 defines them: the
//!   bytes since that tier's `started`, never the torrent's totals. A `started` after a
//!   stop therefore never repeats bytes the `stopped` already reported, which trackers
//!   that credit a new peer's first announce would count twice.

use std::time::{Duration, Instant};

use rand::seq::SliceRandom;

use apitorrent_metainfo::Sha1Hash;
use apitorrent_tracker::{AnnounceEvent, AnnounceResponse};

/// Never announce more often than this, whatever the tracker says.
const MIN_INTERVAL: Duration = Duration::from_secs(60);
/// Never wait longer than this between announces.
const MAX_INTERVAL: Duration = Duration::from_secs(2 * 3600);
const FIRST_BACKOFF: Duration = Duration::from_secs(60);
pub(crate) const MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);
/// How long a tier waits for the answer to its `stopped` before announcing again anyway.
pub(crate) const STOP_WAIT: Duration = Duration::from_secs(20);

/// An entry's transfer totals since it was loaded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Totals {
    pub uploaded: u64,
    pub downloaded: u64,
}

impl Totals {
    /// The bytes since `base`.
    fn since(self, base: Totals) -> Totals {
        Totals {
            uploaded: self.uploaded.saturating_sub(base.uploaded),
            downloaded: self.downloaded.saturating_sub(base.downloaded),
        }
    }
}

/// One announce to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Job {
    pub lane: usize,
    pub tier: usize,
    pub url: String,
    pub info_hash: Sha1Hash,
    pub event: AnnounceEvent,
    pub tracker_id: Option<String>,
    /// What to report: the bytes since this tracker session's `started`.
    pub report: Totals,
}

/// A tracker's latest status, for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerStatus {
    pub url: String,
    pub tier: usize,
    /// Peers in the last successful response.
    pub peers: Option<u32>,
    pub seeders: Option<u32>,
    pub leechers: Option<u32>,
    pub error: Option<String>,
    pub warning: Option<String>,
}

#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)] // independent states of one tracker session
struct Tier {
    urls: Vec<String>,
    current: usize,
    /// How many trackers in a row have failed in the current round.
    misses: usize,
    failures: u32,
    next: Instant,
    in_flight: bool,
    started: bool,
    completed_pending: bool,
    tracker_id: Option<String>,
    /// The entry's totals when this tracker session began; what it reports is counted from
    /// here. Set by the first `started`, or by the `stopped` that ended the last session.
    base: Option<Totals>,
    /// A `stopped` to `urls[current]` is under way; nothing else goes until it is answered
    /// or `next` passes.
    stopping: bool,
    status: Vec<TrackerStatus>,
}

impl Tier {
    fn new(urls: &[String], index: usize, now: Instant, rng: &mut impl rand::Rng) -> Self {
        let mut urls = urls.to_vec();
        urls.shuffle(rng);
        let status = urls.iter().map(|u| fresh_status(u, index)).collect();
        Self {
            urls,
            current: 0,
            misses: 0,
            failures: 0,
            next: now,
            in_flight: false,
            started: false,
            completed_pending: false,
            tracker_id: None,
            base: None,
            stopping: false,
            status,
        }
    }

    /// Take over an old tier's state, for the trackers this one keeps of it.
    fn adopt(&mut self, prev: Tier) {
        let index = self.status.first().map_or(0, |s| s.tier);
        // The trackers both list, in the old order (the working one first), then new ones.
        let mut urls: Vec<String> = prev
            .urls
            .iter()
            .filter(|u| self.urls.contains(u))
            .cloned()
            .collect();
        for u in &self.urls {
            if !urls.contains(u) {
                urls.push(u.clone());
            }
        }
        // An announce (or a `stopped`) still under way to a tracker that stays is this
        // tier's to finish.
        let flying = (prev.in_flight || prev.stopping)
            .then(|| prev.urls[prev.current].clone())
            .filter(|u| urls.contains(u));
        self.current = flying
            .as_ref()
            .and_then(|f| urls.iter().position(|u| u == f))
            .unwrap_or(0);
        self.in_flight = flying.is_some() && prev.in_flight;
        self.stopping = flying.is_some() && prev.stopping;
        self.base = prev.base;
        self.status = urls
            .iter()
            .map(|u| {
                prev.status.iter().find(|s| &s.url == u).map_or_else(
                    || fresh_status(u, index),
                    |s| TrackerStatus {
                        tier: index,
                        ..s.clone()
                    },
                )
            })
            .collect();
        self.urls = urls;
        self.failures = prev.failures;
        self.next = prev.next;
        self.started = prev.started;
        self.completed_pending = prev.completed_pending;
        self.tracker_id = prev.tracker_id;
    }
}

fn fresh_status(url: &str, tier: usize) -> TrackerStatus {
    TrackerStatus {
        url: url.to_owned(),
        tier,
        peers: None,
        seeders: None,
        leechers: None,
        error: None,
        warning: None,
    }
}

#[derive(Debug)]
struct Lane {
    info_hash: Sha1Hash,
    tiers: Vec<Tier>,
}

#[derive(Debug)]
pub(crate) struct Announcer {
    lanes: Vec<Lane>,
}

impl Announcer {
    pub(crate) fn new(trackers: &[Vec<String>], hashes: &[Sha1Hash], now: Instant) -> Self {
        let mut rng = rand::rng();
        let lanes = hashes
            .iter()
            .map(|&info_hash| Lane {
                info_hash,
                tiers: trackers
                    .iter()
                    .enumerate()
                    .filter(|(_, urls)| !urls.is_empty())
                    .map(|(i, urls)| Tier::new(urls, i, now, &mut rng))
                    .collect(),
            })
            .collect();
        Self { lanes }
    }

    /// The announces that are due now. Each is marked in flight until its result comes back.
    #[cfg(test)]
    pub(crate) fn due(&mut self, now: Instant, totals: Totals) -> Vec<Job> {
        self.due_where(now, totals, |_| true)
    }

    /// The announces that are due now to trackers `allowed` accepts. A tier whose current
    /// tracker it refuses (another entry of the torrent announces there) waits, unchanged.
    /// `totals` are the entry's, for the stats each announce reports.
    pub(crate) fn due_where(
        &mut self,
        now: Instant,
        totals: Totals,
        allowed: impl Fn(&str) -> bool,
    ) -> Vec<Job> {
        let mut jobs = Vec::new();
        for (lane_index, lane) in self.lanes.iter_mut().enumerate() {
            for (tier_index, tier) in lane.tiers.iter_mut().enumerate() {
                if tier.stopping {
                    if tier.next > now {
                        continue;
                    }
                    // The `stopped` went unanswered: carry on without it.
                    tier.stopping = false;
                }
                if tier.in_flight || tier.next > now || !allowed(&tier.urls[tier.current]) {
                    continue;
                }
                let event = if !tier.started {
                    AnnounceEvent::Started
                } else if tier.completed_pending {
                    AnnounceEvent::Completed
                } else {
                    AnnounceEvent::None
                };
                let base = *tier.base.get_or_insert(totals);
                tier.in_flight = true;
                jobs.push(Job {
                    lane: lane_index,
                    tier: tier_index,
                    url: tier.urls[tier.current].clone(),
                    info_hash: lane.info_hash,
                    event,
                    tracker_id: tier.tracker_id.clone(),
                    report: totals.since(base),
                });
            }
        }
        jobs
    }

    pub(crate) fn on_success(&mut self, job: &Job, resp: &AnnounceResponse, now: Instant) {
        let Some(tier) = self.tier(job) else { return };
        if job.event == AnnounceEvent::Stopped {
            tier.stopping = false;
            tier.next = now;
            return;
        }
        tier.in_flight = false;
        tier.misses = 0;
        tier.failures = 0;
        match job.event {
            AnnounceEvent::Started => tier.started = true,
            AnnounceEvent::Completed => tier.completed_pending = false,
            _ => {}
        }
        if resp.tracker_id.is_some() {
            tier.tracker_id.clone_from(&resp.tracker_id);
        }
        // BEP 12: a tracker that answers moves to the front of its tier.
        if let Some(pos) = tier.urls.iter().position(|u| *u == job.url) {
            let url = tier.urls.remove(pos);
            tier.urls.insert(0, url);
        }
        tier.current = 0;
        let wait = resp
            .interval
            .max(resp.min_interval.unwrap_or_default())
            .clamp(MIN_INTERVAL, MAX_INTERVAL);
        tier.next = now + wait;
        if let Some(s) = tier.status.iter_mut().find(|s| s.url == job.url) {
            s.peers = Some(u32::try_from(resp.peers.len()).unwrap_or(u32::MAX));
            s.seeders = resp.seeders;
            s.leechers = resp.leechers;
            s.error = None;
            s.warning.clone_from(&resp.warning);
        }
    }

    pub(crate) fn on_failure(&mut self, job: &Job, error: &str, now: Instant) {
        let Some(tier) = self.tier(job) else { return };
        if job.event == AnnounceEvent::Stopped {
            // The tracker drops the old session when it times out; carry on.
            tier.stopping = false;
            tier.next = now;
            return;
        }
        tier.in_flight = false;
        if let Some(s) = tier.status.iter_mut().find(|s| s.url == job.url) {
            s.error = Some(error.to_owned());
        }
        tier.misses += 1;
        tier.current = (tier.current + 1) % tier.urls.len();
        if tier.misses < tier.urls.len() {
            // Try the next tracker in the tier straight away.
            tier.next = now;
            return;
        }
        // Every tracker in the tier failed: back off before another round.
        tier.misses = 0;
        let backoff = FIRST_BACKOFF
            .saturating_mul(1 << tier.failures.min(10))
            .min(MAX_BACKOFF);
        tier.failures += 1;
        tier.next = now + backoff;
    }

    /// The download just finished: tell every tier that has started, right away.
    pub(crate) fn completed(&mut self, now: Instant) {
        for tier in self.lanes.iter_mut().flat_map(|l| l.tiers.iter_mut()) {
            if tier.started {
                tier.completed_pending = true;
                tier.next = now;
            }
        }
    }

    /// `stopped` announces for the tiers that had started (or have a `started` under way),
    /// and a reset so a later resume starts a new tracker session: its `started` waits for
    /// the `stopped` to be answered, carries no `trackerid`, and counts bytes from
    /// `totals`. Pass each job's result to [`Announcer::on_success`] or
    /// [`Announcer::on_failure`].
    pub(crate) fn stop(&mut self, now: Instant, totals: Totals) -> Vec<Job> {
        let mut jobs = Vec::new();
        for (lane_index, lane) in self.lanes.iter_mut().enumerate() {
            for (tier_index, tier) in lane.tiers.iter_mut().enumerate() {
                if tier.stopping {
                    // Already stopped, and waiting for that answer.
                } else if tier.started || tier.in_flight {
                    jobs.push(Job {
                        lane: lane_index,
                        tier: tier_index,
                        url: tier.urls[tier.current].clone(),
                        info_hash: lane.info_hash,
                        event: AnnounceEvent::Stopped,
                        tracker_id: tier.tracker_id.clone(),
                        report: totals.since(tier.base.unwrap_or(totals)),
                    });
                    tier.stopping = true;
                    tier.next = now + STOP_WAIT;
                } else {
                    tier.next = now;
                }
                tier.started = false;
                tier.completed_pending = false;
                tier.in_flight = false;
                tier.tracker_id = None;
                tier.base = Some(totals);
            }
        }
        jobs
    }

    /// Status of every tracker in the first lane (the others list the same trackers).
    pub(crate) fn status(&self) -> Vec<TrackerStatus> {
        self.lanes
            .first()
            .map(|l| {
                l.tiers
                    .iter()
                    .flat_map(|t| t.status.iter().cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Change the tiers. A new tier that lists a tracker of an old one takes over that
    /// tier's state (started, tracker id, schedule, an announce under way, the statuses of
    /// the trackers it keeps). Returns `stopped` announces for the old tiers that had
    /// started and whose trackers are all gone; nothing waits for their results.
    pub(crate) fn retarget(
        &mut self,
        trackers: &[Vec<String>],
        now: Instant,
        totals: Totals,
    ) -> Vec<Job> {
        let mut rng = rand::rng();
        let mut stops = Vec::new();
        for (lane_index, lane) in self.lanes.iter_mut().enumerate() {
            let mut old: Vec<Option<Tier>> = std::mem::take(&mut lane.tiers)
                .into_iter()
                .map(Some)
                .collect();
            for (i, urls) in trackers.iter().enumerate().filter(|(_, u)| !u.is_empty()) {
                let mut tier = Tier::new(urls, i, now, &mut rng);
                let prev = old
                    .iter_mut()
                    .find(|t| {
                        t.as_ref()
                            .is_some_and(|t| t.urls.iter().any(|u| urls.contains(u)))
                    })
                    .and_then(Option::take);
                if let Some(prev) = prev {
                    tier.adopt(prev);
                }
                lane.tiers.push(tier);
            }
            for (tier_index, t) in old.into_iter().enumerate() {
                let Some(t) = t.filter(|t| (t.started || t.in_flight) && !t.stopping) else {
                    continue;
                };
                stops.push(Job {
                    lane: lane_index,
                    tier: tier_index,
                    url: t.urls[t.current].clone(),
                    info_hash: lane.info_hash,
                    event: AnnounceEvent::Stopped,
                    tracker_id: t.tracker_id,
                    report: totals.since(t.base.unwrap_or(totals)),
                });
            }
        }
        stops
    }

    /// The tier waiting for this job's answer. Tiers move when they are retargeted, and a
    /// stop forgets announces under way, so it is found by what it waits for, not by its
    /// index; an answer nothing waits for is ignored.
    fn tier(&mut self, job: &Job) -> Option<&mut Tier> {
        let lane = self.lanes.get_mut(job.lane)?;
        let stop = job.event == AnnounceEvent::Stopped;
        let waiting = |t: &Tier| {
            (if stop { t.stopping } else { t.in_flight }) && t.urls[t.current] == job.url
        };
        let at = if lane.tiers.get(job.tier).is_some_and(waiting) {
            job.tier
        } else {
            lane.tiers.iter().position(waiting)?
        };
        lane.tiers.get_mut(at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Totals = Totals {
        uploaded: 0,
        downloaded: 0,
    };

    fn response(interval: u64) -> AnnounceResponse {
        AnnounceResponse {
            interval: Duration::from_secs(interval),
            min_interval: None,
            peers: Vec::new(),
            seeders: Some(1),
            leechers: Some(2),
            tracker_id: None,
            warning: None,
            external_ip: None,
        }
    }

    #[test]
    fn lifecycle_events() {
        let t0 = Instant::now();
        let mut a = Announcer::new(
            &[vec!["http://a/announce".into()]],
            &[Sha1Hash([1; 20])],
            t0,
        );
        let jobs = a.due(t0, NONE);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].event, AnnounceEvent::Started);
        assert!(a.due(t0, NONE).is_empty(), "in flight");
        a.on_success(&jobs[0], &response(1800), t0);
        assert!(a.due(t0 + Duration::from_secs(1799), NONE).is_empty());
        let jobs = a.due(t0 + Duration::from_secs(1800), NONE);
        assert_eq!(jobs[0].event, AnnounceEvent::None);
        a.on_success(&jobs[0], &response(1800), t0);

        a.completed(t0);
        let jobs = a.due(t0, NONE);
        assert_eq!(jobs[0].event, AnnounceEvent::Completed);
        a.on_success(&jobs[0], &response(1800), t0);

        let stops = a.stop(t0, NONE);
        assert_eq!(stops.len(), 1);
        assert_eq!(stops[0].event, AnnounceEvent::Stopped);
        // After a stop, the next announce starts again, once the stop is answered.
        assert!(a.due(t0, NONE).is_empty(), "waits for the stop's answer");
        a.on_success(&stops[0], &response(1800), t0);
        assert_eq!(a.due(t0, NONE)[0].event, AnnounceEvent::Started);
    }

    fn totals(uploaded: u64, downloaded: u64) -> Totals {
        Totals {
            uploaded,
            downloaded,
        }
    }

    #[test]
    fn a_started_waits_for_the_stopped_before_it() {
        let t0 = Instant::now();
        let mut a = Announcer::new(&[urls(&["http://a"])], &[Sha1Hash([1; 20])], t0);
        let start = a.due(t0, NONE).remove(0);
        a.on_success(&start, &response(1800), t0);
        let stop = a.stop(t0, NONE).remove(0);
        assert!(a.stop(t0, NONE).is_empty(), "one stop is enough");
        // A failed stop releases the tier too.
        a.on_failure(&stop, "refused", t0);
        assert_eq!(a.due(t0, NONE)[0].event, AnnounceEvent::Started);

        // An unanswered stop holds the tier for STOP_WAIT, then gives up.
        let mut b = Announcer::new(&[urls(&["http://a"])], &[Sha1Hash([1; 20])], t0);
        let start = b.due(t0, NONE).remove(0);
        b.on_success(&start, &response(1800), t0);
        let stop = b.stop(t0, NONE).remove(0);
        assert!(
            b.due(t0 + STOP_WAIT.saturating_sub(Duration::from_secs(1)), NONE)
                .is_empty()
        );
        let late = b.due(t0 + STOP_WAIT, NONE).remove(0);
        assert_eq!(late.event, AnnounceEvent::Started);
        // The stop's late answer then changes nothing.
        b.on_success(&stop, &response(1800), t0 + STOP_WAIT);
        assert!(
            b.due(t0 + STOP_WAIT, NONE).is_empty(),
            "the start is in flight"
        );

        // A `started` still under way when stopping gets a `stopped` too, and its own
        // late answer is ignored.
        let mut c = Announcer::new(&[urls(&["http://a"])], &[Sha1Hash([1; 20])], t0);
        let start = c.due(t0, NONE).remove(0);
        let stop = c.stop(t0, NONE).remove(0);
        c.on_success(&start, &response(1800), t0);
        assert!(c.due(t0, NONE).is_empty());
        c.on_success(&stop, &response(1800), t0);
        assert_eq!(c.due(t0, NONE)[0].event, AnnounceEvent::Started);
    }

    #[test]
    fn stats_are_per_tracker_session() {
        let t0 = Instant::now();
        let mut a = Announcer::new(&[urls(&["http://a"])], &[Sha1Hash([1; 20])], t0);
        // Bytes before the first `started` aren't this session's.
        let start = a.due(t0, totals(100, 50)).remove(0);
        assert_eq!(start.report, NONE);
        a.on_success(&start, &response(60), t0);
        let regular = a.due(t0 + MIN_INTERVAL, totals(400, 80)).remove(0);
        assert_eq!(regular.report, totals(300, 30));
        a.on_success(&regular, &response(60), t0);
        let stop = a.stop(t0, totals(1000, 90)).remove(0);
        assert_eq!(stop.report, totals(900, 40));
        a.on_success(&stop, &response(60), t0);
        // The next session counts from the stop: nothing is reported twice, and bytes
        // between the stop and the next `started` are the new session's.
        let restart = a.due(t0, totals(1010, 90)).remove(0);
        assert_eq!(restart.event, AnnounceEvent::Started);
        assert_eq!(restart.report, totals(10, 0));
    }

    #[test]
    fn a_stop_forgets_the_tracker_id() {
        let t0 = Instant::now();
        let mut a = Announcer::new(&[urls(&["http://a"])], &[Sha1Hash([1; 20])], t0);
        let start = a.due(t0, NONE).remove(0);
        let resp = AnnounceResponse {
            tracker_id: Some("abc".into()),
            ..response(60)
        };
        a.on_success(&start, &resp, t0);
        let stop = a.stop(t0, NONE).remove(0);
        assert_eq!(stop.tracker_id.as_deref(), Some("abc"), "the old session's");
        a.on_success(&stop, &response(60), t0);
        assert_eq!(a.due(t0, NONE)[0].tracker_id, None);
    }

    #[test]
    fn tiny_intervals_are_clamped() {
        let t0 = Instant::now();
        let mut a = Announcer::new(&[vec!["http://a".into()]], &[Sha1Hash([1; 20])], t0);
        let jobs = a.due(t0, NONE);
        a.on_success(&jobs[0], &response(1), t0);
        assert!(a.due(t0 + Duration::from_secs(59), NONE).is_empty());
        assert_eq!(a.due(t0 + MIN_INTERVAL, NONE).len(), 1);
    }

    #[test]
    fn failover_within_a_tier_then_backoff() {
        let t0 = Instant::now();
        let urls = vec!["http://a".to_owned(), "http://b".to_owned()];
        let mut a = Announcer::new(&[urls], &[Sha1Hash([1; 20])], t0);
        let first = a.due(t0, NONE).remove(0);
        a.on_failure(&first, "refused", t0);
        // The other tracker is tried immediately.
        let second = a.due(t0, NONE).remove(0);
        assert_ne!(second.url, first.url);
        a.on_failure(&second, "refused", t0);
        // Both failed: wait.
        assert!(a.due(t0, NONE).is_empty());
        assert_eq!(a.due(t0 + FIRST_BACKOFF, NONE).len(), 1);
        let status = a.status();
        assert!(status.iter().all(|s| s.error.is_some()));
    }

    #[test]
    fn working_tracker_moves_to_front() {
        let t0 = Instant::now();
        let urls = vec!["http://a".to_owned(), "http://b".to_owned()];
        let mut a = Announcer::new(&[urls], &[Sha1Hash([1; 20])], t0);
        let first = a.due(t0, NONE).remove(0);
        a.on_failure(&first, "down", t0);
        let second = a.due(t0, NONE).remove(0);
        a.on_success(&second, &response(60), t0);
        let next = a.due(t0 + MIN_INTERVAL, NONE).remove(0);
        assert_eq!(next.url, second.url);
    }

    fn urls(v: &[&str]) -> Vec<String> {
        v.iter().map(|&u| u.to_owned()).collect()
    }

    #[test]
    fn retargeting_keeps_trackers_that_stay() {
        let t0 = Instant::now();
        let mut a = Announcer::new(
            &[urls(&["http://a"]), urls(&["http://b"])],
            &[Sha1Hash([1; 20])],
            t0,
        );
        let jobs = a.due(t0, NONE);
        for j in &jobs {
            a.on_success(j, &response(1800), t0);
        }
        // Drop b, keep a (now the second tier), add c in front.
        let stops = a.retarget(&[urls(&["udp://c"]), urls(&["http://a"])], t0, NONE);
        assert_eq!(stops.len(), 1);
        assert_eq!(stops[0].url, "http://b");
        assert_eq!(stops[0].event, AnnounceEvent::Stopped);
        // c starts now; a waits for its interval, and isn't `started` again.
        let due = a.due(t0, NONE);
        assert_eq!(due.len(), 1);
        assert_eq!(
            (due[0].url.as_str(), due[0].event),
            ("udp://c", AnnounceEvent::Started)
        );
        let later = a.due(t0 + Duration::from_secs(1800), NONE);
        assert_eq!(later.len(), 1);
        assert_eq!(
            (later[0].url.as_str(), later[0].event),
            ("http://a", AnnounceEvent::None)
        );
        let status = a.status();
        assert_eq!(status.len(), 2);
        let kept = status.iter().find(|s| s.url == "http://a").unwrap();
        assert_eq!((kept.tier, kept.seeders), (1, Some(1)), "its status stays");
    }

    #[test]
    fn an_answer_under_way_survives_retargeting() {
        let t0 = Instant::now();
        let mut a = Announcer::new(&[urls(&["http://a"])], &[Sha1Hash([1; 20])], t0);
        let job = a.due(t0, NONE).remove(0);
        // a moves to the second tier while its announce is under way.
        assert!(
            a.retarget(&[urls(&["http://z"]), urls(&["http://a"])], t0, NONE)
                .is_empty()
        );
        let due = a.due(t0, NONE);
        assert_eq!(due.len(), 1, "only z: a is still in flight");
        a.on_success(&job, &response(1800), t0);
        assert_eq!(
            a.status()
                .iter()
                .find(|s| s.url == "http://a")
                .unwrap()
                .peers,
            Some(0)
        );
        // An answer from a tracker that is gone is ignored.
        let gone = Job {
            url: "http://gone".into(),
            ..job
        };
        a.on_failure(&gone, "late", t0);
        assert!(a.status().iter().all(|s| s.error.is_none()));
    }

    #[test]
    fn hybrid_announces_both_hashes() {
        let t0 = Instant::now();
        let mut a = Announcer::new(
            &[vec!["udp://a".into()], vec!["udp://b".into()]],
            &[Sha1Hash([1; 20]), Sha1Hash([2; 20])],
            t0,
        );
        let jobs = a.due(t0, NONE);
        assert_eq!(jobs.len(), 4);
        assert_eq!(
            jobs.iter()
                .filter(|j| j.info_hash == Sha1Hash([2; 20]))
                .count(),
            2
        );
    }
}

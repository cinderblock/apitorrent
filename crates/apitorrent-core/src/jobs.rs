//! Background jobs: work too long for a request to wait on (a Deluge import, a take-over).
//! A job runs on its own task; requests read its progress and, once it ends, its outcome.
//! Jobs live in memory only: a restart forgets them (the work they finished stays done).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use apitorrent_types::spaces::SpaceId;
use apitorrent_types::{Job, JobKind, JobOutcome, JobProgress};

/// Finished jobs kept for reading back, oldest dropped first.
const KEEP_FINISHED: usize = 32;

#[derive(Default)]
pub(crate) struct Jobs {
    inner: Mutex<State>,
}

#[derive(Default)]
struct State {
    next: u64,
    jobs: BTreeMap<u64, Job>,
}

impl Jobs {
    /// A new running job of `kind` in `space`, unless one of that kind already runs there.
    pub(crate) fn start(self: &Arc<Self>, kind: JobKind, space: SpaceId) -> Option<Progress> {
        let mut s = self.inner.lock().expect("jobs lock poisoned");
        if s.jobs
            .values()
            .any(|j| j.kind == kind && j.space == space && j.outcome.is_none())
        {
            return None;
        }
        s.next += 1;
        let id = s.next;
        s.jobs.insert(
            id,
            Job {
                id,
                kind,
                space,
                started_at: now_ms(),
                finished_at: None,
                progress: JobProgress::default(),
                outcome: None,
            },
        );
        Some(Progress {
            jobs: Some(Arc::clone(self)),
            id,
        })
    }

    pub(crate) fn get(&self, id: u64) -> Option<Job> {
        self.inner
            .lock()
            .expect("jobs lock poisoned")
            .jobs
            .get(&id)
            .cloned()
    }

    /// The space's jobs, oldest first.
    pub(crate) fn in_space(&self, space: SpaceId) -> Vec<Job> {
        self.inner
            .lock()
            .expect("jobs lock poisoned")
            .jobs
            .values()
            .filter(|j| j.space == space)
            .cloned()
            .collect()
    }

    fn update(&self, id: u64, f: impl FnOnce(&mut Job)) {
        let mut s = self.inner.lock().expect("jobs lock poisoned");
        if let Some(j) = s.jobs.get_mut(&id) {
            f(j);
        }
    }

    fn finish(&self, id: u64, outcome: JobOutcome) {
        let mut s = self.inner.lock().expect("jobs lock poisoned");
        if let Some(j) = s.jobs.get_mut(&id) {
            j.finished_at = Some(now_ms());
            j.progress.current = None;
            j.outcome = Some(outcome);
        }
        let finished: Vec<u64> = s
            .jobs
            .values()
            .filter(|j| j.outcome.is_some())
            .map(|j| j.id)
            .collect();
        for old in finished
            .iter()
            .take(finished.len().saturating_sub(KEEP_FINISHED))
        {
            s.jobs.remove(old);
        }
    }
}

/// Reports one job's progress. `Progress::none()` reports nowhere (a caller that waits for
/// the result anyway). Dropped without finishing (a panic), the job ends as failed.
pub struct Progress {
    jobs: Option<Arc<Jobs>>,
    id: u64,
}

impl std::fmt::Debug for Progress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Progress")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Progress {
    #[must_use]
    pub fn none() -> Self {
        Self { jobs: None, id: 0 }
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// Start a phase: what it does, and how many steps the whole job now has (0: unknown).
    pub fn phase(&self, phase: &str, total: u32) {
        if let Some(jobs) = &self.jobs {
            jobs.update(self.id, |j| {
                phase.clone_into(&mut j.progress.phase);
                j.progress.total = total;
                j.progress.current = None;
            });
        }
    }

    /// One more step done.
    pub fn step(&self, current: Option<&str>) {
        self.steps(1, current);
    }

    /// `n` more steps done; `current` is what comes next.
    pub fn steps(&self, n: u32, current: Option<&str>) {
        if let Some(jobs) = &self.jobs {
            jobs.update(self.id, |j| {
                j.progress.done = j.progress.done.saturating_add(n);
                j.progress.current = current.map(str::to_owned);
            });
        }
    }

    /// What is being worked on now, without counting a step.
    pub fn working_on(&self, current: &str) {
        if let Some(jobs) = &self.jobs {
            jobs.update(self.id, |j| j.progress.current = Some(current.to_owned()));
        }
    }

    pub(crate) fn finish(mut self, outcome: JobOutcome) {
        if let Some(jobs) = self.jobs.take() {
            jobs.update(self.id, |j| {
                j.progress.done = j.progress.done.max(j.progress.total);
            });
            jobs.finish(self.id, outcome);
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        if let Some(jobs) = self.jobs.take() {
            jobs.finish(
                self.id,
                JobOutcome::Failed {
                    message: "the job stopped unexpectedly".into(),
                },
            );
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_running_job_of_a_kind_per_space() {
        let jobs = Arc::new(Jobs::default());
        let p = jobs.start(JobKind::DelugeImport, 1).unwrap();
        assert!(jobs.start(JobKind::DelugeImport, 1).is_none());
        assert!(jobs.start(JobKind::DelugeImport, 2).is_some());
        assert!(jobs.start(JobKind::TakeOver, 1).is_some());
        p.phase("Adding", 4);
        p.step(Some("b"));
        let j = jobs.get(1).unwrap();
        assert_eq!((j.progress.done, j.progress.total), (1, 4));
        assert_eq!(j.progress.current.as_deref(), Some("b"));
        p.finish(JobOutcome::Failed {
            message: "x".into(),
        });
        let j = jobs.get(1).unwrap();
        assert_eq!(j.progress.done, 4);
        assert!(j.finished_at.is_some());
        assert!(jobs.start(JobKind::DelugeImport, 1).is_some());
    }

    #[test]
    fn a_dropped_job_fails() {
        let jobs = Arc::new(Jobs::default());
        drop(jobs.start(JobKind::TakeOver, 1).unwrap());
        assert!(matches!(
            jobs.get(1).unwrap().outcome,
            Some(JobOutcome::Failed { .. })
        ));
    }

    #[test]
    fn old_finished_jobs_are_dropped() {
        let jobs = Arc::new(Jobs::default());
        for _ in 0..KEEP_FINISHED + 5 {
            jobs.start(JobKind::TakeOver, 1)
                .unwrap()
                .finish(JobOutcome::Failed {
                    message: String::new(),
                });
        }
        assert_eq!(jobs.in_space(1).len(), KEEP_FINISHED);
        assert!(jobs.get(1).is_none());
    }
}

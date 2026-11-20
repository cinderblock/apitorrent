// Background jobs the server runs for this browser tab (a Deluge import, a take-over): the
// tab follows each one it started, or found running, until it ends and is dismissed. Any
// page shows them (see JobsTray), so starting one never ties up the UI.

import { useSyncExternalStore } from "react";

import { ApiRequestError, requestIn } from "./api/client";
import type { Job } from "./api/generated/Job";

/** A job this tab follows. `slug`: the space it runs in (`null` only where spaces are unknown). */
export interface TrackedJob {
  slug: string | null;
  job: Job;
  /** Whether the import added its torrents paused (so they can be moved over after). */
  paused?: boolean;
}

/** How often a running job's progress is read. */
const POLL_MS = 500;

let jobs: TrackedJob[] = [];
const listeners = new Set<() => void>();
let timer: ReturnType<typeof setTimeout> | null = null;

function emit() {
  for (const l of listeners) l();
}

function subscribe(l: () => void) {
  listeners.add(l);
  return () => listeners.delete(l);
}

function set(next: TrackedJob[]) {
  jobs = next;
  emit();
  schedule();
}

function same(a: TrackedJob, slug: string | null, id: number) {
  return a.slug === slug && a.job.id === id;
}

/** Follow `job` (just started, or found running) until it ends. */
export function trackJob(slug: string | null, job: Job, extra: Omit<TrackedJob, "slug" | "job"> = {}): void {
  const others = jobs.filter((t) => !same(t, slug, job.id));
  set([...others, { slug, job, ...extra }]);
}

/** Stop showing a finished job. */
export function dismissJob(slug: string | null, id: number): void {
  set(jobs.filter((t) => !same(t, slug, id)));
}

/**
 * Follow the running jobs of the space `slug` (`null`: the page's), such as one another tab
 * started. For its admins: nobody else can see them.
 */
export async function discoverJobs(slug: string | null): Promise<void> {
  let found: Job[];
  try {
    found = await requestIn<Job[]>(slug, "GET", "/jobs");
  } catch {
    return;
  }
  for (const job of found) {
    if (job.outcome === null && !jobs.some((t) => same(t, slug, job.id))) trackJob(slug, job);
  }
}

/** The jobs followed now. */
export function currentJobs(): TrackedJob[] {
  return jobs;
}

export function useJobs(): TrackedJob[] {
  return useSyncExternalStore(subscribe, currentJobs);
}

/** One followed job, or `undefined`. */
export function useJob(slug: string | null, id: number | null): TrackedJob | undefined {
  const all = useJobs();
  return id === null ? undefined : all.find((t) => same(t, slug, id));
}

export function running(t: TrackedJob): boolean {
  return t.job.outcome === null;
}

function schedule() {
  if (timer !== null || !jobs.some(running)) return;
  timer = setTimeout(() => {
    timer = null;
    void poll();
  }, POLL_MS);
}

async function poll() {
  const updated = await Promise.all(
    jobs.filter(running).map(async (t): Promise<TrackedJob> => {
      try {
        return { ...t, job: await requestIn<Job>(t.slug, "GET", `/jobs/${t.job.id}`) };
      } catch (e) {
        // Gone: the server restarted (jobs live in its memory) or the space went away.
        if (e instanceof ApiRequestError && e.status === 404) {
          const message = "The server restarted, so how this ended is unknown. Whatever it finished is kept.";
          return { ...t, job: { ...t.job, finishedAt: Date.now(), outcome: { kind: "failed", message } } };
        }
        // Can't reach the server for now: try again next time.
        return t;
      }
    }),
  );
  set(jobs.map((t) => updated.find((u) => same(u, t.slug, t.job.id)) ?? t));
}

/** For tests: forget every job. */
export function resetJobs(): void {
  if (timer !== null) clearTimeout(timer);
  timer = null;
  set([]);
}

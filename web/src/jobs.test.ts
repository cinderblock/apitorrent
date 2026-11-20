import { afterEach, beforeEach, expect, test } from "bun:test";

import type { Job } from "./api/generated/Job";
import { setTransport } from "./api/transport";
import { currentJobs as read, discoverJobs, dismissJob, resetJobs, running, trackJob } from "./jobs";

function job(id: number, done: number, outcome: Job["outcome"] = null): Job {
  return {
    id,
    kind: "delugeImport",
    space: 1,
    startedAt: 0,
    finishedAt: outcome ? 1 : null,
    progress: { phase: "Adding torrents", done, total: 10, current: null },
    outcome,
  };
}

/** Answers for GET /…/jobs/{id}, by id; a missing id answers 404. */
let server: Map<number, Job>;
let listed: Job[];
const requested: string[] = [];

beforeEach(() => {
  server = new Map();
  listed = [];
  requested.length = 0;
  setTransport({
    fetch: (url) => {
      requested.push(url);
      if (url.endsWith("/jobs")) return Promise.resolve(Response.json(listed));
      const id = Number(url.split("/").pop());
      const j = server.get(id);
      return Promise.resolve(j ? Response.json(j) : Response.json({ code: "not_found", message: "no such job" }, { status: 404 }));
    },
    openSocket: () => {
      throw new Error("no sockets here");
    },
  });
});

afterEach(() => resetJobs());

const tick = () => new Promise((r) => setTimeout(r, 650));

test("a tracked job is polled in its own space until it ends", async () => {
  server.set(7, job(7, 4));
  trackJob("tv", job(7, 0), { paused: true });
  expect(read()[0]!.job.progress.done).toBe(0);
  await tick();
  expect(read()[0]!.job.progress.done).toBe(4);
  expect(requested.at(-1)).toBe("/s/tv/api/v1/jobs/7");
  server.set(7, job(7, 10, { kind: "failed", message: "boom" }));
  await tick();
  expect(running(read()[0]!)).toBe(false);
  expect(read()[0]!.paused).toBe(true);
  // Ended: no more polling.
  const before = requested.length;
  await tick();
  expect(requested.length).toBe(before);
  dismissJob("tv", 7);
  expect(read()).toEqual([]);
});

test("a job the server forgot (it restarted) ends as failed", async () => {
  trackJob(null, job(3, 2));
  await tick();
  const outcome = read()[0]!.job.outcome;
  expect(outcome?.kind).toBe("failed");
});

test("running jobs found in the space are followed; finished ones aren't", async () => {
  listed = [job(1, 1), job(2, 10, { kind: "failed", message: "x" })];
  await discoverJobs("tv");
  expect(read().map((t) => [t.slug, t.job.id])).toEqual([["tv", 1]]);
  // Found again: not followed twice.
  await discoverJobs("tv");
  expect(read().length).toBe(1);
});

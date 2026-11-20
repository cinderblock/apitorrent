import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";

import type { Job } from "../api/generated/Job";
import { JobProgress, jobSummary } from "./JobProgress";

function job(progress: Partial<Job["progress"]>, outcome: Job["outcome"] = null): Job {
  return {
    id: 1,
    kind: "delugeImport",
    space: 1,
    startedAt: 0,
    finishedAt: null,
    progress: { phase: "Adding torrents", done: 0, total: 0, current: null, ...progress },
    outcome,
  };
}

describe("a job's progress", () => {
  test("counts steps once the size of the work is known", () => {
    const html = renderToStaticMarkup(<JobProgress job={job({ done: 30, total: 120, current: "ubuntu.iso" })} />);
    expect(html).toContain('role="progressbar"');
    expect(html).toContain('aria-valuenow="30"');
    expect(html).toContain('aria-valuemax="120"');
    expect(html).toContain("width:25%");
    expect(html).toContain("30 of 120");
    expect(html).toContain("Adding torrents…");
    expect(html).toContain("ubuntu.iso");
    expect(html).not.toContain("progress-indeterminate");
  });

  test("sweeps while the size isn't known", () => {
    const html = renderToStaticMarkup(<JobProgress job={job({ phase: "Reading Deluge's state" })} />);
    expect(html).toContain("progress-indeterminate");
    expect(html).not.toContain("aria-valuenow");
    expect(html).toContain("Reading Deluge&#x27;s state…");
  });
});

describe("how a job ended", () => {
  test("an import", () => {
    const report = { imported: ["a", "b"], notSelected: 0, identityHosts: [], skipped: [{ name: "c", reason: "r" }], warnings: [] };
    expect(jobSummary(job({}, { kind: "delugeImport", report }))).toBe("Imported 2 torrents, skipped 1.");
  });

  test("a failure says why", () => {
    expect(jobSummary(job({}, { kind: "failed", message: "no such folder" }))).toBe("no such folder");
  });
});

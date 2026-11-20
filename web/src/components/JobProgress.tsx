// A background job's progress and outcome, as text and a bar.

import type { Job } from "../api/generated/Job";

export function jobTitle(job: Job): string {
  return job.kind === "delugeImport" ? "Import from Deluge" : "Moving torrents over from Deluge";
}

/** One line on how a finished job went. */
export function jobSummary(job: Job): string {
  const o = job.outcome;
  if (o === null) return "";
  switch (o.kind) {
    case "delugeImport": {
      const r = o.report;
      const n = r.imported.length;
      return `Imported ${n} torrent${n === 1 ? "" : "s"}${r.skipped.length > 0 ? `, skipped ${r.skipped.length}` : ""}.`;
    }
    case "takeOver": {
      const r = o.report;
      return `Moved over ${r.takenOver}${r.skipped.length > 0 ? `, ${r.skipped.length} stayed paused` : ""}.`;
    }
    case "failed":
      return o.message;
  }
}

/** A job's progress: what it does now, a bar, how far along, and the item it's on. */
export function JobProgress({ job }: { job: Job }) {
  const { phase, done, total, current } = job.progress;
  const known = total > 0;
  const percent = known ? Math.min(100, (done / total) * 100) : 0;
  const label = phase || "Starting";
  return (
    <div className="job-progress">
      <div
        className={known ? "progress" : "progress progress-indeterminate"}
        role="progressbar"
        aria-label={label}
        aria-valuemin={0}
        aria-valuemax={known ? total : undefined}
        aria-valuenow={known ? done : undefined}
        aria-valuetext={known ? `${done} of ${total}` : label}
      >
        <div className="progress-fill" style={known ? { width: `${percent}%` } : undefined} />
        <span className="progress-text">{known ? `${done} of ${total}` : ""}</span>
      </div>
      <div className="job-progress-line muted">
        <span>{label}…</span>
        {current && <span className="job-progress-current">{current}</span>}
      </div>
    </div>
  );
}

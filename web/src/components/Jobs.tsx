// Background jobs on screen: a tray in the corner of every page that keeps each running or
// just-finished job in view (so the work never blocks the UI), and the Deluge import dialog,
// which any page can open.

import { useEffect } from "react";

import { discoverJobs, dismissJob, running, useJobs, type TrackedJob } from "../jobs";
import { useSpaceContext } from "../spaces/context";
import { ImportDeluge } from "./ImportDeluge";
import { closeDelugeImport, showDelugeImport, useDelugeImportDialog } from "./importDialog";
import { JobProgress, jobSummary, jobTitle } from "./JobProgress";

/** The tray and the import dialog: rendered once, beside whichever page is showing. */
export function BackgroundWork() {
  const dialog = useDelugeImportDialog();
  return (
    <>
      <JobsTray />
      {dialog && <ImportDeluge key={dialog.job ? `${dialog.job.slug ?? ""}:${dialog.job.id}` : "new"} job={dialog.job} onClose={closeDelugeImport} />}
    </>
  );
}

/** Every job this tab follows. */
export function JobsTray() {
  const all = useJobs();
  const ctx = useSpaceContext();
  const slug = ctx?.current.slug ?? null;
  const admin = ctx === null || ctx.serverAdmin || ctx.current.role === "admin";
  useEffect(() => {
    if (admin) void discoverJobs(slug);
  }, [admin, slug]);
  if (all.length === 0) return null;
  return (
    <aside className="jobs-tray" aria-label="Background work">
      {all.map((t) => (
        <TrayItem key={`${t.slug ?? ""}:${t.job.id}`} tracked={t} />
      ))}
    </aside>
  );
}

function TrayItem({ tracked }: { tracked: TrackedJob }) {
  const { job, slug } = tracked;
  const failed = job.outcome?.kind === "failed";
  const title = jobTitle(job);
  return (
    <div className="jobs-tray-item">
      <div className="jobs-tray-head">
        <strong>{title}</strong>
        {!running(tracked) && (
          <button type="button" className="link" aria-label={`Dismiss "${title}"`} onClick={() => dismissJob(slug, job.id)}>
            ✕
          </button>
        )}
      </div>
      {running(tracked) ? <JobProgress job={job} /> : <p className={failed ? "error-text" : "ok-text"}>{jobSummary(job)}</p>}
      <button type="button" className="link" onClick={() => showDelugeImport({ slug, id: job.id })}>
        {running(tracked) ? "Show details" : "See what happened"}
      </button>
    </div>
  );
}

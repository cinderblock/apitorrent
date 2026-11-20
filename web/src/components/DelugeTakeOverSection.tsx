// Settings → Deluge → Still with Deluge: imported torrents the Deluge they came from may still be
// running. Taking them over pauses them in Deluge (through its daemon, with the config
// directory's own password and certificate) and then runs them here. Resuming one torrent
// does the same for it.

import { useCallback, useEffect, useState } from "react";

import { ApiRequestError, api, request } from "../api/client";
import type { ImportSource } from "../api/generated/ImportSource";
import { trackJob, useJob } from "../jobs";
import { Empty, SettingCard } from "../settings/controls";
import { useSpaceContext } from "../spaces/context";
import type { JobRef } from "./importDialog";
import { JobProgress } from "./JobProgress";

function message(e: unknown): string {
  return e instanceof ApiRequestError ? e.message : String(e);
}

/** This space's admins. */
export function DelugeTakeOverSection() {
  const [waiting, setWaiting] = useState<ImportSource[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // The take-over started here: it runs in the background (the jobs tray shows it too).
  const [jobRef, setJobRef] = useState<JobRef | null>(null);
  const slug = useSpaceContext()?.current.slug ?? null;
  const tracked = useJob(jobRef?.slug ?? null, jobRef?.id ?? null);
  const outcome = tracked?.job.outcome ?? null;
  const report = outcome?.kind === "takeOver" ? outcome.report : null;
  const running = tracked !== undefined && outcome === null;
  const load = useCallback(() => {
    request<ImportSource[]>("GET", "/import/deluge/waiting").then(setWaiting, (e: unknown) => setError(message(e)));
  }, []);
  useEffect(load, [load]);
  // What's still waiting changes once the take-over ends.
  useEffect(() => {
    if (outcome !== null) load();
  }, [outcome, load]);

  async function takeOver() {
    setBusy(true);
    setError(null);
    try {
      const job = await api.startTakeOver(slug);
      trackJob(slug, job);
      setJobRef({ slug, id: job.id });
    } catch (e) {
      setError(message(e));
    } finally {
      setBusy(false);
    }
  }

  const total = waiting?.reduce((n, w) => n + w.torrents, 0) ?? 0;
  return (
    <SettingCard
      title="Still with Deluge"
      description="Imported torrents stay paused here until Deluge's copy is stopped, so only one client runs each. Taking them over pauses them in Deluge (if its daemon is running) and starts them here. Resuming a single torrent does the same for it."
    >
      {waiting?.length === 0 && !report && <Empty>No imported torrent is waiting on Deluge.</Empty>}
      {waiting && waiting.length > 0 && (
        <ul className="outcomes">
          {waiting.map((w) => (
            <li key={w.configDir}>
              <code>{w.configDir}</code>: {w.torrents} torrent{w.torrents === 1 ? "" : "s"}
            </li>
          ))}
        </ul>
      )}
      {total > 0 && !running && (
        <div>
          <button type="button" disabled={busy} onClick={() => void takeOver()}>
            {busy ? "Starting…" : `Take over all ${total}`}
          </button>
        </div>
      )}
      {running && tracked && <JobProgress job={tracked.job} />}
      {outcome?.kind === "failed" && <p className="error-text">{outcome.message}</p>}
      {report && (
        <p className={report.skipped.length > 0 ? "error-text" : "ok-text"}>
          Took over {report.takenOver} torrent{report.takenOver === 1 ? "" : "s"}.
          {report.skipped.length > 0 && ` ${report.skipped.length} stayed paused:`}
        </p>
      )}
      {report && report.skipped.length > 0 && (
        <ul className="outcomes">
          {report.skipped.map((s) => (
            <li key={s.name}>
              {s.name}: <span className="muted">{s.reason}</span>
            </li>
          ))}
        </ul>
      )}
      {error && <p className="error-text">{error}</p>}
    </SettingCard>
  );
}

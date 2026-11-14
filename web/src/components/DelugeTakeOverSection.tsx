// Settings → Deluge → Still with Deluge: imported torrents the Deluge they came from may still be
// running. Taking them over pauses them in Deluge (through its daemon, with the config
// directory's own password and certificate) and then runs them here. Resuming one torrent
// does the same for it.

import { useCallback, useEffect, useState } from "react";

import { ApiRequestError, request } from "../api/client";
import type { ImportSource } from "../api/generated/ImportSource";
import type { TakeOverReport } from "../api/generated/TakeOverReport";
import { Empty, SettingCard } from "../settings/controls";

function message(e: unknown): string {
  return e instanceof ApiRequestError ? e.message : String(e);
}

/** This space's admins. */
export function DelugeTakeOverSection() {
  const [waiting, setWaiting] = useState<ImportSource[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [report, setReport] = useState<TakeOverReport | null>(null);
  const load = useCallback(() => {
    request<ImportSource[]>("GET", "/import/deluge/waiting").then(setWaiting, (e: unknown) => setError(message(e)));
  }, []);
  useEffect(load, [load]);

  async function takeOver() {
    setBusy(true);
    setError(null);
    try {
      setReport(await request<TakeOverReport>("POST", "/import/deluge/take-over", { ids: [] }));
      load();
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
      {total > 0 && (
        <div>
          <button type="button" disabled={busy} onClick={() => void takeOver()}>
            {busy ? "Taking over…" : `Take over all ${total}`}
          </button>
        </div>
      )}
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

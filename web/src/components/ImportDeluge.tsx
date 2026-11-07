import { useEffect, useRef, useState } from "react";

import { ApiRequestError, api } from "../api/client";
import type { ImportReport } from "../api/generated/ImportReport";

/** Bring in a Deluge installation's torrents, from a directory on the server. */
export function ImportDeluge({ onClose }: { onClose: () => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [configDir, setConfigDir] = useState("");
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  const [importSettings, setImportSettings] = useState(false);
  const [paused, setPaused] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [report, setReport] = useState<ImportReport | null>(null);

  useEffect(() => {
    dialog.current?.showModal();
  }, []);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const pathMap = from.trim() && to.trim() ? [{ from: from.trim(), to: to.trim() }] : [];
      setReport(await api.importDeluge({ configDir: configDir.trim(), pathMap, importSettings, paused }));
    } catch (err) {
      setError(err instanceof ApiRequestError ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <dialog ref={dialog} className="dialog wide" onClose={onClose} aria-labelledby="import-title">
      <h2 id="import-title">Import from Deluge</h2>
      {report ? (
        <>
          <p>
            Imported {report.imported.length} torrent{report.imported.length === 1 ? "" : "s"}
            {report.skipped.length > 0 && `, skipped ${report.skipped.length}`}.
          </p>
          {report.skipped.length > 0 && (
            <ul className="outcomes">
              {report.skipped.map((s) => (
                <li key={s.name}>
                  {s.name}: <span className="muted">{s.reason}</span>
                </li>
              ))}
            </ul>
          )}
          {report.warnings.length > 0 && (
            <ul className="outcomes muted">
              {report.warnings.map((w) => (
                <li key={w}>{w}</li>
              ))}
            </ul>
          )}
          <div className="actions">
            <button type="button" className="primary" onClick={onClose}>
              Done
            </button>
          </div>
        </>
      ) : (
        <form onSubmit={submit}>
          <p className="hint muted">
            Torrents keep their files where Deluge left them, with their progress, labels, queue order and totals. Nothing is downloaded again and nothing in
            Deluge is changed. Stop Deluge first, so its state files are complete.
          </p>
          <label className="field">
            <span>Deluge config directory, on the machine running apitorrent</span>
            <input value={configDir} required placeholder="~/.config/deluge or %APPDATA%\deluge" onChange={(e) => setConfigDir(e.target.value)} />
          </label>
          <div className="field-row">
            <label className="field grow">
              <span>Deluge saved under (optional)</span>
              <input value={from} placeholder="/srv/torrents" onChange={(e) => setFrom(e.target.value)} />
            </label>
            <label className="field grow">
              <span>which is now</span>
              <input value={to} placeholder="D:\torrents" onChange={(e) => setTo(e.target.value)} />
            </label>
          </div>
          <label className="check">
            <input type="checkbox" checked={importSettings} onChange={(e) => setImportSettings(e.target.checked)} />
            Also use Deluge&apos;s speed, connection and queue limits
          </label>
          <label className="check">
            <input type="checkbox" checked={paused} onChange={(e) => setPaused(e.target.checked)} />
            Add them paused (resume them when you&apos;ve checked the paths)
          </label>
          {error && <p className="error-text">{error}</p>}
          <div className="actions">
            <button type="button" onClick={onClose}>
              Cancel
            </button>
            <button type="submit" className="primary" disabled={busy || configDir.trim() === ""}>
              {busy ? "Importing…" : "Import"}
            </button>
          </div>
        </form>
      )}
    </dialog>
  );
}

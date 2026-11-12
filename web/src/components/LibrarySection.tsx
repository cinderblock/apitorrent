// Settings → Library, the server admin's part: what dedupe found and freed, "Deduplicate now",
// and library folders (files outside torrents that new torrents can reuse).

import { useEffect, useState } from "react";

import { ApiRequestError, libraryApi } from "../api/client";
import type { LibraryStatus } from "../api/generated/LibraryStatus";
import { formatDateTime } from "../format";
import { dedupeFinding, dedupeSaved, folderPathProblem, folderProgress } from "./library";

function message(e: unknown): string {
  return e instanceof ApiRequestError ? e.message : String(e);
}

/** Server admins only: folders and dedupe are the instance's, across spaces. */
export function LibraryStatusSection({ dedupeOff }: { dedupeOff: boolean }) {
  const [status, setStatus] = useState<LibraryStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [path, setPath] = useState("");
  const [busy, setBusy] = useState(false);

  const load = () => {
    libraryApi.status().then(
      (s) => {
        setStatus(s);
        setError(null);
      },
      (e: unknown) => setError(message(e)),
    );
  };
  useEffect(load, []);
  // Poll while anything is moving: a dedupe pass, a walk, or fingerprinting still to do.
  const active = status !== null && (status.dedupe.running || status.folders.some((f) => f.scanning || f.hashedFiles < f.files));
  useEffect(() => {
    if (!active) return;
    const t = setInterval(load, 3000);
    return () => clearInterval(t);
  }, [active]);

  async function act(f: () => Promise<unknown>) {
    setBusy(true);
    setError(null);
    try {
      await f();
      load();
    } catch (e) {
      setError(message(e));
    } finally {
      setBusy(false);
    }
  }

  const problem = path.trim() === "" ? null : folderPathProblem(path);
  const saved = status && dedupeSaved(status.dedupe);
  return (
    <>
      <div className="library-status">
        {status && (
          <p>
            {status.dedupe.running ? "Checking for identical copies…" : dedupeFinding(status.dedupe)}
            {status.dedupe.lastPassAt !== null && !status.dedupe.running && (
              <span className="muted"> Last checked {formatDateTime(status.dedupe.lastPassAt)}.</span>
            )}
          </p>
        )}
        {saved && <p className="ok-text">{saved}</p>}
        {status && status.dedupe.unmergeableFiles > 0 && (
          <p className="hint muted">
            {status.dedupe.unmergeableFiles} more identical {status.dedupe.unmergeableFiles === 1 ? "copy is" : "copies are"} on another drive, or linked from
            somewhere apitorrent doesn&apos;t manage, so merging {status.dedupe.unmergeableFiles === 1 ? "it" : "them"} wouldn&apos;t free space.
          </p>
        )}
        <div className="inline-edit">
          <button type="button" disabled={busy || dedupeOff || status?.dedupe.running} onClick={() => void act(libraryApi.dedupe)}>
            Deduplicate now
          </button>
          {dedupeOff && <span className="muted">Turn dedupe on above (and save) to use this.</span>}
        </div>
      </div>

      <h3 className="settings-subhead">Library folders</h3>
      <p className="hint muted">
        Folders of files outside torrents, such as a media folder. New torrents reuse matching files from them instead of downloading. apitorrent only reads
        them: it never changes, moves or deletes their files.
      </p>
      {status && status.folders.length > 0 && (
        <ul className="outcomes">
          {status.folders.map((f) => (
            <li key={f.id}>
              <code>{f.path}</code> <span className={f.error ? "error-text" : "muted"}>{folderProgress(f)}</span>{" "}
              <button type="button" className="link" disabled={busy || f.scanning} onClick={() => void act(() => libraryApi.rescanFolder(f.id))}>
                Rescan
              </button>{" "}
              <button type="button" className="link" disabled={busy} onClick={() => void act(() => libraryApi.removeFolder(f.id))}>
                Remove
              </button>
            </li>
          ))}
        </ul>
      )}
      <div className="inline-edit">
        <input aria-label="Library folder to add" value={path} placeholder="/srv/media or D:\Media" onChange={(e) => setPath(e.target.value)} />
        <button
          type="button"
          disabled={busy || path.trim() === "" || problem !== null}
          onClick={() =>
            void act(async () => {
              await libraryApi.addFolder(path.trim());
              setPath("");
            })
          }
        >
          Add folder
        </button>
      </div>
      {problem && <p className="error-text">{problem}</p>}
      {error && <p className="error-text">{error}</p>}
    </>
  );
}

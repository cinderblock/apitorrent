// Settings → Library, the server admin's parts: what dedupe found and freed with
// "Deduplicate now", and library folders (files outside torrents that new torrents can
// reuse). Both act at once, apart from the page's Save.

import { useCallback, useEffect, useState } from "react";

import { ApiRequestError, libraryApi } from "../api/client";
import type { LibraryStatus } from "../api/generated/LibraryStatus";
import { formatDateTime } from "../format";
import { Empty } from "../settings/controls";
import { dedupeFinding, dedupeHeld, dedupeSaved, folderPathProblem, folderProgress } from "./library";

function message(e: unknown): string {
  return e instanceof ApiRequestError ? e.message : String(e);
}

/** The library's status, polled while anything is moving. */
function useLibraryStatus() {
  const [status, setStatus] = useState<LibraryStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const load = useCallback(() => {
    libraryApi.status().then(
      (s) => {
        setStatus(s);
        setError(null);
      },
      (e: unknown) => setError(message(e)),
    );
  }, []);
  useEffect(load, [load]);
  // A dedupe pass, a walk, or fingerprinting still to do.
  const active = status !== null && (status.dedupe.running || status.folders.some((f) => f.scanning || f.hashedFiles < f.files));
  useEffect(() => {
    if (!active) return;
    const t = setInterval(load, 3000);
    return () => clearInterval(t);
  }, [active, load]);

  const [busy, setBusy] = useState(false);
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
  return { status, error, busy, act };
}

/** What dedupe found and freed, and "Deduplicate now". `savedOff`: dedupe is off on the server. */
export function DedupeStatus({ savedOff }: { savedOff: boolean }) {
  const { status, error, busy, act } = useLibraryStatus();
  const saved = status && dedupeSaved(status.dedupe);
  const held = status && dedupeHeld(status.dedupe);
  return (
    <div className="s-status">
      {status && (
        <p>
          {status.dedupe.running ? "Checking for identical copies…" : dedupeFinding(status.dedupe)}
          {status.dedupe.lastPassAt !== null && !status.dedupe.running && (
            <span className="muted"> Last checked {formatDateTime(status.dedupe.lastPassAt)}.</span>
          )}
        </p>
      )}
      {saved && <p className="ok-text">{saved}</p>}
      {held && <p className="muted">{held}</p>}
      {status && status.dedupe.unmergeableFiles > 0 && (
        <p className="muted">
          {status.dedupe.unmergeableFiles} more identical {status.dedupe.unmergeableFiles === 1 ? "copy is" : "copies are"} on another drive, or linked from
          somewhere apitorrent doesn&apos;t manage, so merging {status.dedupe.unmergeableFiles === 1 ? "it" : "them"} wouldn&apos;t free space.
        </p>
      )}
      <div className="s-inline">
        <button type="button" disabled={busy || savedOff || status?.dedupe.running} onClick={() => void act(libraryApi.dedupe)}>
          Deduplicate now
        </button>
        {savedOff && <span className="muted">Pick Report or On and save first.</span>}
      </div>
      {error && <p className="error-text">{error}</p>}
    </div>
  );
}

/** Folders of files outside torrents that new torrents reuse. */
export function LibraryFolders() {
  const { status, error, busy, act } = useLibraryStatus();
  const [path, setPath] = useState("");
  const problem = path.trim() === "" ? null : folderPathProblem(path);
  return (
    <>
      {status && status.folders.length === 0 && <Empty>No library folders yet.</Empty>}
      {status && status.folders.length > 0 && (
        <ul className="s-list">
          {status.folders.map((f) => (
            <li key={f.id}>
              <div className="s-list-main">
                <code>{f.path}</code>
                <span className={f.error ? "error-text" : "muted"}>{folderProgress(f)}</span>
              </div>
              <div className="s-list-actions">
                <button type="button" disabled={busy || f.scanning} onClick={() => void act(() => libraryApi.rescanFolder(f.id))}>
                  Rescan
                </button>
                <button type="button" disabled={busy} onClick={() => void act(() => libraryApi.removeFolder(f.id))}>
                  Remove
                </button>
              </div>
            </li>
          ))}
        </ul>
      )}
      <form
        className="s-add"
        onSubmit={(e) => {
          e.preventDefault();
          void act(async () => {
            await libraryApi.addFolder(path.trim());
            setPath("");
          });
        }}
      >
        <input aria-label="Library folder to add" value={path} placeholder="/srv/media or D:\Media" spellCheck={false} onChange={(e) => setPath(e.target.value)} />
        <button type="submit" disabled={busy || path.trim() === "" || problem !== null}>
          Add folder
        </button>
      </form>
      {problem && <p className="error-text">{problem}</p>}
      {error && <p className="error-text">{error}</p>}
    </>
  );
}

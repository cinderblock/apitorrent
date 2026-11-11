import { useEffect, useRef, useState } from "react";

import { api, ApiRequestError, fileToBase64 } from "../api/client";
import { FolderInput, useLocalMode } from "../desktop/FolderInput";

interface Props {
  /** Files dropped on the window, to add straight away. */
  initialFiles: File[];
  /** Magnet links to add (one per line), such as one the desktop app was opened with. */
  initialMagnet?: string;
  onClose: () => void;
}

interface Outcome {
  name: string;
  error: string | null;
}

export function AddTorrent({ initialFiles, initialMagnet = "", onClose }: Props) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [files, setFiles] = useState<File[]>(initialFiles);
  const [magnet, setMagnet] = useState(initialMagnet);
  const [paused, setPaused] = useState(false);
  const [label, setLabel] = useState("");
  const [savePath, setSavePath] = useState("");
  const [busy, setBusy] = useState(false);
  const [outcomes, setOutcomes] = useState<Outcome[]>([]);
  // The server's folders are this computer's only in the desktop app's Local mode.
  const localMode = useLocalMode();

  useEffect(() => {
    // An in-page modal dialog, not a browser prompt: it never blocks the page.
    dialog.current?.showModal();
  }, []);

  const options = {
    paused,
    label: label.trim() || null,
    savePath: savePath.trim() || null,
    filePriorities: null,
    unwantedPolicy: "hold" as const,
  };

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    const results: Outcome[] = [];
    for (const file of files) {
      try {
        await api.add({ source: { kind: "torrentFile", data: await fileToBase64(file) }, ...options });
        results.push({ name: file.name, error: null });
      } catch (err) {
        results.push({ name: file.name, error: err instanceof ApiRequestError ? err.message : String(err) });
      }
    }
    for (const line of magnet.split(/\s+/).filter((l) => l.startsWith("magnet:"))) {
      try {
        await api.add({ source: { kind: "magnet", uri: line }, ...options });
        results.push({ name: "magnet link", error: null });
      } catch (err) {
        results.push({ name: "magnet link", error: err instanceof ApiRequestError ? err.message : String(err) });
      }
    }
    setBusy(false);
    setOutcomes(results);
    if (results.length > 0 && results.every((r) => r.error === null)) onClose();
  }

  const nothing = files.length === 0 && !magnet.includes("magnet:");

  return (
    <dialog ref={dialog} className="dialog" onClose={onClose} aria-labelledby="add-title">
      <form onSubmit={submit}>
        <h2 id="add-title">Add torrents</h2>
        <label className="field">
          <span>Torrent files</span>
          <input
            type="file"
            accept=".torrent,application/x-bittorrent"
            multiple
            onChange={(e) => setFiles([...files, ...Array.from(e.target.files ?? [])])}
          />
        </label>
        {files.length > 0 && (
          <ul className="chosen">
            {files.map((f, i) => (
              <li key={`${f.name}-${i}`}>
                {f.name}
                <button type="button" className="link" onClick={() => setFiles(files.filter((_, j) => j !== i))}>
                  Remove
                </button>
              </li>
            ))}
          </ul>
        )}
        <label className="field">
          <span>Magnet links (one per line)</span>
          <textarea rows={3} value={magnet} onChange={(e) => setMagnet(e.target.value)} placeholder="magnet:?xt=urn:btih:…" />
        </label>
        <div className="field-row">
          <label className="field">
            <span>Label</span>
            <input value={label} onChange={(e) => setLabel(e.target.value)} placeholder="none" />
          </label>
          <label className="field grow">
            <span>Save to</span>
            <FolderInput
              value={savePath}
              onChange={setSavePath}
              placeholder="the default download folder"
              pickerTitle="Save to"
              browse={localMode}
            />
          </label>
        </div>
        <label className="check">
          <input type="checkbox" checked={paused} onChange={(e) => setPaused(e.target.checked)} />
          Add paused
        </label>
        {outcomes.some((o) => o.error) && (
          <ul className="outcomes" role="alert">
            {outcomes.map((o, i) => (
              <li key={i} className={o.error ? "error-text" : "ok-text"}>
                {o.name}: {o.error ?? "added"}
              </li>
            ))}
          </ul>
        )}
        <div className="actions">
          <button type="button" onClick={onClose}>Cancel</button>
          <button type="submit" className="primary" disabled={busy || nothing}>
            {busy ? "Adding…" : "Add"}
          </button>
        </div>
      </form>
    </dialog>
  );
}

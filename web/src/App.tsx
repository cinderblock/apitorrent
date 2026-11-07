import { useCallback, useEffect, useMemo, useState, useSyncExternalStore, type KeyboardEvent } from "react";

import { api } from "./api/client";
import { SyncClient } from "./api/sync";
import { AddTorrent } from "./components/AddTorrent";
import { ConfirmRemove } from "./components/ConfirmRemove";
import { DetailPane } from "./components/DetailPane";
import { ImportDeluge } from "./components/ImportDeluge";
import { SettingsDialog } from "./components/SettingsDialog";
import { Sidebar } from "./components/Sidebar";
import { TorrentTable } from "./components/TorrentTable";
import { formatRate } from "./format";
import { countByFilter, labels as labelCounts, visibleTorrents, type SortKey, type View } from "./state/torrents";

const sync = SyncClient.forCurrentPage();

/** Keys that sort large-first by default; text and state sort A–Z. */
const DESCENDING_FIRST: ReadonlySet<SortKey> = new Set([
  "size",
  "progress",
  "downloadRate",
  "uploadRate",
  "peers",
  "ratio",
  "addedAt",
]);

export function App() {
  useEffect(() => {
    sync.start();
    return () => sync.stop();
  }, []);
  const { state, status } = useSyncExternalStore(sync.subscribe, sync.getSnapshot);

  const [view, setView] = useState<View>({
    filter: "all",
    label: null,
    search: "",
    sort: { key: "addedAt", descending: true },
  });
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [focus, setFocus] = useState<string | null>(null);
  const [adding, setAdding] = useState<File[] | null>(null);
  const [removing, setRemoving] = useState<string[] | null>(null);
  const [dragging, setDragging] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [importing, setImporting] = useState(false);

  const rows = useMemo(() => visibleTorrents(state.torrents, view), [state.torrents, view]);
  const counts = useMemo(() => countByFilter(state.torrents), [state.torrents]);
  const labels = useMemo(() => labelCounts(state.torrents), [state.torrents]);
  const totals = useMemo(() => {
    let down = 0;
    let up = 0;
    for (const t of state.torrents.values()) {
      down += t.downloadRate;
      up += t.uploadRate;
    }
    return { down, up };
  }, [state.torrents]);

  // Forget selected torrents that have been removed.
  useEffect(() => {
    if ([...selected].some((id) => !state.torrents.has(id))) {
      setSelected(new Set([...selected].filter((id) => state.torrents.has(id))));
    }
  }, [state.torrents, selected]);

  const focused = focus ? state.torrents.get(focus) : undefined;
  const selection = [...selected];

  const pauseOrResume = useCallback(
    (ids: string[]) => {
      const anyRunning = ids.some((id) => state.torrents.get(id)?.state !== "paused");
      for (const id of ids) void (anyRunning ? api.pause(id) : api.resume(id));
    },
    [state.torrents],
  );

  /** Move the selection in the queue, keeping its order: going up, the top one moves first. */
  function queueMove(to: "top" | "up" | "down" | "bottom") {
    const ids = [...selection].sort((a, b) => (state.torrents.get(a)?.queuePosition ?? 0) - (state.torrents.get(b)?.queuePosition ?? 0));
    const ordered = to === "down" || to === "top" ? ids.reverse() : ids;
    void (async () => {
      for (const id of ordered) await api.queueMove(id, to);
    })();
  }

  function onSort(key: SortKey) {
    setView((v) => ({
      ...v,
      sort: v.sort.key === key ? { key, descending: !v.sort.descending } : { key, descending: DESCENDING_FIRST.has(key) },
    }));
  }

  function onKey(e: KeyboardEvent) {
    if (selection.length === 0) return;
    if (e.key === "Delete") {
      e.preventDefault();
      setRemoving(selection);
    } else if (e.key === " ") {
      e.preventDefault();
      pauseOrResume(selection);
    } else if (e.key === "a" && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      setSelected(new Set(rows.map((r) => r.id)));
    }
  }

  // Drop .torrent files anywhere on the window.
  useEffect(() => {
    const over = (e: DragEvent) => {
      if (e.dataTransfer?.types.includes("Files")) {
        e.preventDefault();
        setDragging(true);
      }
    };
    const leave = (e: DragEvent) => {
      if (e.relatedTarget === null) setDragging(false);
    };
    const drop = (e: DragEvent) => {
      e.preventDefault();
      setDragging(false);
      const files = Array.from(e.dataTransfer?.files ?? []).filter((f) => f.name.toLowerCase().endsWith(".torrent"));
      if (files.length > 0) setAdding(files);
    };
    window.addEventListener("dragover", over);
    window.addEventListener("dragleave", leave);
    window.addEventListener("drop", drop);
    return () => {
      window.removeEventListener("dragover", over);
      window.removeEventListener("dragleave", leave);
      window.removeEventListener("drop", drop);
    };
  }, []);

  return (
    <div className={`app ${dragging ? "dragging" : ""}`}>
      <header className="toolbar">
        <h1 className="brand">apitorrent</h1>
        <button type="button" className="primary" onClick={() => setAdding([])}>
          Add
        </button>
        <button type="button" disabled={selection.length === 0} onClick={() => pauseOrResume(selection)}>
          Pause / Resume
        </button>
        <button type="button" disabled={selection.length === 0} onClick={() => setRemoving(selection)}>
          Remove
        </button>
        <div className="button-group" role="group" aria-label="Queue">
          <button type="button" disabled={selection.length === 0} aria-label="Move to the top of the queue" onClick={() => queueMove("top")}>
            ⤒
          </button>
          <button type="button" disabled={selection.length === 0} aria-label="Move up the queue" onClick={() => queueMove("up")}>
            ↑
          </button>
          <button type="button" disabled={selection.length === 0} aria-label="Move down the queue" onClick={() => queueMove("down")}>
            ↓
          </button>
          <button type="button" disabled={selection.length === 0} aria-label="Move to the bottom of the queue" onClick={() => queueMove("bottom")}>
            ⤓
          </button>
        </div>
        <button type="button" onClick={() => setSettingsOpen(true)}>
          Settings
        </button>
        <input
          type="search"
          className="search"
          placeholder="Search"
          aria-label="Search torrents"
          value={view.search}
          onChange={(e) => setView({ ...view, search: e.target.value })}
        />
        <div className="rates" aria-label="Total transfer rates">
          <span aria-label="Download rate">↓ {formatRate(totals.down) || "0 B/s"}</span>
          <span aria-label="Upload rate">↑ {formatRate(totals.up) || "0 B/s"}</span>
        </div>
        <span className={`conn conn-${status}`} role="status">
          {status === "live" ? "Live" : status === "connecting" ? "Connecting…" : "Reconnecting…"}
        </span>
      </header>
      <Sidebar
        filter={view.filter}
        label={view.label}
        counts={counts}
        labels={labels}
        onFilter={(filter) => setView({ ...view, filter })}
        onLabel={(label) => setView({ ...view, label })}
      />
      <main className="main">
        <TorrentTable
          rows={rows}
          selected={selected}
          sort={view.sort}
          onSort={onSort}
          focus={focus}
          onSelect={(ids, f) => {
            setSelected(ids);
            setFocus(f);
          }}
          onOpen={(id) => setFocus(id)}
          onKey={onKey}
        />
        {focused && <DetailPane summary={focused} />}
      </main>
      {adding && <AddTorrent initialFiles={adding} onClose={() => setAdding(null)} />}
      {settingsOpen && (
        <SettingsDialog
          onClose={() => setSettingsOpen(false)}
          onImport={() => {
            setSettingsOpen(false);
            setImporting(true);
          }}
        />
      )}
      {importing && <ImportDeluge onClose={() => setImporting(false)} />}
      {removing && (
        <ConfirmRemove
          count={removing.length}
          onClose={() => setRemoving(null)}
          onConfirm={(deleteFiles) => {
            for (const id of removing) void api.remove(id, deleteFiles);
            setRemoving(null);
          }}
        />
      )}
    </div>
  );
}

import { useVirtualizer } from "@tanstack/react-virtual";
import { useMemo, useRef, useState, type KeyboardEvent, type MouseEvent } from "react";

import type { TorrentSummary } from "../api/generated/TorrentSummary";
import { formatBytes, formatDateTime, formatEta, formatPercent, formatRate, formatRatio } from "../format";
import { loadColumnPrefs, saveColumnPrefs, visibleColumns, type ColumnInfo, type ColumnPrefs } from "../state/columns";
import { etaSeconds, type Sort, type SortKey } from "../state/torrents";
import { ColumnChooser } from "./ColumnChooser";

const ROW_HEIGHT = 30;

export interface Column extends ColumnInfo {
  id: SortKey;
  label: string;
  width: string;
  align?: "end";
  render: (t: TorrentSummary) => React.ReactNode;
}

const STATE_LABEL: Record<TorrentSummary["state"], string> = {
  checking: "Checking",
  downloading: "Downloading",
  seeding: "Seeding",
  paused: "Paused",
  queued: "Queued",
  metadata: "Metadata…",
  error: "Error",
};

function eta(t: TorrentSummary): string {
  const s = etaSeconds(t);
  return Number.isFinite(s) ? formatEta(s) : "";
}

export const COLUMNS: Column[] = [
  // Queue order: lower numbers get the free slots first.
  { id: "queuePosition", label: "#", width: "2.5rem", align: "end", defaultVisible: true, render: (t) => `${t.queuePosition + 1}` },
  {
    id: "name",
    label: "Name",
    width: "minmax(8rem, 1fr)",
    defaultVisible: true,
    required: true,
    render: (t) => <span className="cell-name">{t.name}</span>,
  },
  { id: "size", label: "Size", width: "5.5rem", align: "end", defaultVisible: true, render: (t) => formatBytes(t.size) },
  {
    id: "progress",
    label: "Progress",
    width: "7rem",
    defaultVisible: true,
    render: (t) => (
      <div className={`progress progress-${t.state}`} role="progressbar" aria-valuenow={Math.round(t.progress * 100)} aria-valuemin={0} aria-valuemax={100}>
        <div className="progress-fill" style={{ width: `${t.progress * 100}%` }} />
        <span className="progress-text">{formatPercent(t.progress)}</span>
      </div>
    ),
  },
  {
    id: "state",
    label: "Status",
    width: "6rem",
    defaultVisible: true,
    render: (t) => <span className={`state state-${t.state}`}>{STATE_LABEL[t.state]}</span>,
  },
  { id: "downloadRate", label: "Down", width: "5.25rem", align: "end", defaultVisible: true, render: (t) => formatRate(t.downloadRate) },
  { id: "uploadRate", label: "Up", width: "5.25rem", align: "end", defaultVisible: true, render: (t) => formatRate(t.uploadRate) },
  {
    id: "peers",
    label: "Peers (seeds)",
    width: "6.25rem",
    align: "end",
    defaultVisible: true,
    // Connected peers, with how many of them are seeds.
    render: (t) => (t.peers > 0 ? `${t.peers} (${t.seeds})` : ""),
  },
  { id: "ratio", label: "Ratio", width: "3.5rem", align: "end", defaultVisible: true, render: (t) => formatRatio(t.uploaded, t.downloaded) },
  { id: "eta", label: "ETA", width: "4rem", align: "end", defaultVisible: true, render: eta },
  { id: "downloaded", label: "Downloaded", width: "6rem", align: "end", defaultVisible: false, render: (t) => formatBytes(t.downloaded) },
  { id: "uploaded", label: "Uploaded", width: "6rem", align: "end", defaultVisible: false, render: (t) => formatBytes(t.uploaded) },
  { id: "label", label: "Label", width: "7rem", defaultVisible: false, render: (t) => t.label ?? "" },
  {
    id: "savePath",
    label: "Save path",
    width: "minmax(6rem, 0.6fr)",
    defaultVisible: false,
    render: (t) => <span className="cell-path">{t.savePath}</span>,
  },
  {
    id: "addedAt",
    label: "Added",
    width: "8.75rem",
    defaultVisible: true,
    render: (t) => formatDateTime(t.addedAt),
  },
];

const BY_ID = new Map(COLUMNS.map((c) => [c.id as string, c]));

interface Props {
  rows: TorrentSummary[];
  selected: ReadonlySet<string>;
  sort: Sort;
  onSort: (key: SortKey) => void;
  onSelect: (ids: Set<string>, focus: string | null) => void;
  focus: string | null;
  onOpen: (id: string) => void;
  onKey: (e: KeyboardEvent) => void;
}

export function TorrentTable({ rows, selected, sort, onSort, onSelect, focus, onOpen, onKey }: Props) {
  const scroller = useRef<HTMLDivElement>(null);
  const [prefs, setPrefs] = useState<ColumnPrefs | null>(loadColumnPrefs);
  const [choosing, setChoosing] = useState(false);
  const columns = useMemo(() => visibleColumns(COLUMNS, prefs).map((id) => BY_ID.get(id)!), [prefs]);
  // The last track is the column chooser's button.
  const template = `${columns.map((c) => c.width).join(" ")} 1.75rem`;
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scroller.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 12,
  });

  function changePrefs(next: ColumnPrefs | null) {
    setPrefs(next);
    saveColumnPrefs(next);
  }

  function click(e: MouseEvent, index: number) {
    const id = rows[index]!.id;
    if (e.shiftKey && focus !== null) {
      const from = rows.findIndex((r) => r.id === focus);
      const [a, b] = from < index ? [from, index] : [index, from];
      const range = new Set(rows.slice(Math.max(a, 0), b + 1).map((r) => r.id));
      onSelect(e.ctrlKey || e.metaKey ? new Set([...selected, ...range]) : range, focus);
    } else if (e.ctrlKey || e.metaKey) {
      const next = new Set(selected);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      onSelect(next, id);
    } else {
      onSelect(new Set([id]), id);
    }
  }

  function move(e: KeyboardEvent) {
    if (e.key !== "ArrowDown" && e.key !== "ArrowUp" && e.key !== "Home" && e.key !== "End") {
      onKey(e);
      return;
    }
    e.preventDefault();
    if (rows.length === 0) return;
    const current = focus === null ? -1 : rows.findIndex((r) => r.id === focus);
    const next =
      e.key === "Home" ? 0
      : e.key === "End" ? rows.length - 1
      : e.key === "ArrowDown" ? Math.min(rows.length - 1, current + 1)
      : Math.max(0, current - 1);
    const id = rows[next]!.id;
    onSelect(e.shiftKey ? new Set([...selected, id]) : new Set([id]), id);
    virtualizer.scrollToIndex(next);
  }

  return (
    <div className="table" role="grid" aria-rowcount={rows.length} aria-multiselectable="true">
      <div
        className="table-head"
        role="row"
        style={{ gridTemplateColumns: template }}
        onContextMenu={(e) => {
          e.preventDefault();
          setChoosing(true);
        }}
      >
        {columns.map((c) => (
          <button
            key={c.id}
            type="button"
            role="columnheader"
            className={`th ${c.align === "end" ? "end" : ""}`}
            aria-sort={sort.key === c.id ? (sort.descending ? "descending" : "ascending") : "none"}
            onClick={() => onSort(c.id)}
          >
            {c.label}
            {sort.key === c.id && <span className="sort-arrow">{sort.descending ? "▾" : "▴"}</span>}
          </button>
        ))}
        <button
          type="button"
          className="th th-columns"
          aria-label="Choose columns"
          aria-haspopup="dialog"
          aria-expanded={choosing}
          onClick={() => setChoosing((v) => !v)}
        >
          ⋮
        </button>
      </div>
      {choosing && <ColumnChooser columns={COLUMNS} prefs={prefs} onChange={changePrefs} onClose={() => setChoosing(false)} />}
      <div className="table-body" ref={scroller} tabIndex={0} onKeyDown={move}>
        {rows.length === 0 ? (
          <div className="empty">No torrents here. Drop a .torrent file anywhere to add one.</div>
        ) : (
          <div style={{ height: virtualizer.getTotalSize(), position: "relative" }}>
            {virtualizer.getVirtualItems().map((v) => {
              const t = rows[v.index]!;
              const isSelected = selected.has(t.id);
              return (
                <div
                  key={t.id}
                  role="row"
                  aria-selected={isSelected}
                  className={`tr ${isSelected ? "selected" : ""} ${focus === t.id ? "focused" : ""}`}
                  style={{ gridTemplateColumns: template, transform: `translateY(${v.start}px)`, height: ROW_HEIGHT }}
                  onMouseDown={(e) => {
                    if (e.shiftKey) e.preventDefault(); // don't select text
                  }}
                  onClick={(e) => click(e, v.index)}
                  onDoubleClick={() => onOpen(t.id)}
                >
                  {columns.map((c) => (
                    <div key={c.id} role="gridcell" className={`td ${c.align === "end" ? "end" : ""}`}>
                      {c.render(t)}
                    </div>
                  ))}
                  <div role="presentation" />
                </div>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}

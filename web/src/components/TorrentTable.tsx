import { useVirtualizer } from "@tanstack/react-virtual";
import { useRef, type KeyboardEvent, type MouseEvent } from "react";

import type { TorrentSummary } from "../api/generated/TorrentSummary";
import { formatBytes, formatDateTime, formatEta, formatPercent, formatRate, formatRatio } from "../format";
import type { Sort, SortKey } from "../state/torrents";

const ROW_HEIGHT = 30;

interface Column {
  key: SortKey;
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
  error: "Error",
};

function eta(t: TorrentSummary): string {
  if (t.state !== "downloading" || t.downloadRate <= 0) return "";
  const left = t.size * (1 - t.progress);
  return formatEta(left / t.downloadRate);
}

const COLUMNS: Column[] = [
  {
    key: "name",
    label: "Name",
    width: "minmax(10rem, 1fr)",
    render: (t) => <span className="cell-name">{t.name}</span>,
  },
  { key: "size", label: "Size", width: "6.5rem", align: "end", render: (t) => formatBytes(t.size) },
  {
    key: "progress",
    label: "Progress",
    width: "8rem",
    render: (t) => (
      <div className={`progress progress-${t.state}`} role="progressbar" aria-valuenow={Math.round(t.progress * 100)} aria-valuemin={0} aria-valuemax={100}>
        <div className="progress-fill" style={{ width: `${t.progress * 100}%` }} />
        <span className="progress-text">{formatPercent(t.progress)}</span>
      </div>
    ),
  },
  {
    key: "state",
    label: "Status",
    width: "6.5rem",
    render: (t) => <span className={`state state-${t.state}`}>{STATE_LABEL[t.state]}</span>,
  },
  { key: "downloadRate", label: "Down", width: "5.75rem", align: "end", render: (t) => formatRate(t.downloadRate) },
  { key: "uploadRate", label: "Up", width: "5.75rem", align: "end", render: (t) => formatRate(t.uploadRate) },
  {
    key: "peers",
    label: "Peers (seeds)",
    width: "6.5rem",
    align: "end",
    // Connected peers, with how many of them are seeds.
    render: (t) => (t.peers > 0 ? `${t.peers} (${t.seeds})` : ""),
  },
  { key: "ratio", label: "Ratio", width: "3.5rem", align: "end", render: (t) => formatRatio(t.uploaded, t.downloaded) },
  {
    key: "addedAt",
    label: "Added",
    width: "9.5rem",
    render: (t) => formatDateTime(t.addedAt),
  },
];

const TEMPLATE = `${COLUMNS.map((c) => c.width).join(" ")} 4rem`;

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
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scroller.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 12,
  });

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
      <div className="table-head" role="row" style={{ gridTemplateColumns: TEMPLATE }}>
        {COLUMNS.map((c) => (
          <button
            key={c.key}
            type="button"
            role="columnheader"
            className={`th ${c.align === "end" ? "end" : ""}`}
            aria-sort={sort.key === c.key ? (sort.descending ? "descending" : "ascending") : "none"}
            onClick={() => onSort(c.key)}
          >
            {c.label}
            {sort.key === c.key && <span className="sort-arrow">{sort.descending ? "▾" : "▴"}</span>}
          </button>
        ))}
        <div className="th end" role="columnheader">ETA</div>
      </div>
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
                  style={{ gridTemplateColumns: TEMPLATE, transform: `translateY(${v.start}px)`, height: ROW_HEIGHT }}
                  onMouseDown={(e) => {
                    if (e.shiftKey) e.preventDefault(); // don't select text
                  }}
                  onClick={(e) => click(e, v.index)}
                  onDoubleClick={() => onOpen(t.id)}
                >
                  {COLUMNS.map((c) => (
                    <div key={c.key} role="gridcell" className={`td ${c.align === "end" ? "end" : ""}`}>
                      {c.render(t)}
                    </div>
                  ))}
                  <div role="gridcell" className="td end">{eta(t)}</div>
                </div>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}

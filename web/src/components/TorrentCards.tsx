import { useVirtualizer } from "@tanstack/react-virtual";
import { useRef } from "react";

import type { TorrentSummary } from "../api/generated/TorrentSummary";
import { formatBytes, formatEta, formatPercent, formatRate } from "../format";
import { etaSeconds, type Sort, type SortKey } from "../state/torrents";
import { COLUMNS } from "./TorrentTable";

const CARD_HEIGHT = 84;

const STATE_LABEL: Record<TorrentSummary["state"], string> = {
  checking: "Checking",
  downloading: "Downloading",
  seeding: "Seeding",
  paused: "Paused",
  queued: "Queued",
  metadata: "Fetching metadata",
  error: "Error",
};

interface Props {
  rows: TorrentSummary[];
  selected: ReadonlySet<string>;
  sort: Sort;
  onSort: (key: SortKey) => void;
  onSelect: (ids: Set<string>, focus: string | null) => void;
  focus: string | null;
  onOpen: (id: string) => void;
}

/**
 * The torrent list on a phone: one card per torrent with the name, a progress bar and the
 * numbers that matter, instead of a grid too wide for the screen. Tapping a card opens its
 * details; the checkbox adds it to the selection for the action bar.
 */
export function TorrentCards({ rows, selected, sort, onSort, onSelect, focus, onOpen }: Props) {
  const scroller = useRef<HTMLDivElement>(null);
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scroller.current,
    estimateSize: () => CARD_HEIGHT,
    overscan: 8,
  });

  function toggle(id: string) {
    const next = new Set(selected);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    onSelect(next, focus);
  }

  return (
    <div className="cards">
      <div className="cards-sort">
        <label>
          <span className="muted">Sort by </span>
          <select value={sort.key} onChange={(e) => onSort(e.target.value as SortKey)}>
            {COLUMNS.map((c) => (
              <option key={c.id} value={c.id}>
                {c.label === "#" ? "Queue position" : c.label}
              </option>
            ))}
          </select>
        </label>
        <button type="button" aria-label={sort.descending ? "Sorted high to low; reverse" : "Sorted low to high; reverse"} onClick={() => onSort(sort.key)}>
          {sort.descending ? "↓ High first" : "↑ Low first"}
        </button>
        {selected.size > 0 && (
          <button type="button" onClick={() => onSelect(new Set(), focus)}>
            Clear {selected.size} selected
          </button>
        )}
      </div>
      <div className="cards-body" ref={scroller}>
        {rows.length === 0 ? (
          <div className="empty">No torrents here. Use Add to add one.</div>
        ) : (
          <div style={{ height: virtualizer.getTotalSize(), position: "relative" }}>
            {virtualizer.getVirtualItems().map((v) => {
              const t = rows[v.index]!;
              const eta = etaSeconds(t);
              const isSelected = selected.has(t.id);
              return (
                <div
                  key={t.id}
                  className={`card ${isSelected ? "selected" : ""}`}
                  style={{ transform: `translateY(${v.start}px)`, height: CARD_HEIGHT }}
                >
                  <input type="checkbox" className="card-check" aria-label={`Select ${t.name}`} checked={isSelected} onChange={() => toggle(t.id)} />
                  <button type="button" className="card-main" onClick={() => onOpen(t.id)}>
                    <span className="card-name">{t.name}</span>
                    <span className={`progress progress-${t.state}`} role="progressbar" aria-valuenow={Math.round(t.progress * 100)} aria-valuemin={0} aria-valuemax={100}>
                      <span className="progress-fill" style={{ width: `${t.progress * 100}%` }} />
                      <span className="progress-text">{formatPercent(t.progress)}</span>
                    </span>
                    <span className="card-meta">
                      <span className={`state state-${t.state}`}>{STATE_LABEL[t.state]}</span>
                      <span>{formatBytes(t.size)}</span>
                      {t.downloadRate > 0 && <span>↓ {formatRate(t.downloadRate)}</span>}
                      {t.uploadRate > 0 && <span>↑ {formatRate(t.uploadRate)}</span>}
                      {Number.isFinite(eta) && <span>{formatEta(eta)} left</span>}
                      {t.peers > 0 && <span>{t.peers} peers</span>}
                    </span>
                  </button>
                </div>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}

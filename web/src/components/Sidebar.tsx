import { useEffect } from "react";

import type { StateFilter } from "../state/torrents";

const FILTERS: Array<[StateFilter, string]> = [
  ["all", "All"],
  ["active", "Active"],
  ["downloading", "Downloading"],
  ["seeding", "Seeding"],
  ["paused", "Paused"],
  ["queued", "Queued"],
  ["checking", "Checking"],
  ["metadata", "Fetching metadata"],
  ["error", "Errors"],
];

interface Props {
  /** On a phone the sidebar is a drawer: whether it is showing. */
  open: boolean;
  onClose: () => void;
  filter: StateFilter;
  label: string | null;
  counts: Record<StateFilter, number>;
  labels: Array<[string, number]>;
  onFilter: (f: StateFilter) => void;
  onLabel: (l: string | null) => void;
}

export function Sidebar({ open, onClose, filter, label, counts, labels, onFilter, onLabel }: Props) {
  useEffect(() => {
    if (!open) return;
    const escape = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", escape);
    return () => document.removeEventListener("keydown", escape);
  }, [open, onClose]);

  return (
    <>
      {open && <div className="drawer-backdrop" onClick={onClose} aria-hidden="true" />}
      <nav id="filters" className={`sidebar ${open ? "open" : ""}`} aria-label="Filters">
        <h2>Status</h2>
        <ul>
          {FILTERS.filter(([f]) => f === "all" || f === filter || counts[f] > 0).map(([f, name]) => (
            <li key={f}>
              <button type="button" className={`filter ${filter === f ? "active" : ""}`} aria-pressed={filter === f} onClick={() => onFilter(f)}>
                <span>{name}</span>
                <span className="count">{counts[f]}</span>
              </button>
            </li>
          ))}
        </ul>
        {labels.length > 0 && (
          <>
            <h2>Labels</h2>
            <ul>
              <li>
                <button type="button" className={`filter ${label === null ? "active" : ""}`} aria-pressed={label === null} onClick={() => onLabel(null)}>
                  <span>Any</span>
                </button>
              </li>
              {labels.map(([l, n]) => (
                <li key={l}>
                  <button type="button" className={`filter ${label === l ? "active" : ""}`} aria-pressed={label === l} onClick={() => onLabel(l)}>
                    <span>{l}</span>
                    <span className="count">{n}</span>
                  </button>
                </li>
              ))}
            </ul>
          </>
        )}
      </nav>
    </>
  );
}

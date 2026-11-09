// The client side of the sync protocol: a snapshot, then patches that each advance the
// revision by exactly one. Anything else means we missed something and must resubscribe.

import type { SyncMessage } from "../api/generated/SyncMessage";
import type { TorrentSummary } from "../api/generated/TorrentSummary";

export type TorrentMap = ReadonlyMap<string, TorrentSummary>;

export interface SyncState {
  /** `null` until the first snapshot arrives. */
  revision: number | null;
  torrents: TorrentMap;
}

export const initialSyncState: SyncState = { revision: null, torrents: new Map() };

export type ApplyResult =
  | { ok: true; state: SyncState }
  /** A gap in revisions: the caller must reconnect to get a fresh snapshot. */
  | { ok: false; reason: string };

export function applySync(state: SyncState, msg: SyncMessage): ApplyResult {
  switch (msg.type) {
    case "snapshot":
      return {
        ok: true,
        state: {
          revision: msg.revision,
          torrents: new Map(msg.torrents.map((t) => [t.id, t])),
        },
      };
    case "patch": {
      if (state.revision === null) {
        return { ok: false, reason: "patch before snapshot" };
      }
      if (msg.revision !== state.revision + 1) {
        return {
          ok: false,
          reason: `expected revision ${state.revision + 1}, got ${msg.revision}`,
        };
      }
      const torrents = new Map(state.torrents);
      for (const t of msg.upserted) torrents.set(t.id, t);
      for (const id of msg.removed) torrents.delete(id);
      return { ok: true, state: { revision: msg.revision, torrents } };
    }
  }
}

export type StateFilter =
  | "all"
  | "downloading"
  | "seeding"
  | "paused"
  | "queued"
  | "metadata"
  | "checking"
  | "error"
  | "active";

export function matchesFilter(t: TorrentSummary, filter: StateFilter): boolean {
  switch (filter) {
    case "all":
      return true;
    case "active":
      return t.downloadRate > 0 || t.uploadRate > 0;
    default:
      return t.state === filter;
  }
}

export type SortKey =
  | "queuePosition"
  | "name"
  | "size"
  | "progress"
  | "state"
  | "downloadRate"
  | "uploadRate"
  | "peers"
  | "ratio"
  | "addedAt"
  | "eta"
  | "downloaded"
  | "uploaded"
  | "label"
  | "savePath";

export interface Sort {
  key: SortKey;
  descending: boolean;
}

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });

function ratio(t: TorrentSummary): number {
  return t.downloaded > 0 ? t.uploaded / t.downloaded : t.uploaded > 0 ? Infinity : 0;
}

/** Seconds left at the current rate; Infinity when not downloading. */
export function etaSeconds(t: TorrentSummary): number {
  if (t.state !== "downloading" || t.downloadRate <= 0) return Infinity;
  return (t.size * (1 - t.progress)) / t.downloadRate;
}

/** Infinity minus Infinity is NaN, which would break the sort. */
function byNumber(a: number, b: number): number {
  return a === b ? 0 : a < b ? -1 : 1;
}

function compare(a: TorrentSummary, b: TorrentSummary, key: SortKey): number {
  switch (key) {
    case "name":
      return collator.compare(a.name, b.name);
    case "label":
      // Unlabelled torrents sort after labelled ones.
      return a.label === b.label ? 0 : a.label === null ? 1 : b.label === null ? -1 : collator.compare(a.label, b.label);
    case "savePath":
      return collator.compare(a.savePath, b.savePath);
    case "eta":
      return byNumber(etaSeconds(a), etaSeconds(b));
    case "state":
      return collator.compare(a.state, b.state);
    case "ratio":
      return byNumber(ratio(a), ratio(b));
    case "peers":
      return a.peers - b.peers;
    default:
      return a[key] - b[key];
  }
}

export interface View {
  filter: StateFilter;
  label: string | null;
  search: string;
  sort: Sort;
}

/** The rows to show, in order. Ties fall back to name, then id, so the order is stable. */
export function visibleTorrents(torrents: TorrentMap, view: View): TorrentSummary[] {
  const needle = view.search.trim().toLocaleLowerCase();
  const rows = [...torrents.values()].filter(
    (t) =>
      matchesFilter(t, view.filter) &&
      (view.label === null || t.label === view.label) &&
      (needle === "" || t.name.toLocaleLowerCase().includes(needle)),
  );
  const dir = view.sort.descending ? -1 : 1;
  rows.sort(
    (a, b) =>
      dir * compare(a, b, view.sort.key) ||
      collator.compare(a.name, b.name) ||
      (a.id < b.id ? -1 : 1),
  );
  return rows;
}

export function countByFilter(torrents: TorrentMap): Record<StateFilter, number> {
  const counts: Record<StateFilter, number> = {
    all: 0,
    downloading: 0,
    seeding: 0,
    paused: 0,
    queued: 0,
    metadata: 0,
    checking: 0,
    error: 0,
    active: 0,
  };
  for (const t of torrents.values()) {
    counts.all++;
    counts[t.state]++;
    if (matchesFilter(t, "active")) counts.active++;
  }
  return counts;
}

export function labels(torrents: TorrentMap): Array<[string, number]> {
  const counts = new Map<string, number>();
  for (const t of torrents.values()) {
    if (t.label) counts.set(t.label, (counts.get(t.label) ?? 0) + 1);
  }
  return [...counts.entries()].sort((a, b) => collator.compare(a[0], b[0]));
}

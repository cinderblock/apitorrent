// The logic behind editing many torrents at once: what the selection has in common, and
// the `PATCH /torrents` body for the fields someone changed. Untouched fields are left out,
// so each torrent keeps its own value for them.

import type { BulkEdit } from "../api/generated/BulkEdit";
import type { BulkReport } from "../api/generated/BulkReport";
import type { SeedLimits } from "../api/generated/SeedLimits";
import type { TorrentSettings } from "../api/generated/TorrentSettings";
import type { UnwantedPolicy } from "../api/generated/UnwantedPolicy";
import { fromKiB } from "../format";

/** What a set of values has in common: one value, or several (how many different). */
export type Common<T> = { kind: "same"; value: T } | { kind: "mixed"; distinct: number } | { kind: "none" };

/** The value all of `values` share, or how many different ones there are. */
export function common<T>(values: readonly T[], key: (v: T) => string = (v) => JSON.stringify(v)): Common<T> {
  if (values.length === 0) return { kind: "none" };
  const seen = new Set(values.map(key));
  return seen.size === 1 ? { kind: "same", value: values[0]! } : { kind: "mixed", distinct: seen.size };
}

/** Each announce URL in the selection and how many of its torrents list it, most first. */
export function trackerCounts(settings: readonly TorrentSettings[]): Array<{ url: string; count: number }> {
  const counts = new Map<string, number>();
  for (const s of settings) {
    for (const url of new Set(s.trackers.flat())) counts.set(url, (counts.get(url) ?? 0) + 1);
  }
  return [...counts]
    .map(([url, count]) => ({ url, count }))
    .sort((a, b) => b.count - a.count || a.url.localeCompare(b.url));
}

/** Seed limits as a bulk edit sees them: follow the group/instance, or a torrent's own. */
export type SeedChoice = { kind: "keep" } | { kind: "follow" } | { kind: "own"; limits: SeedLimits };

/** The dialog's fields. `null` everywhere means "keep each torrent's own". */
export interface BulkForm {
  /** The new label; "" removes it. */
  label: string | null;
  /** KiB/s; 0 removes the torrent's own limit. */
  down: number | null;
  up: number | null;
  /** Peers at once; 0 is the server's default. */
  peers: number | null;
  seed: SeedChoice;
  /** URLs to drop from every torrent. */
  remove: ReadonlySet<string>;
  /** Old URL → new URL. */
  replace: ReadonlyMap<string, string>;
  /** URLs to add, one per line. */
  add: string;
  unwanted: UnwantedPolicy | null;
  /** The folder to move their files to. */
  location: string | null;
}

export const EMPTY_FORM: BulkForm = {
  label: null,
  down: null,
  up: null,
  peers: null,
  seed: { kind: "keep" },
  remove: new Set(),
  replace: new Map(),
  add: "",
  unwanted: null,
  location: null,
};

/** The URLs in a multi-line "add trackers" box: trimmed, blank lines dropped, each once. */
export function parseTrackerLines(text: string): string[] {
  return [...new Set(text.split(/\r?\n/).map((l) => l.trim()).filter((l) => l !== ""))];
}

/** The `PATCH /torrents` body for `ids` with the form's changed fields, or null if none. */
export function buildEdit(ids: readonly string[], f: BulkForm): BulkEdit | null {
  const edit: BulkEdit = { ids: [...ids] };
  let changed = false;
  if (f.label !== null) {
    edit.label = f.label.trim();
    changed = true;
  }
  if (f.down !== null || f.up !== null || f.peers !== null) {
    edit.limits = {};
    if (f.down !== null) edit.limits.downloadRate = fromKiB(f.down);
    if (f.up !== null) edit.limits.uploadRate = fromKiB(f.up);
    if (f.peers !== null) edit.limits.maxConnections = Math.max(0, Math.floor(f.peers));
    changed = true;
  }
  if (f.seed.kind !== "keep") {
    edit.seedLimits = { limits: f.seed.kind === "own" ? f.seed.limits : null };
    changed = true;
  }
  const replace = [...f.replace]
    .map(([from, to]) => ({ from, to: to.trim() }))
    .filter((r) => r.to !== "" && r.to !== r.from && !f.remove.has(r.from));
  const add = parseTrackerLines(f.add);
  if (replace.length > 0 || f.remove.size > 0 || add.length > 0) {
    edit.trackers = { replace, remove: [...f.remove], add };
    changed = true;
  }
  if (f.unwanted !== null) {
    edit.unwantedPolicy = f.unwanted;
    changed = true;
  }
  if (f.location !== null && f.location.trim() !== "") {
    edit.savePath = f.location.trim();
    changed = true;
  }
  return changed ? edit : null;
}

/** "Updated 37 torrents." or "Updated 35 of 37 torrents; 2 failed." */
export function describeReport(r: BulkReport): string {
  const total = r.updated.length + r.failed.length;
  const noun = total === 1 ? "torrent" : "torrents";
  if (r.failed.length === 0) return `Updated ${total} ${noun}.`;
  if (r.updated.length === 0) return `None of the ${total} ${noun} changed.`;
  return `Updated ${r.updated.length} of ${total} ${noun}; ${r.failed.length} failed.`;
}

/**
 * Run `action` on each id, a few at a time, and report which failed and why. For the
 * actions that have no bulk endpoint (pause, resume, remove, queue).
 */
export async function eachOf(ids: readonly string[], action: (id: string) => Promise<unknown>, parallel = 8): Promise<BulkReport> {
  const report: BulkReport = { updated: [], failed: [] };
  let next = 0;
  async function worker() {
    while (next < ids.length) {
      const id = ids[next++]!;
      try {
        await action(id);
        report.updated.push(id);
      } catch (e) {
        const code = typeof e === "object" && e !== null && "code" in e ? String(e.code) : "error";
        report.failed.push({ id, code, message: e instanceof Error ? e.message : String(e) });
      }
    }
  }
  await Promise.all(Array.from({ length: Math.min(parallel, ids.length) }, worker));
  return report;
}

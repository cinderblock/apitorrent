// The import wizard's filters and choices, apart from the dialog so they can be tested.
// The matching follows the server's `deluge_import::matches`: a filter field that is set
// must match, and a list matches when any entry does.

import type { DelugePreviewTorrent } from "../api/generated/DelugePreviewTorrent";

export interface WizardFilter {
  /** Labels to show; empty shows every label. `""` is "no label". */
  labels: string[];
  /** One tracker host, or "" for any. `example.org` also matches its subdomains. */
  tracker: string;
  state: "any" | "complete" | "incomplete";
  deluge: "any" | "paused" | "running";
  /** Words that must all appear in the name, ignoring case. */
  name: string;
}

export const NO_FILTER: WizardFilter = { labels: [], tracker: "", state: "any", deluge: "any", name: "" };

/** `host` is `wanted` or one of its subdomains. */
export function hostMatches(host: string, wanted: string): boolean {
  const h = host.toLowerCase();
  const w = wanted.toLowerCase().replace(/^\.+/, "");
  return h === w || h.endsWith(`.${w}`);
}

export function complete(t: DelugePreviewTorrent): boolean {
  return t.progress !== null && t.progress >= 1;
}

export function matches(t: DelugePreviewTorrent, f: WizardFilter): boolean {
  if (f.labels.length > 0) {
    const label = (t.label ?? "").toLowerCase();
    if (!f.labels.some((l) => l.toLowerCase() === label)) return false;
  }
  if (f.tracker && !t.trackers.some((h) => hostMatches(h, f.tracker))) return false;
  if (f.state !== "any" && complete(t) !== (f.state === "complete")) return false;
  if (f.deluge !== "any" && t.paused !== (f.deluge === "paused")) return false;
  const words = f.name.toLowerCase().split(/\s+/).filter((w) => w !== "");
  if (words.length > 0) {
    const name = t.name.toLowerCase();
    if (!words.every((w) => name.includes(w))) return false;
  }
  return true;
}

/** Whether the torrent can be ticked: not already in the space, and no problem. */
export function importable(t: DelugePreviewTorrent): boolean {
  return !t.present && t.problem === null;
}

/**
 * Storage roots for a new space that holds `paths`: each save path, without the ones
 * inside another. Separators are compared as they are; the server checks again.
 */
export function rootsFor(paths: string[]): string[] {
  const norm = (p: string) => p.replace(/[\\/]+$/, "");
  const unique = [...new Set(paths.map(norm).filter((p) => p !== ""))].sort();
  const inside = (p: string, root: string) => p !== root && (p.startsWith(`${root}/`) || p.startsWith(`${root}\\`));
  return unique.filter((p) => !unique.some((r) => inside(p, r)));
}

/** `path` is one of `roots` or inside one; with no roots, anything is. */
export function within(path: string, roots: string[]): boolean {
  const norm = (p: string) => p.replace(/[\\/]+$/, "");
  const p = norm(path);
  return roots.length === 0 || roots.map(norm).some((r) => p === r || p.startsWith(`${r}/`) || p.startsWith(`${r}\\`));
}

/** A slug the server accepts: lowercase letters, digits and inner hyphens, at most 40. */
export function validSlug(slug: string): boolean {
  return /^[a-z0-9](?:[a-z0-9-]{0,38}[a-z0-9])?$/.test(slug);
}

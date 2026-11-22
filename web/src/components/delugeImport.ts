// The import wizard's filters and choices, apart from the dialog so they can be tested.
// The matching follows the server's `deluge_import::matches`: a filter field that is set
// must match, and a list matches when any entry does.

import type { DelugeImportOptions } from "../api/generated/DelugeImportOptions";
import type { DelugePreviewTorrent } from "../api/generated/DelugePreviewTorrent";
import type { DelugeSettingsView } from "../api/generated/DelugeSettingsView";
import type { StopAtRatio } from "../api/generated/StopAtRatio";
import { formatRate } from "../format";

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

// What comes over: the import's options, and what the ticked torrents have of each.


/** The server's default: everything per torrent, none of Deluge's own settings. */
export const DEFAULT_OPTIONS: DelugeImportOptions = {
  labels: true,
  labelRenames: [],
  queueOrder: true,
  addedDates: true,
  totals: true,
  skippedFiles: true,
  trackers: true,
  torrentLimits: true,
  seedLimits: true,
  moveCompleted: true,
  sequential: true,
  firstLast: true,
  superSeed: true,
  identity: { kind: "deluge" },
  identitySkipHosts: [],
  settings: { downloadRate: false, uploadRate: false, maxConnections: false, queueSlots: false, ignoreSlow: false, stopAtRatio: false, encryption: false, discovery: false },
};

const sameRatio = (a: StopAtRatio | null, b: StopAtRatio | null) => (a === null || b === null ? a === b : a.ratio === b.ratio && a.remove === b.remove);

/** What the ticked torrents carry, option by option. */
export interface Carried {
  torrents: number;
  size: number;
  /** Deluge label (`""`: none) → how many. Sorted by label. */
  labels: [string, number][];
  downloaded: number;
  uploaded: number;
  /** With seeding time or totals at all. */
  withTotals: number;
  withSkippedFiles: number;
  skippedFiles: number;
  renamedFiles: number;
  withRenamedFiles: number;
  editedTrackers: number;
  withLimits: number;
  /** Their own stop-at-ratio, different from Deluge's global one. */
  ownRatio: number;
  /** Moved somewhere by Deluge when they complete. */
  moveCompleted: number;
  /** Downloaded in order in Deluge. */
  sequential: number;
  /** First and last pieces first in Deluge. */
  firstLast: number;
  /** Super-seeding in Deluge. */
  superSeed: number;
  /** Private trackers' hosts → how many torrents. */
  privateHosts: [string, number][];
  /** A Deluge option that doesn't carry over → how many torrents have it. */
  notCarried: [string, number][];
  /** Torrents running in Deluge. */
  running: number;
  /** `.torrent` files Deluge left but doesn't list. */
  leftovers: number;
}

export function carried(torrents: DelugePreviewTorrent[], global: StopAtRatio | null): Carried {
  const count = <K>(m: Map<K, number>, k: K) => m.set(k, (m.get(k) ?? 0) + 1);
  const labels = new Map<string, number>();
  const hosts = new Map<string, number>();
  const notes = new Map<string, number>();
  const c: Carried = {
    torrents: torrents.length,
    size: 0,
    labels: [],
    downloaded: 0,
    uploaded: 0,
    withTotals: 0,
    withSkippedFiles: 0,
    skippedFiles: 0,
    renamedFiles: 0,
    withRenamedFiles: 0,
    editedTrackers: 0,
    withLimits: 0,
    ownRatio: 0,
    moveCompleted: 0,
    sequential: 0,
    firstLast: 0,
    superSeed: 0,
    privateHosts: [],
    notCarried: [],
    running: 0,
    leftovers: 0,
  };
  for (const t of torrents) {
    c.size += t.size;
    count(labels, t.label ?? "");
    c.downloaded += t.downloaded;
    c.uploaded += t.uploaded;
    if (t.downloaded > 0 || t.uploaded > 0 || t.seedingSecs > 0) c.withTotals++;
    if (t.skippedFiles > 0) c.withSkippedFiles++;
    c.skippedFiles += t.skippedFiles;
    if (t.renamedFiles > 0) c.withRenamedFiles++;
    c.renamedFiles += t.renamedFiles;
    if (t.editedTrackers) c.editedTrackers++;
    if (t.limits.downloadRate > 0 || t.limits.uploadRate > 0 || t.limits.maxConnections !== null || t.limits.uploadSlots !== null) c.withLimits++;
    if (!sameRatio(t.stopAtRatio, global)) c.ownRatio++;
    if (t.moveCompleted) c.moveCompleted++;
    if (t.sequential) c.sequential++;
    if (t.firstLast) c.firstLast++;
    if (t.superSeed) c.superSeed++;
    if (t.private) for (const h of t.trackers) count(hosts, h);
    for (const n of t.notCarried) count(notes, n);
    if (!t.paused) c.running++;
    if (t.leftover) c.leftovers++;
  }
  const byKey = <V>(m: Map<string, V>) => [...m.entries()].sort(([a], [b]) => a.localeCompare(b));
  c.labels = byKey(labels);
  c.privateHosts = byKey(hosts);
  c.notCarried = byKey(notes);
  return c;
}

/** What a Deluge label becomes here: itself, a new name, or none (null). */
export function labelAfter(o: DelugeImportOptions, label: string): string | null {
  if (label === "" || !o.labels) return null;
  const r = o.labelRenames.find((x) => x.from.toLowerCase() === label.toLowerCase());
  if (!r) return label;
  const to = r.to?.trim() ?? "";
  return to === "" ? null : to;
}

/** Options with `label` renamed to `to` (null or empty: dropped; `label` itself: unchanged). */
export function renameLabel(o: DelugeImportOptions, label: string, to: string | null): DelugeImportOptions {
  const rest = o.labelRenames.filter((x) => x.from.toLowerCase() !== label.toLowerCase());
  return { ...o, labelRenames: to === label ? rest : [...rest, { from: label, to }] };
}

/** One of Deluge's settings, as the options name it. */
export type SettingKey = keyof DelugeImportOptions["settings"];

/** Each of Deluge's settings in words, for the tick boxes. */
const ENCRYPTION: Record<DelugeSettingsView["encryption"], string> = {
  disabled: "off",
  enabled: "on when the peer can",
  preferPlaintext: "the handshake only, when the peer allows",
  required: "required",
};

export type SettingRow = {
  key: SettingKey;
  label: string;
  value: string;
  /** Changed in Deluge from Deluge's default. */
  changed: boolean;
  note?: string;
};

/**
 * Deluge's settings an import can take, as rows: the instance-wide ones (slow torrents,
 * encryption) only for the default space.
 */
export function settingRows(s: DelugeSettingsView, defaultSpace: boolean): SettingRow[] {
  const rate = (b: number | null) => (b === null || b === 0 ? "no limit" : `${formatRate(b)}`);
  const n = (v: number | null) => (v === null ? "no limit" : String(v));
  const row = (key: SettingKey, label: string, value: string, note?: string): SettingRow => ({
    key,
    label,
    value,
    changed: s.changed[key],
    ...(note ? { note } : {}),
  });
  const rows: SettingRow[] = [
    row("downloadRate", "Download speed", rate(s.downloadRate)),
    row("uploadRate", "Upload speed", rate(s.uploadRate)),
    row("maxConnections", "Connections", n(s.maxConnections)),
    row("queueSlots", "Active torrents", `${n(s.maxActiveDownloading)} downloading, ${n(s.maxActiveSeeding)} seeding, ${n(s.maxActive)} in all`),
  ];
  if (defaultSpace) rows.push(row("ignoreSlow", "Slow torrents", s.dontCountSlow ? "don't count against the limits" : "count"));
  rows.push(row("stopAtRatio", "Stop seeding at ratio", s.stopAtRatio === null ? "off" : `${s.stopAtRatio.ratio}, then ${s.stopAtRatio.remove ? "remove" : "pause"}`));
  if (defaultSpace) {
    rows.push(row("encryption", "Encryption", ENCRYPTION[s.encryption], s.encryptionNote ?? undefined));
    const d = s.discovery;
    const onOff = (on: boolean) => (on ? "on" : "off");
    rows.push(
      row(
        "discovery",
        "Peer discovery and port forwarding",
        `DHT ${onOff(d.dht)}, local discovery ${onOff(d.lsd)}, UPnP ${onOff(d.upnp)}, NAT-PMP ${onOff(d.natpmp)}`,
        "Applies when the daemon next restarts.",
      ),
    );
  }
  return rows;
}

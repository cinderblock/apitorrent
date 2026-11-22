// The Settings page's model: its sections, which of them have unsaved changes, what's wrong
// with a draft before it is saved, the nav filter, and rate units. No React here.

import type { Settings } from "../api/generated/Settings";
import { identityProblems } from "../components/clientIdentity";

export type SectionId =
  | "transfers"
  | "seeding"
  | "groups"
  | "network"
  | "identity"
  | "bans"
  | "files"
  | "library"
  | "https"
  | "updates"
  | "passkeys"
  | "deluge"
  | "appearance";

export type SectionGroup = "server" | "space" | "device";

/** What the person may see and do here. */
export type Access = {
  /** Changes the instance's settings; sees the server-only sections. */
  serverAdmin: boolean;
  /** Administers this space: its passkeys and the Deluge import. */
  spaceAdmin: boolean;
};

export type Section = {
  id: SectionId;
  title: string;
  group: SectionGroup;
  /** One line under the title, and the nav's second line on a phone. */
  summary: string;
  /** More words the nav filter matches (field labels, synonyms). */
  keywords: string;
  /** The `Settings` fields this section edits, saved with the Save bar. Empty: it acts at once. */
  fields: readonly (keyof Settings)[];
  visible: (a: Access) => boolean;
};

const always = () => true;

export const SECTIONS: readonly Section[] = [
  {
    id: "transfers",
    title: "Transfers",
    group: "server",
    summary: "Speed limits and how many torrents run at once",
    keywords: "bandwidth download upload rate speed limit queue active slow torrents",
    fields: ["downloadRate", "uploadRate", "queue"],
    visible: always,
  },
  {
    id: "seeding",
    title: "Seeding",
    group: "server",
    summary: "When a finished torrent has shared enough",
    keywords: "ratio seed time hours pause remove limits",
    fields: ["seedLimits"],
    visible: always,
  },
  {
    id: "groups",
    title: "Groups",
    group: "server",
    summary: "Shared limits for torrents by label",
    keywords: "labels group limits bandwidth connections seeding ratio",
    fields: ["groups"],
    visible: always,
  },
  {
    id: "network",
    title: "Network",
    group: "server",
    summary: "Ports, connections and encryption",
    keywords: "peer port dht udp tcp utp connections half open connecting encryption mse obfuscation",
    fields: ["peerPort", "dhtPort", "maxConnections", "maxHalfOpen", "connectRate", "encryption"],
    visible: always,
  },
  {
    id: "identity",
    title: "Client identity",
    group: "server",
    summary: "Which client trackers and peers see",
    keywords: "peer id user agent deluge qbittorrent transmission tracker spoof present as whitelist",
    fields: ["identity"],
    visible: always,
  },
  {
    id: "bans",
    title: "Banned peers",
    group: "server",
    summary: "Peers that sent bad data, or that you banned",
    keywords: "ban block ip address unban",
    fields: [],
    visible: always,
  },
  {
    id: "files",
    title: "Files",
    group: "server",
    summary: "File names, download order, moving finished files, and where other computers find downloads",
    keywords: "naming portable native names storage roots share smb nfs unc network path locators web http https nginx download links order sequential first move completed finished public destination rules",
    fields: ["naming", "fileOrder", "moveCompleted"],
    visible: always,
  },
  {
    id: "library",
    title: "Library",
    group: "server",
    summary: "Reuse, share and deduplicate files across torrents",
    keywords: "reuse hard link copy dedupe deduplicate identical fingerprint share folders media repack partial",
    fields: ["library"],
    visible: always,
  },
  {
    id: "https",
    title: "HTTPS",
    group: "server",
    summary: "The server's certificate and pairing code",
    keywords: "tls certificate acme pairing code ssl",
    fields: [],
    visible: always,
  },
  {
    id: "updates",
    title: "Updates",
    group: "server",
    summary: "New releases of apitorrent",
    keywords: "update upgrade version release install automatic",
    fields: [],
    visible: (a) => a.serverAdmin,
  },
  {
    id: "passkeys",
    title: "Tracker passkeys",
    group: "space",
    summary: "Change a private tracker's passkey on every torrent",
    keywords: "passkey private tracker announce token",
    fields: [],
    visible: (a) => a.spaceAdmin,
  },
  {
    id: "deluge",
    title: "Deluge",
    group: "space",
    summary: "Import torrents from Deluge, and take them over",
    keywords: "deluge import migrate take over",
    fields: [],
    visible: (a) => a.spaceAdmin,
  },
  {
    id: "appearance",
    title: "Appearance",
    group: "device",
    summary: "Light or dark, and where details open, on this device",
    keywords: "theme dark light mode colour color details panel window popup",
    fields: [],
    visible: always,
  },
];

export function sectionById(id: string): Section | undefined {
  return SECTIONS.find((s) => s.id === id);
}

export function visibleSections(a: Access): Section[] {
  return SECTIONS.filter((s) => s.visible(a));
}

/** Sections where every word of the query starts a word of the title, summary or keywords. */
export function filterSections(sections: readonly Section[], query: string): Section[] {
  const words = query.toLowerCase().split(/[^\p{L}\p{N}]+/u).filter((w) => w !== "");
  if (words.length === 0) return [...sections];
  return sections.filter((s) => {
    const hay = `${s.title} ${s.summary} ${s.keywords}`.toLowerCase().split(/[^\p{L}\p{N}]+/u);
    return words.every((w) => hay.some((h) => h.startsWith(w)));
  });
}

function same(a: unknown, b: unknown): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

/** The sections with unsaved changes, in nav order. */
export function dirtySections(saved: Settings, draft: Settings): SectionId[] {
  return SECTIONS.filter((s) => s.fields.some((f) => !same(saved[f], draft[f]))).map((s) => s.id);
}

export type Problem = { section: SectionId; message: string };

/** What has to be fixed before the draft can be saved: the daemon's own checks
    (`settings::validate`), so the page can point at the section. */
export function draftProblems(draft: Settings): Problem[] {
  const out: Problem[] = [];
  if (draft.peerPort !== 0 && draft.peerPort === draft.dhtPort) {
    out.push({ section: "network", message: "The peer port and the DHT port must differ." });
  }
  const names = new Set<string>();
  const labels = new Map<string, string>();
  draft.groups.forEach((g, i) => {
    const name = g.name.trim();
    if (name === "") out.push({ section: "groups", message: `Group ${i + 1} needs a name.` });
    else if (names.has(name)) out.push({ section: "groups", message: `Two groups are called "${name}".` });
    names.add(name);
    for (const l of g.labels) {
      const other = labels.get(l);
      if (other !== undefined) out.push({ section: "groups", message: `Label "${l}" is in two groups (${other} and ${name || `group ${i + 1}`}).` });
      else labels.set(l, name || `group ${i + 1}`);
    }
  });
  for (const p of identityProblems(draft.identity)) out.push({ section: "identity", message: p });
  return out;
}

/** The draft as it is sent: names and hosts trimmed. */
export function forSaving(draft: Settings): Settings {
  return {
    ...draft,
    groups: draft.groups.map((g) => ({ ...g, name: g.name.trim() })),
    identity: { ...draft.identity, rules: draft.identity.rules.map((r) => ({ ...r, host: r.host.trim() })) },
  };
}

/** "Transfers and Network", "Transfers, Seeding and Network". */
export function listTitles(ids: readonly SectionId[]): string {
  const titles = ids.map((id) => sectionById(id)?.title ?? id);
  if (titles.length <= 1) return titles.join("");
  return `${titles.slice(0, -1).join(", ")} and ${titles[titles.length - 1]}`;
}

// Rates

export type RateUnit = "KiB" | "MiB";

export const RATE_UNIT_BYTES: Record<RateUnit, number> = { KiB: 1024, MiB: 1024 * 1024 };

/** The unit a stored rate reads best in: MiB/s from 1 MiB/s up when that's at most one decimal. */
export function pickRateUnit(bytesPerSecond: number): RateUnit {
  const mib = bytesPerSecond / RATE_UNIT_BYTES.MiB;
  return mib >= 1 && Math.abs(mib * 10 - Math.round(mib * 10)) < 1e-9 ? "MiB" : "KiB";
}

/** The number to show for a rate in a unit, at most two decimals. */
export function rateIn(bytesPerSecond: number, unit: RateUnit): number {
  return Math.round((bytesPerSecond / RATE_UNIT_BYTES[unit]) * 100) / 100;
}

/** Bytes per second from a typed number in a unit; whole KiB/s, as the daemon stores them. */
export function rateFrom(value: number, unit: RateUnit): number {
  if (!Number.isFinite(value) || value <= 0) return 0;
  return Math.max(1, Math.round((value * RATE_UNIT_BYTES[unit]) / 1024)) * 1024;
}

/** A typed number: empty or junk is 0, never negative. */
export function parseAmount(text: string): number {
  const n = Number(text.trim().replace(",", "."));
  return Number.isFinite(n) && n > 0 ? n : 0;
}

/** Labels typed comma-separated: trimmed, blanks and repeats dropped. */
export function parseLabels(text: string): string[] {
  const out: string[] = [];
  for (const raw of text.split(",")) {
    const l = raw.trim();
    if (l !== "" && !out.includes(l)) out.push(l);
  }
  return out;
}

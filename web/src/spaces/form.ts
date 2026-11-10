// A space's settings as the editor shows them (text fields), and back.

import type { SpaceSettings } from "../api/generated/SpaceSettings";

export interface SpaceForm {
  slug: string;
  name: string;
  description: string;
  /** One absolute directory per line. */
  roots: string;
  downloadDir: string;
  /** `label = folder`, one per line. */
  folderRules: string;
  /** KiB/s; empty for no limit. */
  downloadKib: string;
  uploadKib: string;
  maxConnections: string;
  maxActive: string;
  maxActiveDownloads: string;
  maxActiveSeeds: string;
}

const lines = (s: string) =>
  s
    .split(/\r?\n/)
    .map((l) => l.trim())
    .filter((l) => l !== "");

const num = (s: string) => {
  const n = Number(s.trim());
  return Number.isFinite(n) && n > 0 ? Math.round(n) : 0;
};

const shown = (n: number) => (n > 0 ? String(n) : "");

export function emptyForm(): SpaceForm {
  return {
    slug: "",
    name: "",
    description: "",
    roots: "",
    downloadDir: "",
    folderRules: "",
    downloadKib: "",
    uploadKib: "",
    maxConnections: "",
    maxActive: "",
    maxActiveDownloads: "",
    maxActiveSeeds: "",
  };
}

export function formOf(slug: string, name: string, description: string, s: SpaceSettings): SpaceForm {
  return {
    slug,
    name,
    description,
    roots: s.storageRoots.join("\n"),
    downloadDir: s.downloadDir ?? "",
    folderRules: s.folderRules.map((r) => `${r.label} = ${r.folder}`).join("\n"),
    downloadKib: shown(Math.round(s.downloadRate / 1024)),
    uploadKib: shown(Math.round(s.uploadRate / 1024)),
    maxConnections: shown(s.maxConnections),
    maxActive: shown(s.maxActive),
    maxActiveDownloads: shown(s.maxActiveDownloads),
    maxActiveSeeds: shown(s.maxActiveSeeds),
  };
}

/** The settings a form describes, or the first problem with it. */
export function settingsOf(f: SpaceForm): SpaceSettings | string {
  const folderRules = [];
  for (const l of lines(f.folderRules)) {
    const eq = l.indexOf("=");
    if (eq <= 0) return `Folder rules are "label = folder": ${l}`;
    folderRules.push({ label: l.slice(0, eq).trim(), folder: l.slice(eq + 1).trim() });
  }
  return {
    storageRoots: lines(f.roots),
    downloadDir: f.downloadDir.trim() || null,
    folderRules,
    downloadRate: num(f.downloadKib) * 1024,
    uploadRate: num(f.uploadKib) * 1024,
    maxConnections: num(f.maxConnections),
    maxActive: num(f.maxActive),
    maxActiveDownloads: num(f.maxActiveDownloads),
    maxActiveSeeds: num(f.maxActiveSeeds),
  };
}

/** A slug from a name: `My TV` → `my-tv`. */
export function slugFrom(name: string): string {
  return name
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 40)
    .replace(/-+$/, "");
}

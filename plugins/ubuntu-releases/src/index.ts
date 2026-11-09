/**
 * Ubuntu releases as a source: read Ubuntu's list of releases (`meta-release`), then each
 * supported release's download directory, and offer the `.iso.torrent` files for the
 * chosen flavours and architecture to the server's rules as release candidates (with the
 * version, release date, flavour and architecture for rules to match and order by). A rule
 * then keeps the latest N, supersedes older ones and can keep a "latest" alias. With
 * `addDirectly` on, each torrent file is also added once, as before rules existed.
 *
 * amd64 images live on releases.ubuntu.com/<codename>/, other architectures on
 * cdimage.ubuntu.com/releases/<codename>/release/.
 */

import { definePlugin, type Api, type ReleaseCandidate } from "@apitorrent/plugin-sdk";

export interface Settings {
  flavors: string[];
  arch: "amd64" | "arm64";
  channel: "lts" | "all";
  /** Also add each torrent straight away, without a rule. */
  addDirectly: boolean;
  label: string;
  savePath?: string;
  paused: boolean;
  intervalHours: number;
}

export interface Release {
  dist: string;
  name: string;
  version: string;
  lts: boolean;
  supported: boolean;
  /** The release date, ISO 8601, when `meta-release` gives one. */
  date?: string;
}

export const META_RELEASE = "https://changelogs.ubuntu.com/meta-release";

/** Parse `meta-release`: blank-line separated blocks of `Key: value` lines. */
export function parseMetaRelease(text: string): Release[] {
  return text
    .split(/\r?\n\s*\r?\n/)
    .map((block) => {
      const fields: Record<string, string> = {};
      for (const line of block.split(/\r?\n/)) {
        const i = line.indexOf(":");
        if (i > 0) fields[line.slice(0, i).trim()] = line.slice(i + 1).trim();
      }
      return fields;
    })
    .filter((f) => f.Dist && f.Version)
    .map((f) => ({
      dist: f.Dist!,
      name: f.Name ?? f.Dist!,
      version: f.Version!.replace(/\s*LTS$/, ""),
      lts: /LTS/.test(f.Version!),
      supported: f.Supported === "1",
      date: isoDate(f.Date),
    }));
}

/** `meta-release` dates are RFC 2822 (`Thu, 25 Apr 2024 15:00:00 UTC`). */
function isoDate(text: string | undefined): string | undefined {
  if (!text) return undefined;
  const t = Date.parse(text.replace(/\bUTC$/, "GMT"));
  return Number.isNaN(t) ? undefined : new Date(t).toISOString();
}

export function directoryFor(dist: string, arch: string): string {
  return arch === "amd64"
    ? `https://releases.ubuntu.com/${dist}/`
    : `https://cdimage.ubuntu.com/releases/${dist}/release/`;
}

/** The `.iso.torrent` links in a directory listing, for these flavours and architecture. */
export function torrentsIn(html: string, flavors: string[], arch: string): string[] {
  const names = new Set<string>();
  for (const m of html.matchAll(/href="([^"]+\.iso\.torrent)"/g)) {
    const name = m[1]!.split("/").pop()!;
    const parsed = parseName(name);
    if (parsed && flavors.includes(parsed.flavor) && parsed.arch === arch) names.add(name);
  }
  return [...names].sort();
}

/** `ubuntu-24.04.3-desktop-amd64.iso.torrent`: the version, flavour and architecture. */
export function parseName(name: string): { version: string; flavor: string; arch: string } | null {
  const m = /^ubuntu-([\d.]+)-(.+)-([a-z0-9]+)\.iso\.torrent$/.exec(name);
  return m ? { version: m[1]!, flavor: m[2]!, arch: m[3]! } : null;
}

async function check(api: Api<Settings>, found: Map<string, ReleaseCandidate>) {
  const s = api.settings.get();
  const res = await fetch(META_RELEASE, { signal: AbortSignal.timeout(60_000) });
  if (!res.ok) throw new Error(`meta-release answered ${res.status}`);
  const releases = parseMetaRelease(await res.text())
    .filter((r) => r.supported && (s.channel === "all" || r.lts));
  const offered: ReleaseCandidate[] = [];
  for (const r of releases) {
    const dir = directoryFor(r.dist, s.arch);
    const listing = await fetch(dir, { signal: AbortSignal.timeout(60_000) });
    if (!listing.ok) {
      console.warn(`${dir} answered ${listing.status}`);
      continue;
    }
    for (const name of torrentsIn(await listing.text(), s.flavors, s.arch)) {
      const parsed = parseName(name)!;
      const candidate: ReleaseCandidate = {
        id: name,
        title: `Ubuntu ${parsed.version}${r.lts ? " LTS" : ""} (${r.name}) ${parsed.flavor} ${parsed.arch}`,
        // The point release from the file name (24.04.3), not just the series.
        version: parsed.version,
        published: r.date,
        links: { torrent: new URL(name, dir).href },
        metadata: { dist: r.dist, codename: r.name, lts: r.lts, flavor: parsed.flavor, arch: parsed.arch },
      };
      found.set(name, candidate);
      offered.push(candidate);
    }
  }
  // Offer everything found to the rules; they keep what they want.
  const outcome = await api.candidates.submit(offered);
  for (const why of outcome.rejected) console.warn(`candidate refused: ${why}`);
  let added = 0;
  if (s.addDirectly) {
    for (const c of offered) {
      const key = `added:${c.id}`;
      if (await api.kv.get(key)) continue;
      const url = c.links.torrent!;
      const file = await fetch(url, { signal: AbortSignal.timeout(60_000) });
      if (!file.ok) {
        console.warn(`${url} answered ${file.status}`);
        continue;
      }
      const result = await api.torrents.add({
        torrent: await file.bytes(),
        label: s.label || undefined,
        savePath: s.savePath || undefined,
        paused: s.paused,
      });
      await api.kv.set(key, { at: Date.now(), id: result.id, duplicate: result.duplicate });
      if (!result.duplicate) {
        added++;
        console.info(`added ${c.id}`);
      }
    }
  }
  await api.kv.set("status", { lastCheck: Date.now(), lastError: null, offered: outcome.accepted, added });
}

export default definePlugin<Settings>({
  start(api) {
    const found = new Map<string, ReleaseCandidate>();
    const run = async () => {
      try {
        await check(api, found);
      } catch (e) {
        const message = (e as Error).message;
        console.error(message);
        await api.kv.set("status", { lastCheck: Date.now(), lastError: message, offered: 0, added: 0 });
      }
    };
    const schedule = (s: Settings) => api.schedule.every("check", { hours: s.intervalHours }, run);
    schedule(api.settings.get());
    api.settings.onChange(schedule);
    const status = async () => ({
      ...((await api.kv.get<object>("status")) ?? { lastCheck: null }),
      releases: [...found.keys()],
    });
    api.http.route("GET", "/status", status);
    api.http.route("POST", "/check", async () => {
      await api.schedule.runNow("check");
      return status();
    });
    api.roles.provide("source", {
      candidates: (limit = 100) => [...found.values()].slice(0, limit),
    });
  },
});

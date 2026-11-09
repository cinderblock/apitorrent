/**
 * RSS, Atom and JSON feeds: poll each feed, keep the items whose titles pass its filters,
 * and add their magnets or .torrent files. Items are deduplicated by guid, per feed, so an
 * item is added once however often the feed repeats it. A newly added feed's existing
 * items are only marked seen unless "backfill" is on, so adding a feed doesn't flood the
 * client.
 *
 * The plugin may fetch only its feeds' own origins, plus the hosts an admin lists in
 * `downloadHosts` (for .torrent links served elsewhere): both come from its settings
 * through `networkFromSettings`.
 */

import { definePlugin, type Api, type ReleaseCandidate } from "@apitorrent/plugin-sdk";

import { decodeFeed, parseFeed, type FeedItem } from "./feed";

export interface FeedSettings {
  name?: string;
  url: string;
  enabled: boolean;
  /** A regular expression the title must match (case-insensitive); empty matches all. */
  include: string;
  /** A regular expression that rejects a title (case-insensitive); empty rejects none. */
  exclude: string;
  label?: string;
  savePath?: string;
  paused: boolean;
}

export interface Settings {
  feeds: FeedSettings[];
  intervalMinutes: number;
  backfill: boolean;
  downloadHosts: string[];
}

/** What the plugin remembers per feed (in its KV store). */
export interface FeedState {
  /** Hashes of seen guids, newest first. */
  seen: string[];
  initialized: boolean;
  etag?: string;
  lastModified?: string;
  lastPoll?: number;
  lastError?: string | null;
  added: number;
}

/** How many guids are remembered per feed; feeds rarely list more than a few hundred. */
const MAX_SEEN = 5000;

async function sha256Hex(text: string): Promise<string> {
  const d = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return Array.from(new Uint8Array(d), (b) => b.toString(16).padStart(2, "0")).join("");
}

async function feedKey(url: string): Promise<string> {
  return `feed:${(await sha256Hex(url)).slice(0, 24)}`;
}

function compile(pattern: string, what: string): RegExp | null {
  if (!pattern.trim()) return null;
  try {
    return new RegExp(pattern, "iu");
  } catch (e) {
    throw new Error(`the ${what} filter isn't a valid regular expression: ${(e as Error).message}`);
  }
}

export function passes(title: string, feed: Pick<FeedSettings, "include" | "exclude">): boolean {
  const include = compile(feed.include ?? "", "include");
  const exclude = compile(feed.exclude ?? "", "exclude");
  return (!include || include.test(title)) && !(exclude && exclude.test(title));
}

/** Add one item; returns whether it was handled (added, a duplicate, or nothing to add). */
async function addItem(api: Api<Settings>, feed: FeedSettings, item: FeedItem): Promise<boolean> {
  const common = {
    label: feed.label || undefined,
    savePath: feed.savePath || undefined,
    paused: feed.paused,
  };
  if (item.magnet) {
    await api.torrents.add({ magnet: item.magnet, ...common });
    return true;
  }
  if (item.torrentUrl) {
    const url = new URL(item.torrentUrl, feed.url).href;
    const res = await fetch(url, { signal: AbortSignal.timeout(60_000) });
    if (!res.ok) throw new Error(`${url} answered ${res.status}`);
    await api.torrents.add({ torrent: await res.bytes(), ...common });
    return true;
  }
  console.warn(`"${item.title}" has no magnet or .torrent link; skipped`);
  return true;
}

export async function pollFeed(
  api: Api<Settings>,
  feed: FeedSettings,
  backfill: boolean,
  recent: Map<string, ReleaseCandidate>,
): Promise<FeedState> {
  const key = await feedKey(feed.url);
  const state: FeedState = (await api.kv.get<FeedState>(key)) ??
    { seen: [], initialized: false, added: 0 };
  state.lastPoll = Date.now();
  try {
    const headers: Record<string, string> = { accept: "application/rss+xml, application/atom+xml, application/feed+json, application/xml;q=0.9, */*;q=0.5" };
    if (state.etag) headers["if-none-match"] = state.etag;
    if (state.lastModified) headers["if-modified-since"] = state.lastModified;
    const res = await fetch(feed.url, { headers, signal: AbortSignal.timeout(60_000) });
    if (res.status === 304) {
      state.lastError = null;
      return state;
    }
    if (!res.ok) throw new Error(`the feed answered ${res.status} ${res.statusText}`);
    const items = parseFeed(decodeFeed(await res.bytes(), res.headers.get("content-type"))).items;
    // Validate both filters before touching anything.
    passes("", feed);
    const seen = new Set(state.seen);
    const fresh: string[] = [];
    let failed = 0;
    // Feeds list the newest first; add oldest first.
    for (const item of [...items].reverse()) {
      recent.set(item.guid, {
        id: item.guid,
        title: item.title,
        published: item.published,
        links: { magnet: item.magnet, torrent: item.torrentUrl },
        metadata: { feed: feed.name ?? feed.url, size: item.size },
      });
      const id = (await sha256Hex(item.guid)).slice(0, 32);
      if (seen.has(id)) continue;
      const take = (state.initialized || backfill) && passes(item.title, feed);
      if (take) {
        try {
          await addItem(api, feed, item);
          state.added++;
        } catch (e) {
          // Not marked seen: it is tried again next time.
          console.warn(`couldn't add "${item.title}": ${(e as Error).message}`);
          failed++;
          continue;
        }
      }
      seen.add(id);
      fresh.push(id);
    }
    state.seen = [...fresh.reverse(), ...state.seen].slice(0, MAX_SEEN);
    state.initialized = true;
    // With items left to retry, fetch the whole feed next time rather than a 304.
    state.etag = failed ? undefined : res.headers.get("etag") ?? undefined;
    state.lastModified = failed ? undefined : res.headers.get("last-modified") ?? undefined;
    state.lastError = null;
  } catch (e) {
    state.lastError = (e as Error).message;
    console.error(`${feed.name ?? feed.url}: ${state.lastError}`);
  } finally {
    await api.kv.set(key, state);
  }
  return state;
}

export default definePlugin<Settings>({
  start(api) {
    const recent = new Map<string, ReleaseCandidate>();
    const pollAll = async () => {
      const s = api.settings.get();
      for (const feed of s.feeds) {
        if (feed.enabled) await pollFeed(api, feed, s.backfill, recent);
      }
      // Bound the candidate list.
      while (recent.size > 1000) recent.delete(recent.keys().next().value!);
    };
    const schedule = (s: Settings) =>
      api.schedule.every("poll", { minutes: s.intervalMinutes }, pollAll);
    schedule(api.settings.get());
    api.settings.onChange(schedule);

    const status = async () => {
      const feeds = [];
      for (const feed of api.settings.get().feeds) {
        const state = await api.kv.get<FeedState>(await feedKey(feed.url));
        feeds.push({
          name: feed.name ?? feed.url,
          url: feed.url,
          enabled: feed.enabled,
          lastPoll: state?.lastPoll ?? null,
          lastError: state?.lastError ?? null,
          added: state?.added ?? 0,
          seen: state?.seen.length ?? 0,
        });
      }
      return { feeds };
    };
    api.http.route("GET", "/status", status);
    api.http.route("POST", "/poll", async () => {
      await api.schedule.runNow("poll");
      return status();
    });
    api.roles.provide("source", {
      candidates: (limit = 100) => [...recent.values()].reverse().slice(0, limit),
    });
  },
});

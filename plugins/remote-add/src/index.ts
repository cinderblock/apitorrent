/**
 * Remote add: `GET /add?key=<key>&url=<.torrent link>` adds a torrent. The plugin fetches
 * the `.torrent` itself, so it never passes through the browser that opened the link.
 *
 * Made for links injected into a tracker's pages by the user's own JavaScript there: that
 * script can only open a URL. It can't send an `Authorization` header to another site, and
 * apitorrent refuses cross-site POSTs, so the key rides in the query string. That puts it
 * in proxy logs and in script the tracker's staff can read, so a key is narrow on purpose:
 * it can only add, and only `.torrent` files from the hosts in the settings (the plugin's
 * network grants). No magnet links, which could point anywhere.
 *
 * Replies are for a person looking at a popup (a short HTML page), or, with
 * `reply=image`, for the page's script: a 1×1 GIF when the torrent was added (or was
 * already there), an error status otherwise, so an `<img>`'s `load` and `error` events
 * tell the two apart. Plugin pages are served sandboxed, without script, so a popup can't
 * close itself; the image reply lets the tracker page show the result without one.
 */

import { definePlugin, isApitorrentError, isNotCapable, type Api } from "@apitorrent/plugin-sdk";

export interface Settings {
  key?: string;
  hosts: string[];
  label: string;
  savePath: string;
  paused: boolean;
}

/** The largest `.torrent` fetched. */
export const MAX_TORRENT_BYTES = 16 * 1024 * 1024;
const MAX_REDIRECTS = 5;

/** The result of one add, before it's put into a reply. */
export interface Outcome {
  status: number;
  /** For the page's title and the log. */
  summary: string;
  /** A sentence for the person reading the page. */
  detail: string;
}

const outcome = (status: number, summary: string, detail: string): Outcome => ({ status, summary, detail });

/** Compare keys without leaking, through timing, how much of a wrong key was right. */
export async function sameKey(given: string, expected: string): Promise<boolean> {
  const digest = async (s: string) =>
    new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(s)));
  const [a, b] = await Promise.all([digest(given), digest(expected)]);
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a[i]! ^ b[i]!;
  return diff === 0;
}

type Fetched = { torrent: Uint8Array } | Outcome;

/** Fetch a `.torrent`, following redirects; each hop must be on a granted host. */
async function fetchTorrent(link: URL): Promise<Fetched> {
  let url = link;
  for (let hop = 0; hop <= MAX_REDIRECTS; hop++) {
    if (url.protocol !== "https:" && url.protocol !== "http:") {
      return outcome(400, "Not a web link", `Only http and https links can be fetched, not ${url.protocol}`);
    }
    let r: Response;
    try {
      r = await fetch(url, { redirect: "manual" });
    } catch (e) {
      if (isNotCapable(e)) {
        return outcome(
          403,
          "Host not allowed",
          `${url.host} isn't in this plugin's hosts setting, so links there can't be fetched.`,
        );
      }
      console.warn(`could not fetch from ${url.host}: ${e}`);
      return outcome(502, "Fetch failed", `Could not reach ${url.host}.`);
    }
    const location = r.headers.get("location");
    if (r.status >= 300 && r.status < 400 && location) {
      url = new URL(location, url);
      continue;
    }
    if (!r.ok) {
      console.warn(`${url.host} answered HTTP ${r.status}`);
      return outcome(502, "Fetch failed", `${url.host} answered HTTP ${r.status}.`);
    }
    const length = Number(r.headers.get("content-length") ?? "0");
    if (length > MAX_TORRENT_BYTES) return tooLarge();
    const bytes = await r.bytes();
    if (bytes.length > MAX_TORRENT_BYTES) return tooLarge();
    // A torrent is a bencoded dictionary. Anything else is most likely a login or error
    // page, which says more than "invalid torrent" would.
    if (bytes[0] !== 0x64 /* d */) {
      const type = r.headers.get("content-type")?.split(";")[0] ?? "something else";
      return outcome(
        502,
        "Not a torrent",
        `${url.host} sent ${type}, not a .torrent file. Is the link one that works without signing in (with your passkey in it)?`,
      );
    }
    return { torrent: bytes };
  }
  return outcome(502, "Too many redirects", `The link redirected more than ${MAX_REDIRECTS} times.`);
}

const tooLarge = () =>
  outcome(502, "Too large", `The file is larger than ${MAX_TORRENT_BYTES / 1024 / 1024} MiB; not a .torrent.`);

/** Check the request, fetch the torrent and add it. */
export async function addFromLink(api: Api<Settings>, params: URLSearchParams): Promise<Outcome> {
  const settings = api.settings.get();
  if (!settings.key) {
    return outcome(503, "Not set up", "Set a key in this plugin's settings first.");
  }
  const key = params.get("key");
  if (!key || !(await sameKey(key, settings.key))) {
    return outcome(403, "Wrong key", "The key in the link is missing or wrong.");
  }
  const raw = params.get("url")?.trim();
  if (!raw) return outcome(400, "No link", "Give the .torrent link as `url`.");
  if (/^magnet:/i.test(raw)) {
    return outcome(400, "Magnets not accepted", "Give the link to the .torrent file.");
  }
  const link = URL.parse(raw);
  if (!link) return outcome(400, "Not a link", "`url` isn't a link.");

  const fetched = await fetchTorrent(link);
  if (!("torrent" in fetched)) return fetched;

  try {
    const r = await api.torrents.add({
      torrent: fetched.torrent,
      label: settings.label || undefined,
      savePath: settings.savePath || undefined,
      paused: settings.paused,
    });
    if (r.duplicate) return outcome(200, "Already added", "apitorrent already has this torrent.");
    // Only the host: links often carry a passkey.
    console.info(`added ${r.id} from ${link.host}`);
    return outcome(200, "Added", "The torrent was added.");
  } catch (e) {
    if (isApitorrentError(e) && (e.code === "invalid_torrent" || e.code === "invalid_request")) {
      return outcome(422, "Not a valid torrent", e.message);
    }
    throw e;
  }
}

const escape = (s: string) =>
  s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);

/** The smallest transparent GIF. */
const PIXEL = Uint8Array.from(
  atob("R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7"),
  (c) => c.charCodeAt(0),
);

const NO_STORE = { "cache-control": "no-store", "referrer-policy": "no-referrer" };

export function reply(o: Outcome, asImage: boolean): Response {
  if (asImage) {
    return o.status < 300
      ? new Response(PIXEL, { headers: { ...NO_STORE, "content-type": "image/gif" } })
      : new Response(o.summary, { status: o.status, headers: { ...NO_STORE, "content-type": "text/plain" } });
  }
  const body = `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<title>${escape(o.summary)} · apitorrent</title></head>
<body><h1>${o.status < 300 ? "✓" : "✗"} ${escape(o.summary)}</h1><p>${escape(o.detail)}</p></body></html>`;
  return new Response(body, {
    status: o.status,
    headers: { ...NO_STORE, "content-type": "text/html; charset=utf-8" },
  });
}

export default definePlugin<Settings>({
  start(api) {
    api.http.route("GET", "/add", async (request) => {
      const params = new URL(request.url).searchParams;
      const o = await addFromLink(api, params);
      if (o.status >= 300) console.warn(`refused an add: ${o.summary}`);
      return reply(o, params.get("reply") === "image");
    });
    // `auth: plugin` routes need the role; there's nothing for the host to call.
    api.roles.provide("api-compat", {});
  },
});

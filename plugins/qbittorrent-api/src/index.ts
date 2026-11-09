/**
 * qBittorrent's Web API v2, so Sonarr, Radarr, Prowlarr, autobrr, cross-seed and other
 * qBittorrent clients work with apitorrent unchanged.
 *
 * **Where.** Every route is under `/api/v2/`, below the plugin's prefix: clients take
 * `/api/v1/plugins/qbittorrent-api` as their URL base. For a client that can't set one,
 * the daemon can mount the plugin at the root of a port of its own
 * (`--plugin-listen qbittorrent-api=127.0.0.1:8080`).
 *
 * **Sign-in.** `auth/login` takes any username and an apitorrent API token as the
 * password, and answers with qBittorrent's `SID` cookie. The token is checked by
 * apitorrent; this plugin only ever holds the session id. Sessions last an hour unused
 * and end at once when the token is revoked or expires. A read-only token (or a viewer's)
 * can read but not change anything: writes answer 403. A request that signed in to
 * apitorrent itself (a bearer token, or a loopback request with authentication off) needs
 * no login.
 *
 * **Categories are labels.** A category's save path, and tags, are kept by this plugin.
 * Save paths must be inside apitorrent's download directory.
 *
 * It reports itself as qBittorrent 4.6.7 with Web API 2.9.3, so clients use the 4.x names
 * (`pause`/`resume`, `pausedUP`); the 5.x names (`stop`/`start`) work too.
 */

import {
  definePlugin,
  isApitorrentError,
  isNotCapable,
  type Api,
  type PluginCaller,
  type TorrentDetail,
  type TorrentSummary,
} from "@apitorrent/plugin-sdk";

import { bool, cookie, num, readParams, type Params } from "./form";
import {
  hashMatches,
  preferences,
  qbHash,
  torrentFiles,
  torrentInfo,
  torrentProperties,
  torrentTrackers,
  type TorrentInfo,
} from "./map";
import { relativeSavePath } from "./paths";

export const QB_VERSION = "v4.6.7";
export const WEBAPI_VERSION = "2.9.3";

export interface Settings {
  downloadHosts: string[];
  categorySavePaths: boolean;
}

/** A category as kept here: its save path relative to the download directory. */
interface Category {
  savePath: string;
}

const CATEGORIES = "categories";
const TAGS = "tags";
const tagKey = (id: string) => `torrent-tags:${id}`;

type Api_ = Api<Settings>;

/** What a handler gets. */
interface Call {
  api: Api_;
  request: Request;
  params: Params;
  caller: PluginCaller;
}

type Handler = (c: Call) => Promise<Response>;

const text = (body: string, status = 200) =>
  new Response(body, { status, headers: { "content-type": "text/plain; charset=UTF-8" } });
const json = (value: unknown) => Response.json(value);
const ok = () => text("Ok.");
const forbidden = () => text("Forbidden", 403);
const notFound = () => text("Not Found", 404);

// --- Stored categories and tags -------------------------------------------------------

async function categories(api: Api_): Promise<Record<string, Category>> {
  return (await api.kv.get<Record<string, Category>>(CATEGORIES)) ?? {};
}

/** Stored categories plus labels torrents carry: a category is a label. */
async function allCategories(api: Api_, torrents: TorrentSummary[]) {
  const stored = await categories(api);
  const app = await api.app.info();
  const out: Record<string, { name: string; savePath: string }> = {};
  for (const t of torrents) {
    if (t.label) out[t.label] = { name: t.label, savePath: "" };
  }
  for (const [name, c] of Object.entries(stored)) {
    out[name] = { name, savePath: c.savePath === "" ? "" : joinDownload(app.downloadDir, c.savePath) };
  }
  return out;
}

function joinDownload(dir: string, rel: string): string {
  const sep = dir.includes("\\") && !dir.includes("/") ? "\\" : "/";
  return `${dir.replace(/[\\/]+$/, "")}${sep}${rel.split("/").join(sep)}`;
}

/** qBittorrent's rules for category names; `/` makes subcategories. */
function validCategory(name: string): boolean {
  return (
    name.trim() === name &&
    name !== "" &&
    !name.includes("\\") &&
    !name.startsWith("/") &&
    !name.endsWith("/") &&
    !name.includes("//") &&
    name.length <= 100
  );
}

async function knownTags(api: Api_): Promise<string[]> {
  return (await api.kv.get<string[]>(TAGS)) ?? [];
}

async function torrentTags(api: Api_, id: string): Promise<string[]> {
  return (await api.kv.get<string[]>(tagKey(id))) ?? [];
}

function splitTags(v: string | undefined): string[] {
  return [...new Set((v ?? "").split(",").map((t) => t.trim()).filter((t) => t !== ""))];
}

async function addKnownTags(api: Api_, tags: string[]) {
  const known = await knownTags(api);
  const more = tags.filter((t) => !known.includes(t));
  if (more.length) await api.kv.set(TAGS, [...known, ...more].sort());
}

async function setTorrentTags(api: Api_, id: string, tags: string[]) {
  if (tags.length) await api.kv.set(tagKey(id), [...new Set(tags)].sort());
  else await api.kv.delete(tagKey(id));
}

// --- Torrents in qBittorrent's shapes -------------------------------------------------

/** The torrents a `hashes` parameter names: `all`, or hashes separated by `|`. */
function select(torrents: TorrentSummary[], hashes: string | undefined): TorrentSummary[] {
  if (hashes === undefined || hashes === "") return [];
  if (hashes === "all") return torrents;
  const wanted = hashes.split("|").map((h) => h.trim()).filter(Boolean);
  return torrents.filter((t) => wanted.some((h) => hashMatches(t, h)));
}

async function detail(api: Api_, t: TorrentSummary): Promise<TorrentDetail | undefined> {
  try {
    return await api.torrents.get(t.id);
  } catch (e) {
    if (isApitorrentError(e) && e.code === "not_found") return undefined;
    throw e;
  }
}

async function infos(api: Api_, torrents: TorrentSummary[]): Promise<TorrentInfo[]> {
  const now = Date.now();
  return await Promise.all(
    torrents.map(async (t) => torrentInfo(t, await detail(api, t), await torrentTags(api, t.id), now)),
  );
}

/** `torrents/info` filters (`filter=`), as qBittorrent defines them. */
const FILTERS: Record<string, (i: TorrentInfo) => boolean> = {
  all: () => true,
  downloading: (i) => /DL$|^downloading$/.test(i.state) && !i.state.startsWith("paused"),
  seeding: (i) => i.state === "uploading" || i.state === "stalledUP" || i.state === "forcedUP" || i.state === "queuedUP",
  completed: (i) => i.progress >= 1,
  paused: (i) => i.state.startsWith("paused") || i.state.startsWith("stopped"),
  stopped: (i) => i.state.startsWith("paused") || i.state.startsWith("stopped"),
  active: (i) => i.dlspeed > 0 || i.upspeed > 0,
  inactive: (i) => i.dlspeed === 0 && i.upspeed === 0,
  resumed: (i) => !i.state.startsWith("paused"),
  running: (i) => !i.state.startsWith("paused"),
  stalled: (i) => i.state === "stalledUP" || i.state === "stalledDL",
  stalled_uploading: (i) => i.state === "stalledUP",
  stalled_downloading: (i) => i.state === "stalledDL",
  checking: (i) => i.state.startsWith("checking"),
  moving: () => false,
  errored: (i) => i.state === "error" || i.state === "missingFiles",
};

async function info({ api, params }: Call): Promise<Response> {
  let torrents = await api.torrents.list();
  const hashes = params.get("hashes");
  if (hashes !== undefined && hashes !== "") torrents = select(torrents, hashes);
  const category = params.get("category");
  if (category !== undefined) torrents = torrents.filter((t) => (t.label ?? "") === category);
  let list = await infos(api, torrents);
  const tag = params.get("tag");
  if (tag !== undefined) {
    list = list.filter((i) => (tag === "" ? i.tags === "" : i.tags.split(", ").includes(tag)));
  }
  const filter = FILTERS[params.get("filter") ?? "all"] ?? FILTERS.all!;
  list = list.filter(filter);
  const sort = params.get("sort") as keyof TorrentInfo | undefined;
  if (sort && list.length && sort in list[0]!) {
    list.sort((a, b) => {
      const x = a[sort];
      const y = b[sort];
      return x < y ? -1 : x > y ? 1 : 0;
    });
    if (bool(params.get("reverse"))) list.reverse();
  }
  const offset = Math.max(0, num(params.get("offset"), 0));
  const start = offset < 0 ? Math.max(0, list.length + offset) : offset;
  const limit = num(params.get("limit"), 0);
  list = list.slice(start, limit > 0 ? start + limit : undefined);
  return json(list);
}

async function one(api: Api_, params: Params): Promise<TorrentSummary | undefined> {
  const hash = params.get("hash");
  if (!hash) return undefined;
  return (await api.torrents.list()).find((t) => hashMatches(t, hash));
}

async function properties({ api, params }: Call): Promise<Response> {
  const t = await one(api, params);
  const d = t && (await detail(api, t));
  if (!t || !d) return notFound();
  return json(torrentProperties(t, d, Date.now()));
}

async function files({ api, params }: Call): Promise<Response> {
  const t = await one(api, params);
  const d = t && (await detail(api, t));
  if (!t || !d) return notFound();
  let list = torrentFiles(d);
  const indexes = params.get("indexes");
  if (indexes) {
    const want = new Set(indexes.split("|").map(Number));
    list = list.filter((f) => want.has(f.index));
  }
  return json(list);
}

async function trackers({ api, params }: Call): Promise<Response> {
  const t = await one(api, params);
  const d = t && (await detail(api, t));
  if (!t || !d) return notFound();
  return json(torrentTrackers(t, d));
}

// --- Adding ---------------------------------------------------------------------------

const MAGNET = /^magnet:\?/i;
const BARE_HASH = /^([0-9a-f]{40}|[0-9a-f]{64})$/i;

async function add({ api, params }: Call): Promise<Response> {
  const settings = api.settings.get();
  const app = await api.app.info();
  const category = (params.get("category") ?? "").trim();
  const known = await categories(api);
  if (category !== "" && !validCategory(category)) return text("Incorrect category name", 400);
  // The folder: the client's, else the category's (qBittorrent's automatic mode).
  let savePath: string | null = null;
  const asked = params.get("savepath") ?? params.get("savePath");
  if (asked !== undefined && asked.trim() !== "") {
    savePath = relativeSavePath(asked, app.downloadDir);
    if (savePath === null) {
      console.warn(`refused an add: save path ${asked} is outside the download directory ${app.downloadDir}`);
      return text("Fails.");
    }
  } else if (settings.categorySavePaths && bool(params.get("autoTMM"), true) && known[category]) {
    savePath = known[category]!.savePath;
  }
  if (category !== "" && !known[category]) {
    // qBittorrent creates a category an add names.
    await api.kv.set(CATEGORIES, { ...known, [category]: { savePath: "" } });
  }
  const tags = splitTags(params.get("tags"));
  const paused = bool(params.get("paused")) || bool(params.get("stopped"));
  const base = {
    label: category || undefined,
    paused,
    savePath: savePath || undefined,
  };

  const sources: ({ magnet: string } | { torrent: Uint8Array } | { url: string })[] = [];
  for (const line of (params.all("urls").join("\n")).split(/\r?\n/)) {
    const u = line.trim();
    if (u === "") continue;
    if (MAGNET.test(u)) sources.push({ magnet: u });
    else if (BARE_HASH.test(u)) {
      sources.push({ magnet: u.length === 40 ? `magnet:?xt=urn:btih:${u}` : `magnet:?xt=urn:btmh:1220${u}` });
    } else sources.push({ url: u });
  }
  for (const f of params.files("torrents")) sources.push({ torrent: f.data });
  if (sources.length === 0) return text("Fails.");

  let added = 0;
  let invalid = 0;
  for (const s of sources) {
    let source: { magnet: string } | { torrent: Uint8Array };
    if ("url" in s) {
      const fetched = await download(s.url);
      if (!fetched) continue;
      source = fetched;
    } else source = s;
    try {
      const r = await api.torrents.add({ ...source, ...base });
      if (r.duplicate || !r.id) continue;
      added++;
      await afterAdd(api, r.id, params, tags);
    } catch (e) {
      if (isApitorrentError(e) && (e.code === "invalid_torrent" || e.code === "invalid_request")) {
        invalid++;
        console.warn(`refused an add: ${e.message}`);
      } else throw e;
    }
  }
  if (added > 0) return ok();
  // qBittorrent answers 415 when every torrent file was unreadable.
  if (invalid > 0 && invalid === sources.length) return text("Torrent file is not valid.", 415);
  return text("Fails.");
}

/** Fetch a `.torrent` URL, or follow it to a magnet link (Prowlarr and Jackett redirect). */
async function download(url: string): Promise<{ torrent: Uint8Array } | { magnet: string } | null> {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    console.warn(`refused an add: ${url} is not a URL or magnet link`);
    return null;
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return null;
  try {
    const r = await fetch(url, { redirect: "manual" });
    const location = r.headers.get("location");
    if (r.status >= 300 && r.status < 400 && location && MAGNET.test(location)) {
      return { magnet: location };
    }
    if (r.status >= 300 && r.status < 400 && location) {
      return await download(new URL(location, url).href);
    }
    if (!r.ok) {
      console.warn(`could not fetch ${parsed.origin}${parsed.pathname}: HTTP ${r.status}`);
      return null;
    }
    return { torrent: await r.bytes() };
  } catch (e) {
    if (isNotCapable(e)) {
      console.warn(
        `could not fetch a .torrent from ${parsed.host}: add it to the plugin's "download hosts" setting`,
      );
    } else console.warn(`could not fetch ${parsed.origin}${parsed.pathname}: ${e}`);
    return null;
  }
}

/** Options of an add that apply once the torrent exists. */
async function afterAdd(api: Api_, id: string, params: Params, tags: string[]) {
  if (tags.length) {
    await addKnownTags(api, tags);
    await setTorrentTags(api, id, tags);
  }
  const ratioLimit = num(params.get("ratioLimit"), -2);
  const seedingTimeLimit = num(params.get("seedingTimeLimit"), -2);
  if (ratioLimit !== -2 || seedingTimeLimit !== -2) {
    await setShareLimitsFor(api, id, ratioLimit, seedingTimeLimit);
  }
  const up = num(params.get("upLimit"), 0);
  const dl = num(params.get("dlLimit"), 0);
  if (up > 0 || dl > 0) {
    await api.torrents.setRateLimits(id, { download: Math.max(0, dl), upload: Math.max(0, up) });
  }
}

/**
 * qBittorrent's share limits: `-2` follows the global limits, `-1` means none, otherwise
 * a ratio, or minutes of seeding.
 */
async function setShareLimitsFor(api: Api_, id: string, ratioLimit: number, seedingTimeLimit: number) {
  if (ratioLimit === -2 && seedingTimeLimit === -2) {
    await api.torrents.setSeedLimits(id, null);
    return;
  }
  // A half that follows the global limits takes the instance's value.
  const global = (await api.app.info()).seedLimits;
  const ratio = ratioLimit === -2 ? global.ratio : Math.max(0, ratioLimit);
  const seedMinutes = seedingTimeLimit === -2 ? global.seedMinutes : Math.max(0, Math.round(seedingTimeLimit));
  await api.torrents.setSeedLimits(id, { ratio, seedMinutes, action: global.action });
}

// --- Changing torrents ----------------------------------------------------------------

async function each(api: Api_, params: Params, f: (t: TorrentSummary) => Promise<unknown>) {
  for (const t of select(await api.torrents.list(), params.get("hashes"))) await f(t);
  return ok();
}

const pause: Handler = ({ api, params }) => each(api, params, (t) => api.torrents.pause(t.id));
const resume: Handler = ({ api, params }) => each(api, params, (t) => api.torrents.resume(t.id));

async function remove({ api, params }: Call): Promise<Response> {
  const deleteFiles = bool(params.get("deleteFiles"));
  return await each(api, params, async (t) => {
    await api.torrents.remove(t.id, { deleteFiles });
    await api.kv.delete(tagKey(t.id));
  });
}

async function setCategory({ api, params }: Call): Promise<Response> {
  const category = (params.get("category") ?? "").trim();
  if (category !== "") {
    const torrents = await api.torrents.list();
    if (!(category in (await allCategories(api, torrents)))) return text("Incorrect category name", 409);
  }
  return await each(api, params, (t) => api.torrents.setLabel(t.id, category || null));
}

async function categoryList({ api }: Call): Promise<Response> {
  return json(await allCategories(api, await api.torrents.list()));
}

async function saveCategory(c: Call, creating: boolean): Promise<Response> {
  const { api, params } = c;
  const name = (params.get("category") ?? "").trim();
  if (!validCategory(name)) return text("Invalid category name", 400);
  const asked = params.get("savePath") ?? params.get("savepath") ?? "";
  const app = await api.app.info();
  const rel = relativeSavePath(asked, app.downloadDir);
  if (rel === null) {
    return text(`Save path must be inside apitorrent's download directory (${app.downloadDir})`, 400);
  }
  const stored = await categories(api);
  const inUse = (await api.torrents.list()).some((t) => t.label === name);
  const exists = name in stored || inUse;
  if (creating && exists) return text("Category already exists", 409);
  if (!creating && !exists) return text("Category does not exist", 409);
  await api.kv.set(CATEGORIES, { ...stored, [name]: { savePath: rel } });
  return ok();
}

async function removeCategories({ api, params }: Call): Promise<Response> {
  const names = (params.get("categories") ?? "").split(/\r?\n/).map((s) => s.trim()).filter(Boolean);
  const stored = await categories(api);
  for (const n of names) delete stored[n];
  await api.kv.set(CATEGORIES, stored);
  for (const t of await api.torrents.list()) {
    if (t.label && names.includes(t.label)) await api.torrents.setLabel(t.id, null);
  }
  return ok();
}

async function tagList({ api }: Call): Promise<Response> {
  return json(await knownTags(api));
}

async function createTags({ api, params }: Call): Promise<Response> {
  await addKnownTags(api, splitTags(params.get("tags")));
  return ok();
}

async function deleteTags({ api, params }: Call): Promise<Response> {
  const gone = splitTags(params.get("tags"));
  await api.kv.set(TAGS, (await knownTags(api)).filter((t) => !gone.includes(t)));
  for (const t of await api.torrents.list()) {
    const have = await torrentTags(api, t.id);
    if (have.some((x) => gone.includes(x))) await setTorrentTags(api, t.id, have.filter((x) => !gone.includes(x)));
  }
  return ok();
}

async function addTags({ api, params }: Call): Promise<Response> {
  const tags = splitTags(params.get("tags"));
  await addKnownTags(api, tags);
  return await each(api, params, async (t) => setTorrentTags(api, t.id, [...(await torrentTags(api, t.id)), ...tags]));
}

async function removeTags({ api, params }: Call): Promise<Response> {
  const tags = splitTags(params.get("tags"));
  return await each(api, params, async (t) => {
    const have = await torrentTags(api, t.id);
    await setTorrentTags(api, t.id, tags.length ? have.filter((x) => !tags.includes(x)) : []);
  });
}

async function setShareLimits({ api, params }: Call): Promise<Response> {
  const ratioLimit = num(params.get("ratioLimit"), -2);
  const seedingTimeLimit = num(params.get("seedingTimeLimit"), -2);
  return await each(api, params, (t) => setShareLimitsFor(api, t.id, ratioLimit, seedingTimeLimit));
}

async function queue(c: Call, to: "top" | "bottom" | "up" | "down"): Promise<Response> {
  const q = (await c.api.app.info()).queue;
  // Like qBittorrent: without queueing there is nothing to order.
  if (!(q.maxActiveDownloads > 0 || q.maxActiveSeeds > 0 || q.maxActive > 0)) {
    return text("Torrent queueing must be enabled", 409);
  }
  return await each(c.api, c.params, (t) => c.api.torrents.queue(t.id, to));
}

async function setLimit({ api, params }: Call, which: "download" | "upload"): Promise<Response> {
  const limit = Math.max(0, num(params.get("limit"), 0));
  return await each(api, params, async (t) => {
    const d = await api.torrents.get(t.id);
    const next = { download: d.limits.downloadRate, upload: d.limits.uploadRate };
    next[which] = limit;
    await api.torrents.setRateLimits(t.id, next);
  });
}

/** Force start: apitorrent has no queue bypass; it resumes the torrent. */
async function setForceStart({ api, params }: Call): Promise<Response> {
  if (!bool(params.get("value"))) return ok();
  return await each(api, params, (t) => api.torrents.resume(t.id));
}

// --- App, transfer, sync --------------------------------------------------------------

async function transferInfo(api: Api_) {
  const torrents = await api.torrents.list();
  return {
    connection_status: "connected",
    dht_nodes: 0,
    dl_info_data: torrents.reduce((n, t) => n + t.downloaded, 0),
    dl_info_speed: torrents.reduce((n, t) => n + t.downloadRate, 0),
    dl_rate_limit: 0,
    up_info_data: torrents.reduce((n, t) => n + t.uploaded, 0),
    up_info_speed: torrents.reduce((n, t) => n + t.uploadRate, 0),
    up_rate_limit: 0,
  };
}

/** `sync/maindata`, always as a full update: correct for every client, if not minimal. */
async function maindata({ api, params }: Call): Promise<Response> {
  const torrents = await api.torrents.list();
  const list = await infos(api, torrents);
  const app = await api.app.info();
  const server = await transferInfo(api);
  return json({
    categories: await allCategories(api, torrents),
    full_update: true,
    rid: Math.max(0, Math.floor(num(params.get("rid"), 0))) + 1,
    server_state: {
      ...server,
      alltime_dl: server.dl_info_data,
      alltime_ul: server.up_info_data,
      queueing: preferences(app).queueing_enabled,
      use_alt_speed_limits: false,
      refresh_interval: 1500,
    },
    tags: await knownTags(api),
    torrents: Object.fromEntries(list.map((i) => [i.hash, i])),
    trackers: {},
  });
}

// --- Routing --------------------------------------------------------------------------

/** Endpoints that only read; everything else changes something and needs `mayWrite`. */
const READS: Record<string, Handler> = {
  "app/version": async () => text(QB_VERSION),
  "app/webapiVersion": async () => text(WEBAPI_VERSION),
  "app/buildInfo": async ({ api }) => {
    const app = await api.app.info();
    return json({
      apitorrent: app.version,
      bitness: 64,
      boost: "1.86.0",
      libtorrent: "2.0.10.0",
      openssl: "3.4.0",
      qt: "6.7.3",
      zlib: "1.3.1",
    });
  },
  "app/preferences": async ({ api }) => json(preferences(await api.app.info())),
  "app/defaultSavePath": async ({ api }) => text((await api.app.info()).downloadDir),
  "transfer/info": async ({ api }) => json(await transferInfo(api)),
  "sync/maindata": maindata,
  "torrents/info": info,
  "torrents/properties": properties,
  "torrents/files": files,
  "torrents/trackers": trackers,
  "torrents/categories": categoryList,
  "torrents/tags": tagList,
};

const WRITES: Record<string, Handler> = {
  "torrents/add": add,
  "torrents/pause": pause,
  "torrents/stop": pause,
  "torrents/resume": resume,
  "torrents/start": resume,
  "torrents/delete": remove,
  "torrents/setCategory": setCategory,
  "torrents/createCategory": (c) => saveCategory(c, true),
  "torrents/editCategory": (c) => saveCategory(c, false),
  "torrents/removeCategories": removeCategories,
  "torrents/createTags": createTags,
  "torrents/deleteTags": deleteTags,
  "torrents/addTags": addTags,
  "torrents/removeTags": removeTags,
  "torrents/setShareLimits": setShareLimits,
  "torrents/topPrio": (c) => queue(c, "top"),
  "torrents/bottomPrio": (c) => queue(c, "bottom"),
  "torrents/increasePrio": (c) => queue(c, "up"),
  "torrents/decreasePrio": (c) => queue(c, "down"),
  "torrents/setDownloadLimit": (c) => setLimit(c, "download"),
  "torrents/setUploadLimit": (c) => setLimit(c, "upload"),
  "torrents/setForceStart": setForceStart,
  // apitorrent checks data when a torrent is added and announces on its own schedule;
  // these succeed without doing anything.
  "torrents/recheck": async () => ok(),
  "torrents/reannounce": async () => ok(),
};

/** Who a request is: apitorrent's own sign-in, or our session cookie. */
async function who(api: Api_, request: Request, caller: PluginCaller | null): Promise<PluginCaller | null> {
  if (caller) return caller;
  const sid = cookie(request, "SID");
  return sid ? await api.auth.session(sid) : null;
}

/** The session cookie, as qBittorrent sets it. */
function sessionCookie(value: string): string {
  return `SID=${value}; HttpOnly; SameSite=Strict; Path=/`;
}

/** A marker cookie for clients that signed in to apitorrent itself and insist on one. */
const SIGNED_IN = "apitorrent";

async function login(api: Api_, params: Params, caller: PluginCaller | null): Promise<Response> {
  const password = params.get("password") ?? "";
  const session = password ? await api.auth.login(password).catch(() => null) : null;
  if (session) {
    return new Response("Ok.", {
      headers: { "content-type": "text/plain; charset=UTF-8", "set-cookie": sessionCookie(session.session) },
    });
  }
  if (caller) {
    return new Response("Ok.", {
      headers: { "content-type": "text/plain; charset=UTF-8", "set-cookie": sessionCookie(SIGNED_IN) },
    });
  }
  console.info("a client failed to sign in: the password is not a live apitorrent API token");
  return text("Fails.");
}

export async function handle(api: Api_, request: Request, caller: PluginCaller | null, path: string) {
  const endpoint = path.replace(/^\/api\/v2\//, "");
  if (endpoint === "auth/login") {
    if (request.method !== "POST") return text("Method Not Allowed", 405);
    return await login(api, await readParams(request), caller);
  }
  const user = await who(api, request, caller);
  if (endpoint === "auth/logout") {
    const sid = cookie(request, "SID");
    if (sid && sid !== SIGNED_IN) await api.auth.logout(sid);
    return new Response(null, {
      status: 200,
      headers: { "set-cookie": "SID=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0" },
    });
  }
  const read = READS[endpoint];
  const write = WRITES[endpoint];
  if (!read && !write) return notFound();
  // Unauthenticated requests get 403, which tells qBittorrent clients to sign in again.
  if (!user) return forbidden();
  if (write) {
    if (request.method !== "POST") return text("Method Not Allowed", 405);
    if (!user.mayWrite) {
      console.warn(`refused ${endpoint} for ${user.userName}: the token may only read`);
      return forbidden();
    }
  }
  const params = await readParams(request);
  return await (read ?? write)!({ api, request, params, caller: user });
}

export default definePlugin<Settings>({
  start(api) {
    const serve = (request: Request, ctx: { caller: PluginCaller | null; path: string }) =>
      handle(api, request, ctx.caller, ctx.path);
    api.http.route("GET", "/api/v2/*", serve);
    api.http.route("POST", "/api/v2/*", serve);
    api.roles.provide("api-compat", {});
  },
});

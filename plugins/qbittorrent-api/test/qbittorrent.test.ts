import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

import type { PluginCaller, PluginManifest, TorrentDetail, TorrentSummary } from "@apitorrent/plugin-sdk";
import { createTestHost } from "@apitorrent/plugin-sdk/testing";

import plugin, { QB_VERSION, WEBAPI_VERSION } from "../src/index";
import { readParams } from "../src/form";
import { qbHash, qbState } from "../src/map";
import { joinPath, relativeSavePath } from "../src/paths";
import manifestJson from "../manifest.json";

const manifest = manifestJson as unknown as PluginManifest;
const DOWNLOADS = "/data/torrents";
const TOKEN = "apt_full_token";
const READ_TOKEN = "apt_read_token";
const MAGNET_HASH = "c9e15763f722f23e98a29decdfae341b98d53056";
const MAGNET = `magnet:?xt=urn:btih:${MAGNET_HASH}&dn=Show.S01E02.1080p`;
const MAGNET2 = `magnet:?xt=urn:btih:${"d".repeat(40)}&dn=Other`;
/** Not a real torrent: the test host takes any bytes, and names them by their SHA-1. */
const TORRENT = new TextEncoder().encode("d8:announce26:https://tracker.example/ann4:infod4:name12:Some.Release6:lengthi1048576eee");

const admin: PluginCaller = { userId: "u1", userName: "alice", role: "admin", mayWrite: true };
const reader: PluginCaller = { userId: "u2", userName: "bob", role: "viewer", mayWrite: false };

async function sha1(bytes: Uint8Array): Promise<string> {
  const d = await crypto.subtle.digest("SHA-1", bytes as Uint8Array<ArrayBuffer>);
  return Array.from(new Uint8Array(d), (b) => b.toString(16).padStart(2, "0")).join("");
}

/**
 * A test host that behaves like apitorrent for this plugin: added torrents show up in the
 * list (with details), and controls change them.
 */
function app() {
  const details: Record<string, TorrentDetail> = {};
  const host = createTestHost<{ downloadHosts: string[]; categorySavePaths: boolean }>({
    manifest,
    torrents: [],
    details,
    app: { downloadDir: DOWNLOADS },
    tokens: { [TOKEN]: admin, [READ_TOKEN]: reader },
  });
  let seenAdded = 0;
  let seenControls = 0;
  let seenRemoved = 0;
  const sync = () => {
    for (const a of host.added.slice(seenAdded)) {
      const name = a.magnet ? /dn=([^&]+)/.exec(a.magnet)?.[1] ?? a.id : `Release ${a.id.slice(0, 8)}`;
      const summary: TorrentSummary = {
        id: a.id,
        name,
        infoHashV1: a.id,
        infoHashV2: null,
        state: a.paused ? "paused" : "downloading",
        progress: 0.25,
        size: 1_048_576,
        piecesHave: 16,
        piecesTotal: 64,
        downloaded: 262_144,
        uploaded: 0,
        downloadRate: a.paused ? 0 : 65_536,
        uploadRate: 0,
        peers: 4,
        seeds: 3,
        addedAt: 1_762_660_000_000,
        label: a.label ?? null,
        savePath: joinPath(DOWNLOADS, a.savePath ?? ""),
        private: true,
        error: null,
        queuePosition: 0,
        group: null,
        supersededBy: null,
      };
      host.torrents.push(summary);
      details[a.id] = {
        summary,
        files: [{ index: 0, path: `${name}.mkv`, length: 1_048_576, priority: 1, progress: 0.25, placement: "save" }],
        trackers: [["https://tracker.example/announce"]],
        comment: null,
        createdBy: null,
        creationDate: null,
        pieceLength: 16_384,
        magnet: a.magnet ?? `magnet:?xt=urn:btih:${a.id}`,
        unwantedPolicy: "hold",
        limits: { downloadRate: 0, uploadRate: 0, maxConnections: null },
        naming: "portable",
        peers: [],
        trackerStatus: [{ url: "https://tracker.example/announce", tier: 0, peers: 7, seeders: 5, leechers: 2, error: null, warning: null }],
        seedLimits: null,
        effectiveSeedLimits: { ratio: 0, seedMinutes: 0, action: "pause" },
        seedingSecs: 0,
        seedLimitReached: false,
      };
    }
    seenAdded = host.added.length;
    for (const c of host.controls.slice(seenControls)) {
      const t = host.torrents.find((x) => x.id === c.id);
      if (!t) continue;
      if (c.action === "setLabel") t.label = c.label ?? null;
      if (c.action === "pause") t.state = "paused";
      if (c.action === "resume") t.state = "downloading";
      if (c.action === "setSeedLimits") {
        const l = c.limits ? { ratio: c.limits.ratio ?? 0, seedMinutes: c.limits.seedMinutes ?? 0, action: c.limits.action ?? "pause" } : null;
        details[t.id]!.seedLimits = l;
        details[t.id]!.effectiveSeedLimits = l ?? { ratio: 0, seedMinutes: 0, action: "pause" };
      }
      if (c.action === "setRateLimits") {
        details[t.id]!.limits = { ...details[t.id]!.limits, downloadRate: c.download ?? 0, uploadRate: c.upload ?? 0 };
      }
    }
    seenControls = host.controls.length;
    for (const r of host.removed.slice(seenRemoved)) {
      const i = host.torrents.findIndex((t) => t.id === r.id);
      if (i >= 0) host.torrents.splice(i, 1);
    }
    seenRemoved = host.removed.length;
  };
  return { host, details, sync };
}

interface Step {
  name: string;
  method: "GET" | "POST";
  path: string;
  form?: Record<string, string>;
  multipart?: { name: string; value?: string; filename?: string; file?: string }[];
  signedIn?: boolean;
  keepCookie?: boolean;
  expect: {
    status: number;
    body?: string;
    bodyMatches?: string;
    cookie?: string;
    object?: Record<string, string>;
    objectOfObjects?: Record<string, string>;
    arrayOf?: Record<string, string>;
    count?: number;
    json?: unknown;
  };
}

interface Fixture {
  client: string;
  source: string;
  multipart: { boundary: string; file: string; field: string };
  hash: "magnet" | "torrent";
  steps: Step[];
}

function expectFields(value: unknown, fields: Record<string, string>, where: string) {
  expect(value, where).toBeObject();
  for (const [k, type] of Object.entries(fields)) {
    expect(typeof (value as Record<string, unknown>)[k], `${where}: field ${k}`).toBe(type);
  }
}

const fixtures = readdirSync(join(import.meta.dir, "fixtures"))
  .filter((f) => f.endsWith(".json"))
  .map((f) => JSON.parse(readFileSync(join(import.meta.dir, "fixtures", f), "utf8")) as Fixture);

describe("pinned client conversations", () => {
  for (const fx of fixtures) {
    test(fx.client, async () => {
      const { host, sync } = app();
      await host.start(plugin);
      const torrentId = await sha1(TORRENT);
      // `$HASH` is the torrent the conversation is about: the magnet's, or the file's.
      const hash = fx.hash === "torrent" ? torrentId : MAGNET_HASH;
      const subst = (s: string) =>
        s
          .replaceAll("$TOKEN", TOKEN)
          .replaceAll("$MAGNET2", MAGNET2)
          .replaceAll("$MAGNET", MAGNET)
          .replaceAll("$HASH", hash)
          .replaceAll("$DOWNLOADS", DOWNLOADS);
      let sid: string | null = null;
      for (const step of fx.steps) {
        const headers = new Headers();
        let body: BodyInit | undefined;
        if (step.form) {
          headers.set("content-type", "application/x-www-form-urlencoded");
          body = new URLSearchParams(Object.entries(step.form).map(([k, v]) => [k, subst(v)])).toString();
        } else if (step.multipart) {
          const enc = new TextEncoder();
          const chunks: Uint8Array[] = [];
          for (const p of step.multipart) {
            const header = (p.file ? fx.multipart.file : fx.multipart.field)
              .replace("{name}", p.name)
              .replace("{filename}", subst(p.filename ?? ""));
            chunks.push(enc.encode(`--${fx.multipart.boundary}\r\n${header}\r\n\r\n`));
            chunks.push(p.file ? TORRENT : enc.encode(subst(p.value ?? "")));
            chunks.push(enc.encode("\r\n"));
          }
          chunks.push(enc.encode(`--${fx.multipart.boundary}--\r\n`));
          headers.set("content-type", `multipart/form-data; boundary=${fx.multipart.boundary}`);
          body = new Uint8Array(await new Blob(chunks as BlobPart[]).arrayBuffer());
        }
        if (sid && step.signedIn !== false) headers.set("cookie", `SID=${sid}`);
        const r = await host.request(step.method, subst(step.path), { caller: null, headers, body });
        sync();
        const where = `${fx.client}: ${step.name}`;
        expect(r.status, where).toBe(step.expect.status);
        const raw = await r.text();
        if (step.expect.body !== undefined) expect(raw, where).toBe(step.expect.body);
        if (step.expect.bodyMatches) expect(raw, where).toMatch(new RegExp(step.expect.bodyMatches));
        const setCookie = r.headers.get("set-cookie");
        if (step.expect.cookie) {
          const m = new RegExp(`(?:^|,\\s*)${step.expect.cookie}=([^;]+)`).exec(setCookie ?? "");
          expect(m, `${where}: Set-Cookie ${setCookie}`).not.toBeNull();
          sid = m![1]!;
        } else if (setCookie?.startsWith("SID=;") && !step.keepCookie) {
          sid = null;
        }
        const e = step.expect;
        if (e.object || e.objectOfObjects || e.arrayOf || e.count !== undefined || e.json !== undefined) {
          const v = JSON.parse(raw);
          if (e.json !== undefined) expect(v, where).toEqual(e.json);
          if (e.object) expectFields(v, e.object, where);
          if (e.objectOfObjects) for (const [k, o] of Object.entries(v)) expectFields(o, e.objectOfObjects, `${where} [${k}]`);
          if (e.arrayOf) {
            expect(Array.isArray(v), where).toBe(true);
            for (const item of v) expectFields(item, e.arrayOf, where);
          }
          if (e.count !== undefined) expect(v.length, where).toBe(e.count);
        }
      }
      await host.stop();
    });
  }
});

describe("what the conversations leave behind", () => {
  test("Sonarr's adds land in apitorrent with its label, limits and tags", async () => {
    const { host, sync } = app();
    await host.start(plugin);
    const login = await host.request("POST", "/api/v2/auth/login", {
      caller: null,
      headers: { "content-type": "application/x-www-form-urlencoded" },
      body: `username=sonarr&password=${TOKEN}`,
    });
    const sid = /SID=([^;]+)/.exec(login.headers.get("set-cookie") ?? "")![1]!;
    const call = async (path: string, form: Record<string, string>) => {
      const r = await host.request("POST", path, {
        caller: null,
        headers: { cookie: `SID=${sid}`, "content-type": "application/x-www-form-urlencoded" },
        body: new URLSearchParams(form).toString(),
      });
      sync();
      return r;
    };
    expect((await call("/api/v2/torrents/createCategory", { category: "tv", savePath: `${DOWNLOADS}/tv` })).status).toBe(200);
    expect(await (await call("/api/v2/torrents/add", { urls: MAGNET, category: "tv", tags: "a, b" })).text()).toBe("Ok.");
    // The category's folder, relative to the download directory; the label is the category.
    expect(host.added[0]).toMatchObject({ magnet: MAGNET, label: "tv", savePath: "tv", paused: false });
    expect(await host.api.kv.get(`torrent-tags:${MAGNET_HASH}`)).toEqual(["a", "b"]);
    await call("/api/v2/torrents/setShareLimits", { hashes: MAGNET_HASH, ratioLimit: "1.5", seedingTimeLimit: "-1" });
    await call("/api/v2/torrents/setShareLimits", { hashes: "all", ratioLimit: "-2", seedingTimeLimit: "-2" });
    await call("/api/v2/torrents/pause", { hashes: MAGNET_HASH.toUpperCase() });
    expect(host.controls.map((c) => [c.action, c.limits])).toEqual([
      ["setSeedLimits", { ratio: 1.5, seedMinutes: 0, action: "pause" }],
      ["setSeedLimits", null],
      ["pause", undefined],
    ]);
    const info = await (await host.request("GET", "/api/v2/torrents/info", { caller: null, headers: { cookie: `SID=${sid}` } })).json();
    expect(info[0]).toMatchObject({
      hash: MAGNET_HASH,
      category: "tv",
      tags: "a, b",
      state: "pausedDL",
      save_path: `${DOWNLOADS}/tv`,
      content_path: `${DOWNLOADS}/tv/Show.S01E02.1080p.mkv`,
      ratio_limit: -1,
      seeding_time_limit: -1,
    });
    await host.stop();
  });

  test("an http .torrent URL is fetched only from hosts an admin allowed", async () => {
    const details: Record<string, TorrentDetail> = {};
    let fetched = 0;
    const host = createTestHost({
      manifest,
      details,
      app: { downloadDir: DOWNLOADS },
      settings: { downloadHosts: ["http://127.0.0.1:9696"] },
      fetch: () => {
        fetched++;
        return new Response(TORRENT);
      },
    });
    await host.start(plugin);
    const add = (url: string) =>
      host.request("POST", "/api/v2/torrents/add", {
        headers: { "content-type": "application/x-www-form-urlencoded" },
        body: new URLSearchParams({ urls: url }).toString(),
      });
    expect(await (await add("http://127.0.0.1:9696/1/download?apikey=x")).text()).toBe("Ok.");
    expect(await (await add("http://192.168.1.9:9117/dl/2")).text()).toBe("Fails.");
    expect(fetched).toBe(1);
    expect(host.added).toHaveLength(1);
    await host.stop();
  });
});

describe("sign-in", () => {
  test("only live API tokens sign in, and a read-only token can't change anything", async () => {
    const { host } = app();
    await host.start(plugin);
    const login = async (password: string) => {
      const r = await host.request("POST", "/api/v2/auth/login", {
        caller: null,
        headers: { "content-type": "application/x-www-form-urlencoded" },
        body: new URLSearchParams({ username: "whoever", password }).toString(),
      });
      return { text: await r.text(), sid: /SID=([^;]+)/.exec(r.headers.get("set-cookie") ?? "")?.[1] };
    };
    for (const bad of ["", "adminadmin", "apt_unknown"]) {
      expect(await login(bad)).toEqual({ text: "Fails.", sid: undefined });
    }
    const r = await login(READ_TOKEN);
    expect(r.text).toBe("Ok.");
    const as = (path: string, method: "GET" | "POST" = "GET") =>
      host.request(method, path, { caller: null, headers: { cookie: `SID=${r.sid}` } });
    expect((await as("/api/v2/torrents/info")).status).toBe(200);
    expect((await as("/api/v2/torrents/pause", "POST")).status).toBe(403);
    expect((await as("/api/v2/torrents/add", "POST")).status).toBe(403);
    // Writes need POST, as in qBittorrent.
    expect((await as("/api/v2/torrents/delete")).status).toBe(405);
    // Revoking the token ends the session.
    host.revokeToken(READ_TOKEN);
    expect((await as("/api/v2/torrents/info")).status).toBe(403);
    // A forged session id gets nowhere.
    const forged = await host.request("GET", "/api/v2/torrents/info", { caller: null, headers: { cookie: "SID=apitorrent" } });
    expect(forged.status).toBe(403);
    // Unknown endpoints are 404s, signed in or not.
    expect((await host.request("GET", "/api/v2/app/shutdown", { caller: null })).status).toBe(404);
    await host.stop();
  });

  test("a request apitorrent signed in itself needs no login", async () => {
    const { host } = app();
    await host.start(plugin);
    expect(await (await host.request("GET", "/api/v2/app/version", { caller: admin })).text()).toBe(QB_VERSION);
    expect(await (await host.request("GET", "/api/v2/app/webapiVersion", { caller: admin })).text()).toBe(WEBAPI_VERSION);
    // A client that insists on logging in still gets a cookie.
    const r = await host.request("POST", "/api/v2/auth/login", { caller: admin, body: "username=x&password=y" });
    expect(await r.text()).toBe("Ok.");
    expect(r.headers.get("set-cookie")).toStartWith("SID=");
    expect((await host.request("POST", "/api/v2/torrents/pause", { caller: reader })).status).toBe(403);
    await host.stop();
  });
});

describe("pieces", () => {
  test("multipart bodies the way .NET, Go and undici write them", async () => {
    const enc = new TextEncoder();
    const make = (b: string, sep: string) =>
      new Request("http://x/api/v2/torrents/add?q=1", {
        method: "POST",
        headers: { "content-type": `multipart/form-data; boundary="${b}"` },
        body: enc.encode(
          `preamble${sep}--${b}${sep}Content-Disposition: form-data; name="category"${sep}${sep}tv${sep}` +
            `--${b}${sep}content-disposition: form-data; filename="a \\"b\\".torrent"; name=torrents${sep}Content-Type: application/x-bittorrent${sep}${sep}` +
            `d4:infod4:name1:x${sep}--e${sep}--${b}--${sep}`,
        ),
      });
    for (const sep of ["\r\n", "\n"]) {
      const p = await readParams(make("XyZ-123", sep));
      expect(p.get("q")).toBe("1");
      expect(p.get("category")).toBe("tv");
      const [f] = p.files("torrents");
      expect(f?.filename).toBe('a "b".torrent');
      expect(new TextDecoder().decode(f?.data)).toBe(`d4:infod4:name1:x${sep}--e`);
    }
  });

  test("save paths stay inside the download directory", () => {
    expect(relativeSavePath("/data/torrents", "/data/torrents/")).toBe("");
    expect(relativeSavePath("/data/torrents/tv/", "/data/torrents")).toBe("tv");
    expect(relativeSavePath("tv/shows", "/data/torrents")).toBe("tv/shows");
    expect(relativeSavePath("C:\\Downloads\\TV", "c:\\downloads")).toBe("TV");
    expect(relativeSavePath("D:\\Other", "C:\\Downloads")).toBeNull();
    expect(relativeSavePath("/data/torrentsx/a", "/data/torrents")).toBeNull();
    expect(relativeSavePath("/data/torrents/../etc", "/data/torrents")).toBeNull();
    expect(relativeSavePath("../up", "/data/torrents")).toBeNull();
    expect(joinPath("C:\\Downloads", "tv/a.mkv")).toBe("C:\\Downloads\\tv\\a.mkv");
    expect(joinPath("/data/", "tv")).toBe("/data/tv");
  });

  test("states and hashes as qBittorrent reports them", () => {
    const t = (state: TorrentSummary["state"], progress: number, rates = 0) =>
      ({ state, progress, downloadRate: rates, uploadRate: rates, error: null }) as TorrentSummary;
    expect(qbState(t("downloading", 0.5, 1))).toBe("downloading");
    expect(qbState(t("downloading", 0.5))).toBe("stalledDL");
    expect(qbState(t("seeding", 1, 1))).toBe("uploading");
    expect(qbState(t("seeding", 1))).toBe("stalledUP");
    expect(qbState(t("paused", 1))).toBe("pausedUP");
    expect(qbState(t("paused", 0.2))).toBe("pausedDL");
    expect(qbState(t("metadata", 0))).toBe("metaDL");
    expect(qbState(t("queued", 0))).toBe("queuedDL");
    expect(qbState(t("checking", 1))).toBe("checkingUP");
    const v2 = "ab".repeat(32);
    expect(qbHash({ id: v2, infoHashV1: null, infoHashV2: v2 })).toBe(v2.slice(0, 40));
    expect(qbHash({ id: v2, infoHashV1: "CD".repeat(20), infoHashV2: v2 })).toBe("cd".repeat(20));
  });
});

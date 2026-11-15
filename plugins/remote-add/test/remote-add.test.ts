import { expect, test } from "bun:test";

import type { PluginManifest } from "@apitorrent/plugin-sdk";
import { createTestHost } from "@apitorrent/plugin-sdk/testing";

import plugin, { MAX_TORRENT_BYTES, sameKey, type Settings } from "../src/index";
import manifestJson from "../manifest.json";

const manifest = manifestJson as unknown as PluginManifest;
const KEY = "k".repeat(16) + "0123456789abcdef";
const TORRENT = new TextEncoder().encode("d8:announce22:https://t.example/ann4:infod4:name4:Somee e");

function tracker(fetched: string[]) {
  return (req: Request) => {
    const url = new URL(req.url);
    fetched.push(url.href);
    switch (url.pathname) {
      case "/download.php":
        return new Response(TORRENT, { headers: { "content-type": "application/x-bittorrent" } });
      case "/redirect":
        return new Response(null, { status: 302, headers: { location: "/download.php?id=1" } });
      case "/elsewhere":
        return new Response(null, { status: 302, headers: { location: "https://evil.example/x.torrent" } });
      case "/login":
        return new Response("<html>sign in</html>", { headers: { "content-type": "text/html" } });
      case "/huge":
        return new Response(new Uint8Array(MAX_TORRENT_BYTES + 1).fill(0x64));
      default:
        return new Response("missing", { status: 404 });
    }
  };
}

async function setup(settings: Partial<Settings> = { key: KEY, hosts: ["tracker.example"] }) {
  const fetched: string[] = [];
  const host = createTestHost<Settings>({ manifest, settings, fetch: tracker(fetched) });
  await host.start(plugin);
  const add = (params: Record<string, string>) =>
    host.request("GET", `/add?${new URLSearchParams(params)}`, { caller: null });
  return { host, fetched, add };
}

const link = (path = "/download.php?id=1&passkey=secret") => `https://tracker.example${path}`;

test("adds a torrent from an allowed host, with the settings' label and folder", async () => {
  const { host, add } = await setup({ key: KEY, hosts: ["tracker.example"], label: "tv", savePath: "tv", paused: true });
  const r = await add({ key: KEY, url: link() });
  expect(r.status).toBe(200);
  expect(r.headers.get("content-type")).toContain("text/html");
  expect(await r.text()).toContain("Added");
  expect(host.added).toHaveLength(1);
  expect(host.added[0]).toMatchObject({ label: "tv", savePath: "tv", paused: true });
  expect(new Uint8Array(host.added[0]!.torrent as Uint8Array)).toEqual(TORRENT);
});

test("a second add of the same torrent is fine", async () => {
  const { host, add } = await setup();
  await add({ key: KEY, url: link() });
  const r = await add({ key: KEY, url: link() });
  expect(r.status).toBe(200);
  expect(await r.text()).toContain("Already added");
  expect(host.added).toHaveLength(1);
});

test("the image reply: a GIF when added, an error status otherwise", async () => {
  const { add } = await setup();
  const ok = await add({ key: KEY, url: link(), reply: "image" });
  expect(ok.status).toBe(200);
  expect(ok.headers.get("content-type")).toBe("image/gif");
  expect(ok.headers.get("cache-control")).toBe("no-store");
  const bad = await add({ key: "wrong", url: link(), reply: "image" });
  expect(bad.status).toBe(403);
});

test("refuses a missing or wrong key before fetching anything", async () => {
  const { host, fetched, add } = await setup();
  expect((await add({ url: link() })).status).toBe(403);
  expect((await add({ key: KEY.slice(0, -1) + "x", url: link() })).status).toBe(403);
  expect(fetched).toHaveLength(0);
  expect(host.added).toHaveLength(0);
});

test("does nothing until a key is set", async () => {
  const { add } = await setup({ hosts: ["tracker.example"] });
  expect((await add({ key: "", url: link() })).status).toBe(503);
  expect((await add({ key: KEY, url: link() })).status).toBe(503);
});

test("refuses hosts that aren't listed, magnets, and things that aren't links", async () => {
  const { host, add } = await setup();
  const other = await add({ key: KEY, url: "https://other.example/a.torrent" });
  expect(other.status).toBe(403);
  expect(await other.text()).toContain("other.example");
  const magnet = await add({ key: KEY, url: `magnet:?xt=urn:btih:${"a".repeat(40)}` });
  expect(magnet.status).toBe(400);
  expect((await add({ key: KEY, url: "not a link" })).status).toBe(400);
  expect((await add({ key: KEY, url: "ftp://tracker.example/a.torrent" })).status).toBe(400);
  expect((await add({ key: KEY })).status).toBe(400);
  expect(host.added).toHaveLength(0);
});

test("follows redirects only within the allowed hosts", async () => {
  const { host, add } = await setup();
  expect((await add({ key: KEY, url: link("/redirect") })).status).toBe(200);
  expect(host.added).toHaveLength(1);
  expect((await add({ key: KEY, url: link("/elsewhere") })).status).toBe(403);
});

test("says so when the tracker sends a web page or an error", async () => {
  const { host, add } = await setup();
  const page = await add({ key: KEY, url: link("/login") });
  expect(page.status).toBe(502);
  expect(await page.text()).toContain("text/html");
  expect((await add({ key: KEY, url: link("/gone") })).status).toBe(502);
  expect((await add({ key: KEY, url: link("/huge") })).status).toBe(502);
  expect(host.added).toHaveLength(0);
});

test("sameKey", async () => {
  expect(await sameKey(KEY, KEY)).toBe(true);
  expect(await sameKey(KEY, KEY + "x")).toBe(false);
  expect(await sameKey("", KEY)).toBe(false);
});

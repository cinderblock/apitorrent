import { describe, expect, test } from "bun:test";

import { definePlugin, isNotCapable, type PluginManifest } from "../src/index";
import { allows, grantFromSetting, parseGrant, stringsAt } from "../src/grants";
import { withDefaults } from "../src/schema";
import { createTestHost } from "../src/testing";

describe("grants match what the host allows", () => {
  // The same cases as apitorrent_plugin_api::network's tests.
  const g = (s: string) => {
    const r = parseGrant(s);
    if (!r) throw new Error(`bad grant ${s}`);
    return r;
  };
  test("plain names: HTTPS on 443 only", () => {
    const grant = g("example.com");
    expect(allows(grant, "https://example.com/feed.xml")).toBe(true);
    expect(allows(grant, "https://EXAMPLE.com:443/")).toBe(true);
    expect(allows(grant, "http://example.com/")).toBe(false);
    expect(allows(grant, "https://example.com:8443/")).toBe(false);
    expect(allows(grant, "https://www.example.com/")).toBe(false);
    expect(allows(grant, "https://example.com.evil.net/")).toBe(false);
    expect(allows(grant, "file:///etc/passwd")).toBe(false);
  });
  test("wildcards are subdomains only", () => {
    const grant = g("*.ubuntu.com");
    expect(allows(grant, "https://releases.ubuntu.com/")).toBe(true);
    expect(allows(grant, "https://a.b.ubuntu.com/")).toBe(true);
    expect(allows(grant, "https://ubuntu.com/")).toBe(false);
    expect(allows(grant, "https://notubuntu.com/")).toBe(false);
    expect(parseGrant("*.com")).toBeNull();
    expect(parseGrant("a.*.com")).toBeNull();
  });
  test("ports, schemes and addresses", () => {
    const grant = g("http://127.0.0.1:8080");
    expect(allows(grant, "http://127.0.0.1:8080/x")).toBe(true);
    expect(allows(grant, "http://127.0.0.1:8081/x")).toBe(false);
    expect(allows(grant, "http://localhost:8080/x")).toBe(false);
    expect(allows(g("example.com:*"), "https://example.com:9/")).toBe(true);
    for (const bad of ["", "example.com/path", "user@example.com", "ftp://x.com", "x.com:0"]) {
      expect(parseGrant(bad)).toBeNull();
    }
  });
  test("settings values", () => {
    const grant = grantFromSetting("http://127.0.0.1:8080/feed.xml")!;
    expect(allows(grant, "http://127.0.0.1:8080/other")).toBe(true);
    expect(allows(grant, "http://127.0.0.1:9/")).toBe(false);
    expect(allows(grantFromSetting("https://feeds.example.com/rss")!, "http://feeds.example.com/"))
      .toBe(false);
    expect(stringsAt({ feeds: [{ url: "a" }, { url: "" }, {}] }, "feeds[].url")).toEqual(["a"]);
  });
});

test("defaults fill in like the host's", () => {
  const schema = {
    type: "object",
    properties: {
      interval: { type: "integer", default: 15 },
      feeds: {
        type: "array",
        default: [],
        items: { type: "object", properties: { paused: { type: "boolean", default: false } } },
      },
      advanced: { type: "object", properties: { retries: { type: "integer", default: 3 } } },
    },
  };
  expect(withDefaults(schema, { feeds: [{ url: "x" }] })).toEqual({
    interval: 15,
    feeds: [{ url: "x", paused: false }],
    advanced: { retries: 3 },
  });
});

const manifest: PluginManifest = {
  id: "demo",
  name: "Demo",
  version: "1.0.0",
  description: "",
  author: null,
  homepage: null,
  apiVersion: 1,
  capabilities: {
    network: ["feeds.example.com"],
    networkFromSettings: [],
    api: ["torrents:add"],
    routes: [{ path: "/status", methods: ["GET"] }],
    kv: true,
    schedule: true,
    roles: [],
  },
  settings: null,
};

test("the harness enforces the manifest like the host", async () => {
  const seen: string[] = [];
  const plugin = definePlugin({
    async start(api) {
      api.schedule.every("poll", { minutes: 15 }, async () => {
        const r = await fetch("https://feeds.example.com/feed");
        seen.push(await r.text());
        await api.kv.set("last", seen.length);
        await api.torrents.add({ magnet: `magnet:?xt=urn:btih:${"a".repeat(40)}` });
        await api.torrents.add({ magnet: `magnet:?xt=urn:btih:${"a".repeat(40)}` });
      });
      api.http.route("GET", "/status", () => ({ seen }));
    },
  });
  const host = createTestHost({
    manifest,
    fetch: () => new Response("hello"),
  });
  await host.start(plugin);
  await host.runJob("poll");
  expect(seen).toEqual(["hello"]);
  expect(host.kv.get("last")).toBe(1);
  expect(host.added).toHaveLength(1);
  expect(await (await host.request("GET", "/status")).json()).toEqual({ seen: ["hello"] });
  expect((await host.request("POST", "/status")).status).toBe(404);

  // Undeclared: a host, an API scope, a route.
  const denied: unknown[] = [];
  for (const attempt of [
    () => fetch("https://elsewhere.example.com/"),
    () => host.api.torrents.list(),
    async () => host.api.http.route("GET", "/other", () => null),
    async () => host.api.events.on("torrent.added", () => {}),
  ]) {
    await attempt().then(() => denied.push(null), (e) => denied.push(e));
  }
  expect(denied.every(isNotCapable)).toBe(true);
  await host.stop();
  // The real fetch is back.
  expect(globalThis.fetch).not.toBe(undefined);
});

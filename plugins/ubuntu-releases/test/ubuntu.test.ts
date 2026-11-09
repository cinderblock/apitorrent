import { expect, test } from "bun:test";

import type { PluginManifest } from "@apitorrent/plugin-sdk";
import { createTestHost } from "@apitorrent/plugin-sdk/testing";

import plugin, { parseMetaRelease, torrentsIn, type Settings } from "../src/index";
import manifestJson from "../manifest.json";

const manifest = manifestJson as unknown as PluginManifest;

const META = `Dist: jammy
Name: Jammy Jellyfish
Version: 22.04.5 LTS
Supported: 1

Dist: oracular
Name: Oracular Oriole
Version: 24.10
Supported: 0

Dist: noble
Name: Noble Numbat
Version: 24.04.3 LTS
Date: Thu, 25 Apr 2024 15:00:00 UTC
Supported: 1

Dist: plucky
Name: Plucky Puffin
Version: 25.04
Supported: 1
`;

const listing = (version: string) => `<html><body>
<a href="ubuntu-${version}-desktop-amd64.iso">iso</a>
<a href="ubuntu-${version}-desktop-amd64.iso.torrent">torrent</a>
<a href="ubuntu-${version}-live-server-amd64.iso.torrent">server</a>
<a href="SHA256SUMS">sums</a>
</body></html>`;

test("meta-release and directory listings", () => {
  const releases = parseMetaRelease(META);
  expect(releases.map((r) => [r.dist, r.version, r.lts, r.supported])).toEqual([
    ["jammy", "22.04.5", true, true],
    ["oracular", "24.10", false, false],
    ["noble", "24.04.3", true, true],
    ["plucky", "25.04", false, true],
  ]);
  expect(torrentsIn(listing("24.04.3"), ["desktop"], "amd64")).toEqual([
    "ubuntu-24.04.3-desktop-amd64.iso.torrent",
  ]);
  expect(torrentsIn(listing("24.04.3"), ["desktop", "live-server"], "arm64")).toEqual([]);
});

function fakeUbuntu(fetched: string[]) {
  return (req: Request) => {
    const url = new URL(req.url);
    fetched.push(url.href);
    if (url.href === "https://changelogs.ubuntu.com/meta-release") return new Response(META);
    const dir = /^\/(\w+)\/$/.exec(url.pathname)?.[1];
    if (url.host === "releases.ubuntu.com" && dir) {
      return new Response(listing(dir === "noble" ? "24.04.3" : dir === "jammy" ? "22.04.5" : "25.04"));
    }
    if (url.pathname.endsWith(".torrent")) {
      return new Response(new TextEncoder().encode(`d4:name${url.pathname.length}:${url.pathname}e`));
    }
    return new Response("missing", { status: 404 });
  };
}

test("offers each supported LTS release to the rules, without adding", async () => {
  const fetched: string[] = [];
  const host = createTestHost<Settings>({ manifest, fetch: fakeUbuntu(fetched) });
  await host.start(plugin);
  await host.runJob("check");
  expect(host.added).toHaveLength(0);
  expect([...host.candidates.keys()].sort()).toEqual([
    "ubuntu-22.04.5-desktop-amd64.iso.torrent",
    "ubuntu-24.04.3-desktop-amd64.iso.torrent",
  ]);
  const c = host.candidates.get("ubuntu-24.04.3-desktop-amd64.iso.torrent")!;
  expect(c.version).toBe("24.04.3");
  expect(c.published).toBe("2024-04-25T15:00:00.000Z");
  expect(c.links.torrent).toBe("https://releases.ubuntu.com/noble/ubuntu-24.04.3-desktop-amd64.iso.torrent");
  expect(c.metadata).toEqual({ dist: "noble", codename: "Noble Numbat", lts: true, flavor: "desktop", arch: "amd64" });
  // Only listings and meta-release are fetched: the core fetches a .torrent when a rule
  // takes it.
  expect(fetched.some((f) => f.endsWith(".torrent"))).toBe(false);
  expect(fetched.some((f) => f.includes("plucky"))).toBe(false);
  const status = await (await host.request("GET", "/status")).json();
  expect(status).toMatchObject({ lastError: null, offered: 2, added: 0 });
  await host.stop();
});

test("adds each torrent once when asked to add directly", async () => {
  const fetched: string[] = [];
  const host = createTestHost<Settings>({
    manifest,
    settings: { addDirectly: true },
    fetch: fakeUbuntu(fetched),
  });
  await host.start(plugin);
  await host.runJob("check");
  expect(host.added).toHaveLength(2);
  expect(host.added.every((a) => a.label === "ubuntu")).toBe(true);
  await host.runJob("check");
  expect(host.added).toHaveLength(2);
  const status = await (await host.request("GET", "/status")).json();
  expect(status.lastError).toBeNull();
  expect(status.releases).toContain("ubuntu-24.04.3-desktop-amd64.iso.torrent");

  // All supported releases, server images too.
  host.setSettings({ addDirectly: true, channel: "all", flavors: ["desktop", "live-server"] });
  await host.runJob("check");
  expect(host.added).toHaveLength(6);
  expect(host.candidates.size).toBe(6);
  await host.stop();
});

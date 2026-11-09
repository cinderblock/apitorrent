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

test("adds each supported LTS release's torrent once", async () => {
  const fetched: string[] = [];
  const host = createTestHost<Settings>({
    manifest,
    fetch: (req) => {
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
    },
  });
  await host.start(plugin);
  await host.runJob("check");
  expect(host.added).toHaveLength(2);
  expect(host.added.every((a) => a.label === "ubuntu")).toBe(true);
  // Non-LTS releases are skipped; nothing is downloaded twice.
  expect(fetched.some((f) => f.includes("plucky"))).toBe(false);
  await host.runJob("check");
  expect(host.added).toHaveLength(2);
  const status = await (await host.request("GET", "/status")).json();
  expect(status.lastError).toBeNull();
  expect(status.releases).toContain("ubuntu-24.04.3-desktop-amd64.iso.torrent");

  // All supported releases, server images too.
  host.setSettings({ channel: "all", flavors: ["desktop", "live-server"] });
  await host.runJob("check");
  expect(host.added).toHaveLength(6);
  await host.stop();
});

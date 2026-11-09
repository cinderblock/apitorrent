import { describe, expect, test } from "bun:test";

import type { PluginManifest } from "@apitorrent/plugin-sdk";
import { createTestHost } from "@apitorrent/plugin-sdk/testing";

import plugin, { passes, type Settings } from "../src/index";
import { decodeFeed, parseFeed } from "../src/feed";
import { parseXml } from "../src/xml";
import manifestJson from "../manifest.json";

const manifest = manifestJson as unknown as PluginManifest;
const hash = (c: string) => c.repeat(40);

const rss = (items: string) => `<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:torrent="http://xmlns.ezrss.it/0.1/">
  <channel><title>Shows &amp; more</title>${items}</channel>
</rss>`;

describe("feeds", () => {
  test("RSS 2.0 with enclosures, the torrent namespace and CDATA", () => {
    const feed = parseFeed(rss(`
      <item><title><![CDATA[Show S01E02 <1080p>]]></title><guid>ep2</guid>
        <enclosure url="https://t.example/ep2.torrent" type="application/x-bittorrent" length="1234"/>
        <pubDate>Sat, 07 Nov 2026 10:00:00 GMT</pubDate></item>
      <item><title>Show S01E01</title><link>https://t.example/ep1</link>
        <torrent:magnetURI>magnet:?xt=urn:btih:${hash("a")}</torrent:magnetURI></item>
      <item><title>Hash only</title><torrent:infoHash>${hash("B")}</torrent:infoHash></item>`));
    expect(feed.title).toBe("Shows & more");
    expect(feed.items.map((i) => i.title)).toEqual(["Show S01E02 <1080p>", "Show S01E01", "Hash only"]);
    expect(feed.items[0]).toMatchObject({ guid: "ep2", torrentUrl: "https://t.example/ep2.torrent", size: 1234 });
    expect(feed.items[1]).toMatchObject({ guid: "https://t.example/ep1", magnet: `magnet:?xt=urn:btih:${hash("a")}` });
    expect(feed.items[2]!.magnet).toStartWith(`magnet:?xt=urn:btih:${hash("b")}&dn=Hash%20only`);
  });

  test("Atom and JSON Feed", () => {
    const atom = parseFeed(`<feed xmlns="http://www.w3.org/2005/Atom"><title>A</title>
      <entry><id>urn:1</id><title type="html">One &#x26; two</title>
        <link rel="alternate" href="https://a.example/1"/>
        <link rel="enclosure" type="application/x-bittorrent" href="https://a.example/1.torrent"/></entry>
    </feed>`);
    expect(atom.items[0]).toMatchObject({ guid: "urn:1", title: "One & two", torrentUrl: "https://a.example/1.torrent" });
    const json = parseFeed(JSON.stringify({
      version: "https://jsonfeed.org/version/1.1",
      title: "J",
      items: [{ id: 7, title: "Seven", attachments: [{ url: `magnet:?xt=urn:btih:${hash("c")}` }] }],
    }));
    expect(json.items[0]).toMatchObject({ guid: "7", title: "Seven", magnet: `magnet:?xt=urn:btih:${hash("c")}` });
    expect(() => parseFeed("<html><body>nope</body></html>")).toThrow("not an RSS");
  });

  test("encodings, comments, doctype and sloppy markup", () => {
    const bytes = new Uint8Array([
      ...new TextEncoder().encode('<?xml version="1.0" encoding="windows-1252"?><rss><channel><item><title>'),
      0x93, 0x51, 0x94,
      ...new TextEncoder().encode("</title><guid>q</guid></item></channel></rss>"),
    ]);
    expect(parseFeed(decodeFeed(bytes, "text/xml")).items[0]!.title).toBe("“Q”");
    const root = parseXml(`<!DOCTYPE rss [<!ENTITY x "y">]><!-- c --><rss a='1' b="x>y"><channel><item><title>t</channel></rss>`);
    expect(root?.attrs).toEqual({ a: "1", b: "x>y" });
    expect(root?.children[0]?.name).toBe("channel");
  });

  test("filters", () => {
    expect(passes("Show S01E01 1080p", { include: "1080p", exclude: "" })).toBe(true);
    expect(passes("Show S01E01 720p", { include: "1080p", exclude: "" })).toBe(false);
    expect(passes("Show S01E01 1080p HDCAM", { include: "", exclude: "hdcam|cam" })).toBe(false);
    expect(() => passes("x", { include: "(", exclude: "" })).toThrow("include filter");
  });
});

function feedServer() {
  let items = "";
  let etag = "v1";
  const requests: string[] = [];
  return {
    requests,
    set(xml: string, tag: string) {
      items = xml;
      etag = tag;
    },
    handle(req: Request): Response {
      const url = new URL(req.url);
      requests.push(url.pathname);
      if (url.pathname === "/feed.xml") {
        if (req.headers.get("if-none-match") === etag) return new Response(null, { status: 304 });
        return new Response(rss(items), { headers: { "content-type": "application/rss+xml", etag } });
      }
      if (url.pathname.endsWith(".torrent")) return new Response(new TextEncoder().encode(`d4:path${url.pathname.length}:${url.pathname}e`));
      return new Response("missing", { status: 404 });
    },
  };
}

const item = (n: number, title = `Show S01E0${n}`) =>
  `<item><title>${title}</title><guid>g${n}</guid><link>magnet:?xt=urn:btih:${hash(String(n))}</link></item>`;

test("polls, filters, dedupes and only adds new items", async () => {
  const server = feedServer();
  const settings: Partial<Settings> = {
    feeds: [{
      url: "https://feeds.example.com/feed.xml",
      include: "S01",
      exclude: "",
      enabled: true,
      paused: false,
      label: "tv",
    }],
  };
  const host = createTestHost<Settings>({ manifest, settings, fetch: (r) => server.handle(r) });
  await host.start(plugin);

  // The first poll only marks what's there as seen.
  server.set(item(1) + item(2), "v1");
  await host.runJob("poll");
  expect(host.added).toHaveLength(0);

  // New items are added (oldest first), filtered, once.
  server.set(item(4) + item(3, "Other S02E01") + item(1) + item(2), "v2");
  await host.runJob("poll");
  expect(host.added.map((a) => a.magnet)).toEqual([`magnet:?xt=urn:btih:${hash("4")}`]);
  expect(host.added[0]!.label).toBe("tv");

  // Unchanged (304) and repeated items add nothing.
  await host.runJob("poll");
  server.set(item(4) + item(1), "v3");
  await host.runJob("poll");
  expect(host.added).toHaveLength(1);

  const status = await (await host.request("GET", "/status")).json();
  expect(status.feeds[0]).toMatchObject({ added: 1, lastError: null });
  const candidates = await host.callRole("source", "candidates", 10);
  expect((candidates as { id: string }[]).map((c) => c.id)).toContain("g4");
  await host.stop();
});

test("backfill, .torrent downloads and hosts that aren't granted", async () => {
  const server = feedServer();
  server.set(
    `<item><title>A</title><guid>a</guid><enclosure url="https://feeds.example.com/a.torrent" type="application/x-bittorrent"/></item>
     <item><title>B</title><guid>b</guid><enclosure url="https://cdn.example.net/b.torrent" type="application/x-bittorrent"/></item>`,
    "v1",
  );
  const feeds = [{ url: "https://feeds.example.com/feed.xml", include: "", exclude: "", enabled: true, paused: true }];
  const host = createTestHost<Settings>({
    manifest,
    settings: { feeds, backfill: true },
    fetch: (r) => server.handle(r),
  });
  await host.start(plugin);
  await host.runJob("poll");
  // A's torrent is on the feed's own host; B's isn't granted, so it waits for a retry.
  expect(host.added).toHaveLength(1);
  expect(host.added[0]!.paused).toBe(true);
  expect(host.added[0]!.torrent).toBeInstanceOf(Uint8Array);

  host.setSettings({ feeds, backfill: true, downloadHosts: ["cdn.example.net"] });
  await host.runJob("poll");
  expect(host.added).toHaveLength(2);
  await host.stop();
});

test("a broken feed reports its error and the others carry on", async () => {
  const host = createTestHost<Settings>({
    manifest,
    settings: {
      feeds: [
        { url: "https://down.example.com/rss", include: "", exclude: "", enabled: true, paused: false },
        { url: "https://feeds.example.com/feed.xml", include: "(", exclude: "", enabled: true, paused: false },
      ],
    },
    fetch: (r) => (new URL(r.url).host === "down.example.com"
      ? new Response("no", { status: 503 })
      : new Response(rss(item(1)))),
  });
  await host.start(plugin);
  await host.runJob("poll");
  const status = await (await host.request("POST", "/poll")).json();
  expect(status.feeds[0].lastError).toContain("503");
  expect(status.feeds[1].lastError).toContain("include filter");
  await host.stop();
});

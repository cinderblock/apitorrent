import { describe, expect, test } from "bun:test";

import type { DelugePreviewTorrent } from "../api/generated/DelugePreviewTorrent";
import type { DelugeSettingsView } from "../api/generated/DelugeSettingsView";
import { DEFAULT_OPTIONS, NO_FILTER, carried, describeProxy, hostMatches, importable, labelAfter, matches, renameLabel, rootsFor, settingRows, validSlug, within } from "./delugeImport";

const t = (over: Partial<DelugePreviewTorrent>): DelugePreviewTorrent => ({
  id: "a".repeat(40),
  name: "Some Linux ISO",
  label: "linux",
  trackers: ["tracker.example.org"],
  savePath: "/srv/torrents",
  size: 1,
  progress: 1,
  paused: false,
  leftover: false,
  present: false,
  problem: null,
  private: false,
  limits: { downloadRate: 0, uploadRate: 0, maxConnections: null, uploadSlots: null },
  stopAtRatio: null,
  skippedFiles: 0,
  renamedFiles: 0,
  downloaded: 0,
  uploaded: 0,
  seedingSecs: 0,
  queue: null,
  addedAt: null,
  editedTrackers: false,
  moveCompleted: null,
  sequential: false,
  firstLast: false,
  superSeed: false,
  notCarried: [],
  ...over,
});

describe("Deluge import wizard", () => {
  test("hosts match exactly or as a subdomain", () => {
    expect(hostMatches("tracker.example.org", "example.org")).toBe(true);
    expect(hostMatches("tracker.example.org", "Tracker.Example.org")).toBe(true);
    expect(hostMatches("badexample.org", "example.org")).toBe(false);
  });

  test("every set field must match", () => {
    expect(matches(t({}), NO_FILTER)).toBe(true);
    expect(matches(t({}), { ...NO_FILTER, labels: ["LINUX", "tv"] })).toBe(true);
    expect(matches(t({ label: null }), { ...NO_FILTER, labels: [""] })).toBe(true);
    expect(matches(t({}), { ...NO_FILTER, labels: [""] })).toBe(false);
    expect(matches(t({}), { ...NO_FILTER, tracker: "example.org" })).toBe(true);
    expect(matches(t({}), { ...NO_FILTER, tracker: "other.org" })).toBe(false);
    expect(matches(t({ progress: 0.5 }), { ...NO_FILTER, state: "complete" })).toBe(false);
    expect(matches(t({ progress: null }), { ...NO_FILTER, state: "incomplete" })).toBe(true);
    expect(matches(t({ paused: true }), { ...NO_FILTER, deluge: "running" })).toBe(false);
    expect(matches(t({}), { ...NO_FILTER, name: "iso  linux" })).toBe(true);
    expect(matches(t({}), { ...NO_FILTER, name: "iso bsd" })).toBe(false);
  });

  test("torrents already here or with a problem can't be ticked", () => {
    expect(importable(t({}))).toBe(true);
    expect(importable(t({ present: true }))).toBe(false);
    expect(importable(t({ problem: "unreadable" }))).toBe(false);
  });

  test("a new space's roots are the outermost save paths", () => {
    expect(rootsFor(["/srv/t/tv", "/srv/t", "/srv/t/", "/srv/tv", "D:\\t\\a", "D:\\t"])).toEqual(["/srv/t", "/srv/tv", "D:\\t"]);
  });

  test("a folder is within roots", () => {
    expect(within("/srv/t/x", ["/srv/t/"])).toBe(true);
    expect(within("/srv/tv", ["/srv/t"])).toBe(false);
    expect(within("D:\\t\\a", ["D:\\t"])).toBe(true);
    expect(within("/anywhere", [])).toBe(true);
  });

  test("slugs", () => {
    expect(validSlug("deluge-2")).toBe(true);
    expect(validSlug("-x")).toBe(false);
    expect(validSlug("X")).toBe(false);
  });

  test("what the ticked torrents carry", () => {
    const c = carried(
      [
        t({ label: "tv", private: true, trackers: ["a.org", "b.org"], uploaded: 5, skippedFiles: 2, notCarried: ["kept out of Deluge's queue"], moveCompleted: "/done", sequential: true, superSeed: true }),
        t({ label: null, private: true, trackers: ["a.org"], limits: { downloadRate: 0, uploadRate: 9, maxConnections: null, uploadSlots: null }, paused: true }),
        t({ label: "tv", stopAtRatio: { ratio: 2, remove: false }, editedTrackers: true, notCarried: ["kept out of Deluge's queue"], leftover: true, firstLast: true, limits: { downloadRate: 0, uploadRate: 0, maxConnections: null, uploadSlots: 2 } }),
      ],
      { ratio: 2, remove: false },
    );
    expect(c.labels).toEqual([
      ["", 1],
      ["tv", 2],
    ]);
    expect(c.privateHosts).toEqual([
      ["a.org", 2],
      ["b.org", 1],
    ]);
    expect(c.notCarried).toEqual([["kept out of Deluge's queue", 2]]);
    expect([c.withTotals, c.withSkippedFiles, c.skippedFiles, c.withLimits, c.editedTrackers, c.running]).toEqual([1, 1, 2, 2, 1, 2]);
    // Two have no ratio while the global is 2: their own differs.
    expect(c.ownRatio).toBe(2);
    expect(c.leftovers).toBe(1);
    expect([c.moveCompleted, c.sequential, c.firstLast, c.superSeed]).toEqual([1, 1, 1, 1]);
  });

  test("labels renamed, dropped or left behind", () => {
    let o = renameLabel(DEFAULT_OPTIONS, "tv", "Shows");
    expect(labelAfter(o, "TV")).toBe("Shows");
    o = renameLabel(o, "TV", "");
    expect(o.labelRenames).toEqual([{ from: "TV", to: "" }]);
    expect(labelAfter(o, "tv")).toBeNull();
    o = renameLabel(o, "tv", "tv");
    expect(o.labelRenames).toEqual([]);
    expect(labelAfter({ ...o, labels: false }, "tv")).toBeNull();
    expect(labelAfter(o, "")).toBeNull();
  });

  test("settings rows: slow torrents, encryption, discovery and the proxy only for the default space", () => {
    const s: DelugeSettingsView = {
      downloadRate: null,
      uploadRate: 1024,
      maxConnections: 200,
      maxActiveDownloading: 3,
      maxActiveSeeding: null,
      maxActive: 8,
      dontCountSlow: true,
      stopAtRatio: null,
      proxy: null,
      encryption: "required",
      encryptionNote: "incoming enabled and outgoing forced",
      discovery: { dht: true, lsd: false, upnp: true, natpmp: true },
      changed: { ...DEFAULT_OPTIONS.settings, uploadRate: true, encryption: true },
    };
    expect(settingRows(s, false).map((r) => r.key)).toEqual(["downloadRate", "uploadRate", "maxConnections", "queueSlots", "stopAtRatio"]);
    const rows = settingRows(s, true);
    expect(rows.map((r) => r.key)).toContain("ignoreSlow");
    expect(rows.filter((r) => r.changed).map((r) => r.key)).toEqual(["uploadRate", "encryption"]);
    expect(rows.find((r) => r.key === "encryption")).toMatchObject({ value: "required", note: "incoming enabled and outgoing forced" });
    expect(rows.find((r) => r.key === "proxy")?.value).toBe("none");
    expect(rows.find((r) => r.key === "downloadRate")?.value).toBe("no limit");
    expect(rows.find((r) => r.key === "queueSlots")?.value).toBe("3 downloading, no limit seeding, 8 in all");
  });

  test("Deluge's proxy in words, never with its password", () => {
    const p = {
      type: "socks5" as const,
      host: "proxy.example",
      port: 1080,
      username: "me",
      passwordSet: true,
      proxyHostnames: true,
      proxyPeers: true,
      proxyTrackers: false,
      force: false,
    };
    expect(describeProxy(p)).toBe("SOCKS5 proxy.example:1080 as me (with a password); names and peers through it");
    expect(describeProxy({ ...p, type: "http", username: "", passwordSet: false, proxyHostnames: false, force: true })).toBe(
      "HTTP proxy.example:1080; peers and trackers through it; forced",
    );
    expect(describeProxy(null)).toBe("none");
  });
});

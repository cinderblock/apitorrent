import { describe, expect, test } from "bun:test";

import type { DelugePreviewTorrent } from "../api/generated/DelugePreviewTorrent";
import { NO_FILTER, hostMatches, importable, matches, rootsFor, validSlug, within } from "./delugeImport";

const t = (over: Partial<DelugePreviewTorrent>): DelugePreviewTorrent => ({
  id: "a".repeat(40),
  name: "Some Linux ISO",
  label: "linux",
  trackers: ["tracker.example.org"],
  savePath: "/srv/torrents",
  size: 1,
  progress: 1,
  paused: false,
  present: false,
  problem: null,
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
});

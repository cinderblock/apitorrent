import { describe, expect, test } from "bun:test";

import { badUrls, duplicateUrls, fromDrafts, moveTier, newTier, sameTiers, toDrafts } from "./trackerTiers";

describe("tracker tiers", () => {
  test("drafts round-trip, dropping blank lines and empty tiers", () => {
    const tiers = [["http://a/announce", "udp://b:1"], ["https://c/x"]];
    expect(fromDrafts(toDrafts(tiers))).toEqual(tiers);
    expect(fromDrafts([newTier("  http://a/announce \r\n\n"), newTier("   "), newTier("udp://b:1")])).toEqual([["http://a/announce"], ["udp://b:1"]]);
  });

  test("tiers move up and down, never out of range", () => {
    const d = toDrafts([["a"], ["b"], ["c"]]);
    expect(fromDrafts(moveTier(d, 2, -1))).toEqual([["a"], ["c"], ["b"]]);
    expect(fromDrafts(moveTier(d, 0, 1))).toEqual([["b"], ["a"], ["c"]]);
    expect(moveTier(d, 0, -1)).toBe(d);
    expect(moveTier(d, 2, 1)).toBe(d);
  });

  test("duplicates and unusable URLs are found", () => {
    expect(duplicateUrls([["http://a"], ["http://b", "http://a"], ["http://a"]])).toEqual(["http://a"]);
    expect(badUrls([["http://a/x", "udp://b:80", "ftp://c", "nonsense", "https://d"]])).toEqual(["ftp://c", "nonsense"]);
  });

  test("lists compare by tier and order", () => {
    expect(sameTiers([["a"], ["b"]], [["a"], ["b"]])).toBe(true);
    expect(sameTiers([["a"], ["b"]], [["b"], ["a"]])).toBe(false);
    expect(sameTiers([["a", "b"]], [["a"], ["b"]])).toBe(false);
  });
});

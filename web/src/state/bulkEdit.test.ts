import { describe, expect, test } from "bun:test";

import type { TorrentSettings } from "../api/generated/TorrentSettings";
import { ApiRequestError } from "../api/client";
import { buildEdit, common, describeReport, eachOf, EMPTY_FORM, parseTrackerLines, trackerCounts } from "./bulkEdit";

function settings(id: string, trackers: string[][], label: string | null = null): TorrentSettings {
  return {
    id,
    label,
    limits: { downloadRate: 0, uploadRate: 0, maxConnections: null },
    seedLimits: null,
    trackers,
    unwantedPolicy: "hold",
  };
}

describe("bulk edit", () => {
  test("common values", () => {
    expect(common([])).toEqual({ kind: "none" });
    expect(common(["a", "a"])).toEqual({ kind: "same", value: "a" });
    expect(common(["a", null, "b", "a"])).toEqual({ kind: "mixed", distinct: 3 });
    expect(common([{ r: 1 }, { r: 1 }])).toEqual({ kind: "same", value: { r: 1 } });
  });

  test("tracker counts, most listed first, each torrent counted once", () => {
    const s = [settings("1", [["http://a"], ["http://b", "http://a"]]), settings("2", [["http://b"]]), settings("3", [])];
    expect(trackerCounts(s)).toEqual([
      { url: "http://b", count: 2 },
      { url: "http://a", count: 1 },
    ]);
  });

  test("nothing changed is no edit", () => {
    expect(buildEdit(["1"], EMPTY_FORM)).toBeNull();
    expect(buildEdit(["1"], { ...EMPTY_FORM, add: " \n\n" })).toBeNull();
  });

  test("only changed fields are sent", () => {
    expect(buildEdit(["1", "2"], { ...EMPTY_FORM, up: 512, label: " tv " })).toEqual({
      ids: ["1", "2"],
      label: "tv",
      limits: { uploadRate: 512 * 1024 },
    });
    expect(buildEdit(["1"], { ...EMPTY_FORM, label: "", peers: 0, seed: { kind: "follow" } })).toEqual({
      ids: ["1"],
      label: "",
      limits: { maxConnections: 0 },
      seedLimits: { limits: null },
    });
  });

  test("tracker edits: a removed URL isn't also replaced, blank replacements are dropped", () => {
    const edit = buildEdit(["1"], {
      ...EMPTY_FORM,
      remove: new Set(["http://old"]),
      replace: new Map([
        ["http://old", "http://new"],
        ["http://a", " http://a2 "],
        ["http://b", "  "],
      ]),
      add: "http://c\nhttp://c\n http://d ",
    });
    expect(edit?.trackers).toEqual({
      replace: [{ from: "http://a", to: "http://a2" }],
      remove: ["http://old"],
      add: ["http://c", "http://d"],
    });
  });

  test("tracker lines", () => {
    expect(parseTrackerLines("a\r\n\n b \na")).toEqual(["a", "b"]);
  });

  test("reports", () => {
    expect(describeReport({ updated: ["1"], failed: [] })).toBe("Updated 1 torrent.");
    expect(describeReport({ updated: ["1", "2"], failed: [{ id: "3", code: "x", message: "y" }] })).toBe("Updated 2 of 3 torrents; 1 failed.");
    expect(describeReport({ updated: [], failed: [{ id: "3", code: "x", message: "y" }] })).toBe("None of the 1 torrent changed.");
  });

  test("eachOf runs every id and collects failures", async () => {
    const r = await eachOf(["1", "2", "3"], async (id) => {
      if (id === "2") throw new ApiRequestError(404, "not_found", "no such torrent");
    }, 2);
    expect(r.updated.sort()).toEqual(["1", "3"]);
    expect(r.failed).toEqual([{ id: "2", code: "not_found", message: "no such torrent" }]);
  });
});

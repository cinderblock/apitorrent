import { describe, expect, test } from "bun:test";

import type { TorrentSummary } from "../api/generated/TorrentSummary";
import { applySync, countByFilter, initialSyncState, visibleTorrents } from "./torrents";

function t(id: string, over: Partial<TorrentSummary> = {}): TorrentSummary {
  return {
    id,
    space: 1,
    name: id,
    infoHashV1: null,
    infoHashV2: null,
    state: "downloading",
    progress: 0,
    size: 100,
    piecesHave: 0,
    piecesTotal: 10,
    downloaded: 0,
    uploaded: 0,
    downloadRate: 0,
    uploadRate: 0,
    peers: 0,
    seeds: 0,
    addedAt: 0,
    label: null,
    savePath: "/d",
    private: false,
    error: null,
    queuePosition: 0,
    group: null,
    supersededBy: null,
    ...over,
  };
}

describe("applySync", () => {
  test("snapshot then consecutive patches", () => {
    let r = applySync(initialSyncState, { type: "snapshot", revision: 5, torrents: [t("a")] });
    if (!r.ok) throw new Error(r.reason);
    r = applySync(r.state, { type: "patch", revision: 6, upserted: [t("b")], removed: ["a"] });
    if (!r.ok) throw new Error(r.reason);
    expect([...r.state.torrents.keys()]).toEqual(["b"]);
    expect(r.state.revision).toBe(6);
  });

  test("a revision gap or an early patch asks for a resync", () => {
    const early = applySync(initialSyncState, {
      type: "patch",
      revision: 1,
      upserted: [],
      removed: [],
    });
    expect(early.ok).toBe(false);
    const r = applySync(initialSyncState, { type: "snapshot", revision: 1, torrents: [] });
    if (!r.ok) throw new Error(r.reason);
    const gap = applySync(r.state, { type: "patch", revision: 3, upserted: [], removed: [] });
    expect(gap.ok).toBe(false);
  });
});

describe("visibleTorrents", () => {
  const all = new Map(
    [
      t("x2", { name: "Ubuntu 26.04", size: 5, label: "linux" }),
      t("x10", { name: "ubuntu 24.04", size: 50, state: "seeding" }),
      t("y", { name: "Raspberry Pi OS", size: 20, state: "paused", label: "pi" }),
    ].map((x) => [x.id, x]),
  );

  test("filters, searches and sorts", () => {
    const byName = visibleTorrents(all, {
      filter: "all",
      label: null,
      search: "",
      sort: { key: "name", descending: false },
    });
    expect(byName.map((r) => r.name)).toEqual(["Raspberry Pi OS", "ubuntu 24.04", "Ubuntu 26.04"]);

    const search = visibleTorrents(all, {
      filter: "all",
      label: null,
      search: "UBUNTU",
      sort: { key: "size", descending: true },
    });
    expect(search.map((r) => r.id)).toEqual(["x10", "x2"]);

    const label = visibleTorrents(all, {
      filter: "all",
      label: "pi",
      search: "",
      sort: { key: "name", descending: false },
    });
    expect(label.map((r) => r.id)).toEqual(["y"]);
  });

  test("counts", () => {
    const c = countByFilter(all);
    expect(c.all).toBe(3);
    expect(c.seeding).toBe(1);
    expect(c.paused).toBe(1);
  });
});

test("queued torrents are counted and sort by queue position", () => {
  const map = new Map(
    [t("b", { queuePosition: 1, state: "queued" }), t("a", { queuePosition: 2 }), t("c", { queuePosition: 0 })].map((x) => [x.id, x]),
  );
  expect(countByFilter(map).queued).toBe(1);
  const rows = visibleTorrents(map, { filter: "all", label: null, search: "", sort: { key: "queuePosition", descending: false } });
  expect(rows.map((r) => r.id)).toEqual(["c", "b", "a"]);
});

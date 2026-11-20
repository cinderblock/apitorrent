import { describe, expect, test } from "bun:test";

import { migrateV1, moveColumn, normalize, parsePrefs, placeColumn, toggleColumn, visibleColumns, type ColumnInfo } from "./columns";

const ALL: ColumnInfo[] = [
  { id: "name", defaultVisible: true, required: true },
  { id: "size", defaultVisible: true },
  { id: "label", defaultVisible: false },
  { id: "rate", defaultVisible: true },
];

describe("columns", () => {
  test("defaults", () => {
    expect(visibleColumns(ALL, null)).toEqual(["name", "size", "rate"]);
  });

  test("toggle shows a column in its place, and the name column stays", () => {
    const p = toggleColumn(ALL, null, "label");
    expect(visibleColumns(ALL, p)).toEqual(["name", "size", "label", "rate"]);
    const q = toggleColumn(ALL, p, "size");
    expect(visibleColumns(ALL, q)).toEqual(["name", "label", "rate"]);
    expect(visibleColumns(ALL, toggleColumn(ALL, q, "name"))).toEqual(["name", "label", "rate"]);
  });

  test("move skips hidden columns and stops at the ends", () => {
    // label is hidden, so moving rate left passes it and swaps with size.
    const p = moveColumn(ALL, null, "rate", -1);
    expect(visibleColumns(ALL, p)).toEqual(["name", "rate", "size"]);
    expect(moveColumn(ALL, p, "name", -1)).toEqual(normalize(ALL, p));
    expect(visibleColumns(ALL, moveColumn(ALL, p, "size", 1))).toEqual(["name", "rate", "size"]);
  });

  test("saved prefs survive new and removed columns", () => {
    const saved = { order: ["rate", "gone", "name", "size", "rate"], hidden: ["size", "gone", "name"] };
    // gone is dropped, the duplicate goes, name can't be hidden, and label (new) arrives
    // hidden, after size.
    const p = normalize(ALL, saved);
    expect(p.order).toEqual(["rate", "name", "size", "label"]);
    expect(p.hidden).toEqual(["size", "label"]);
    expect(visibleColumns(ALL, saved)).toEqual(["rate", "name"]);
  });

  test("place drops a column before another, or last", () => {
    expect(visibleColumns(ALL, placeColumn(ALL, null, "rate", "name"))).toEqual(["rate", "name", "size"]);
    expect(visibleColumns(ALL, placeColumn(ALL, null, "name", null))).toEqual(["size", "rate", "name"]);
    // Onto itself, or an unknown column: no change.
    expect(placeColumn(ALL, null, "size", "size")).toEqual(normalize(ALL, null));
    expect(placeColumn(ALL, null, "size", "gone")).toEqual(normalize(ALL, null));
    // Hidden columns stay hidden, wherever they end up.
    const p = placeColumn(ALL, null, "label", "name");
    expect(p.order).toEqual(["label", "name", "size", "rate"]);
    expect(p.hidden).toEqual(["label"]);
  });

  test("v1 prefs hide the queue column once", () => {
    const v1 = parsePrefs(JSON.stringify({ order: ["queuePosition", "name"], hidden: [] }))!;
    expect(migrateV1(v1)).toEqual({ order: ["queuePosition", "name"], hidden: ["queuePosition"] });
    expect(migrateV1({ order: ["name"], hidden: [] })).toEqual({ order: ["name"], hidden: [] });
    expect(parsePrefs("not json")).toBeNull();
    expect(parsePrefs(null)).toBeNull();
  });
});

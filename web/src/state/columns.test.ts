import { describe, expect, test } from "bun:test";

import { moveColumn, normalize, toggleColumn, visibleColumns, type ColumnInfo } from "./columns";

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
});

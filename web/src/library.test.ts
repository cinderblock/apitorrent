import { expect, test } from "bun:test";

import { describeReuse } from "./library";

test("reuse records read as a sentence", () => {
  const r = { sourceId: "a".repeat(40), sourceName: "Show S01", method: "hardLink", bytes: 1024 } as const;
  expect(describeReuse(r)).toBe("Reused from Show S01 (hard link), 1.00 KiB");
  expect(describeReuse(r, 1024)).toBe("Reused from Show S01 (hard link), all of it");
  expect(describeReuse({ ...r, method: "pieceSharing" }, 4096)).toBe("Reused from Show S01 (piece sharing), 1.00 KiB");
  expect(describeReuse({ ...r, sourceName: "" })).toBe(`Reused from ${"a".repeat(12)} (hard link), 1.00 KiB`);
});

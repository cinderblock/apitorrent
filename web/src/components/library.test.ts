import { describe, expect, test } from "bun:test";

import type { DedupeStatus } from "../api/generated/DedupeStatus";
import type { LibraryFolder } from "../api/generated/LibraryFolder";
import { dedupeFinding, dedupeSaved, folderPathProblem, folderProgress } from "./library";

const MiB = 1024 * 1024;

const status = (patch: Partial<DedupeStatus> = {}): DedupeStatus => ({
  mode: "report",
  running: false,
  lastPassAt: 1,
  reclaimableBytes: 0,
  reclaimableFiles: 0,
  groups: 0,
  unmergeableFiles: 0,
  pendingHashBytes: 0,
  reclaimedBytes: 0,
  reclaimedFiles: 0,
  skippedFiles: 0,
  ...patch,
});

const folder = (patch: Partial<LibraryFolder> = {}): LibraryFolder => ({
  id: 1,
  path: "/media",
  addedAt: 0,
  scanning: false,
  lastScanAt: 5,
  files: 4,
  bytes: 400 * MiB,
  hashedFiles: 1,
  hashedBytes: 100 * MiB,
  error: null,
  ...patch,
});

describe("dedupe status", () => {
  test("says what merging would free, or that nothing would", () => {
    expect(dedupeFinding(status({ lastPassAt: null }))).toBe("Not checked yet.");
    expect(dedupeFinding(status())).toBe("No identical copies to merge.");
    expect(dedupeFinding(status({ pendingHashBytes: 2 * MiB }))).toContain("still to read");
    const found = status({ reclaimableBytes: 3 * MiB, reclaimableFiles: 2, groups: 1 });
    expect(dedupeFinding(found)).toContain("2 identical copies in 1 set");
    expect(dedupeFinding(found)).toContain("would free");
    expect(dedupeFinding({ ...found, mode: "on" })).toContain("will free");
  });

  test("totals only once something was merged", () => {
    expect(dedupeSaved(status())).toBeNull();
    expect(dedupeSaved(status({ reclaimedFiles: 1, reclaimedBytes: MiB }))).toContain("linking 1 copy");
  });
});

describe("library folders", () => {
  test("progress reads as a line", () => {
    expect(folderProgress(folder())).toContain("25% fingerprinted");
    expect(folderProgress(folder({ hashedFiles: 4, hashedBytes: 400 * MiB }))).toContain("all fingerprinted");
    expect(folderProgress(folder({ files: 0, bytes: 0, hashedFiles: 0, hashedBytes: 0 }))).toBe("No files.");
    expect(folderProgress(folder({ scanning: true, lastScanAt: null }))).toBe("Looking for files…");
    expect(folderProgress(folder({ lastScanAt: null }))).toBe("Waiting to be walked.");
    expect(folderProgress(folder({ error: "gone" }))).toBe("Can't be read: gone");
  });

  test("new folder paths must be full paths", () => {
    expect(folderPathProblem("  ")).not.toBeNull();
    expect(folderPathProblem("media")).not.toBeNull();
    expect(folderPathProblem("/srv/media")).toBeNull();
    expect(folderPathProblem("D:\\Media")).toBeNull();
    expect(folderPathProblem("\\\\nas\\media")).toBeNull();
  });
});

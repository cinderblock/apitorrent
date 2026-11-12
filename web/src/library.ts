import type { FileReuse } from "./api/generated/FileReuse";
import type { ReuseMethod } from "./api/generated/ReuseMethod";
import { formatBytes } from "./format";

/** How the content library supplied a file, in words. */
export const REUSE_METHOD: Record<ReuseMethod, string> = {
  hardLink: "hard link",
  clone: "clone",
  copy: "copy",
  pieceSharing: "piece sharing",
};

/** "Reused from Show S01 (hard link), 1.20 GiB" for a file's reuse record. */
export function describeReuse(r: FileReuse, length?: number): string {
  const from = r.sourceName || r.sourceId.slice(0, 12);
  const how = REUSE_METHOD[r.method] ?? r.method;
  const amount = length !== undefined && r.bytes >= length ? "all of it" : formatBytes(r.bytes);
  return `Reused from ${from} (${how}), ${amount}`;
}

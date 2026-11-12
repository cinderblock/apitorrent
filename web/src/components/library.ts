// Settings → Library: words for dedupe and library-folder status.

import type { DedupeStatus } from "../api/generated/DedupeStatus";
import type { LibraryFolder } from "../api/generated/LibraryFolder";
import { formatBytes } from "../format";

function plural(n: number, one: string, many: string = `${one}s`): string {
  return `${n} ${n === 1 ? one : many}`;
}

/** What the last dedupe pass found: the space that merging would free, or that there is none. */
export function dedupeFinding(s: DedupeStatus): string {
  if (s.lastPassAt === null && !s.running) return "Not checked yet.";
  if (s.reclaimableBytes === 0) {
    const pending = s.pendingHashBytes > 0 ? ` (${formatBytes(s.pendingHashBytes)} still to read)` : "";
    return `No identical copies to merge${pending}.`;
  }
  const verb = s.mode === "on" ? "will free" : "would free";
  return `${plural(s.reclaimableFiles, "identical copy", "identical copies")} in ${plural(s.groups, "set")}: merging them ${verb} ${formatBytes(s.reclaimableBytes)}.`;
}

/** What dedupe has freed over the instance's life. */
export function dedupeSaved(s: DedupeStatus): string | null {
  if (s.reclaimedFiles === 0) return null;
  return `Freed ${formatBytes(s.reclaimedBytes)} so far by linking ${plural(s.reclaimedFiles, "copy", "copies")}.`;
}

/** A folder's walk and fingerprinting progress, in a line. */
export function folderProgress(f: LibraryFolder): string {
  if (f.error) return `Can't be read: ${f.error}`;
  if (f.scanning) return f.lastScanAt === null ? "Looking for files…" : `Looking for changes… ${plural(f.files, "file")} so far.`;
  if (f.lastScanAt === null) return "Waiting to be walked.";
  const files = `${plural(f.files, "file")}, ${formatBytes(f.bytes)}`;
  if (f.files === 0) return "No files.";
  if (f.hashedFiles >= f.files) return `${files}, all fingerprinted.`;
  const pct = f.bytes > 0 ? Math.floor((f.hashedBytes / f.bytes) * 100) : 0;
  return `${files}; ${pct}% fingerprinted (${plural(f.hashedFiles, "file")}).`;
}

/** A path typed for a new folder: trimmed, and plausibly absolute (Unix, a drive, or UNC). */
export function folderPathProblem(path: string): string | null {
  const p = path.trim();
  if (p === "") return "Enter a folder on the server.";
  if (!/^(\/|[A-Za-z]:[\\/]|\\\\)/.test(p)) return "Use the folder's full path on the server.";
  return null;
}

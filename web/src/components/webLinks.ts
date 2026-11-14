// Web links to a torrent's files: a storage root served by a web server (an nginx `autoindex`
// listing, say) has an `http(s)://` share locator, and a path below the root is appended to it.
// Paths come from the server and are checked like the desktop app's (`mapping.rs`): no empty,
// `.` or `..` parts, and Windows paths compare case-insensitively.

import type { StorageRoot } from "../api/generated/StorageRoot";

/** An absolute path on the server, split into parts. */
export interface ServerPath {
  windows: boolean;
  parts: string[];
}

const BAD_PART = (p: string) => p === "" || p === "." || p === "..";

export function parseServerPath(path: string): ServerPath | null {
  const drive = /^[A-Za-z]:[\\/]/.test(path);
  const unc = path.startsWith("\\\\");
  let windows: boolean;
  let parts: string[];
  if (drive || unc) {
    windows = true;
    parts = path.split(/[\\/]/).filter((p) => p !== "");
  } else if (path.startsWith("/")) {
    windows = false;
    parts = path.split("/").filter((p) => p !== "");
  } else return null;
  if (parts.some(BAD_PART)) return null;
  if (unc && parts.length < 2) return null;
  return { windows, parts };
}

/** The parts of `path` below `prefix`, or `null` when it isn't under (or equal to) it. */
export function below(path: ServerPath, prefix: ServerPath): string[] | null {
  if (path.windows !== prefix.windows || prefix.parts.length > path.parts.length) return null;
  const same = (a: string, b: string) => (path.windows ? a.toLowerCase() === b.toLowerCase() : a === b);
  return prefix.parts.every((p, i) => same(p, path.parts[i]!)) ? path.parts.slice(prefix.parts.length) : null;
}

/** A web locator's base URL (ending in `/`), or `null` for other kinds. */
export function webBase(locator: string): string | null {
  if (!/^https?:\/\//i.test(locator)) return null;
  return locator.endsWith("/") ? locator : `${locator}/`;
}

/**
 * The URL of `relative` (parts joined with `/`, such as a torrent's file path; empty for the
 * folder itself) inside `savePath`, through the first web locator of the storage root holding
 * it. `dir` ends the URL with `/`, for a folder's listing. `null` when no root holding it is
 * served over the web, or a path part is unsafe.
 */
export function webUrl(roots: StorageRoot[], savePath: string, relative = "", dir = false): string | null {
  const save = parseServerPath(savePath);
  if (!save) return null;
  // The innermost root holding the path wins.
  let best: { base: string; rest: string[]; depth: number } | null = null;
  for (const root of roots) {
    const base = root.locators.map(webBase).find((b) => b !== null);
    const rootPath = parseServerPath(root.path);
    if (!base || !rootPath) continue;
    const rest = below(save, rootPath);
    if (rest && (!best || rootPath.parts.length > best.depth)) best = { base, rest, depth: rootPath.parts.length };
  }
  if (!best) return null;
  const extra = relative === "" ? [] : relative.split(save.windows ? /[\\/]/ : "/");
  if (extra.some(BAD_PART)) return null;
  const parts = [...best.rest, ...extra];
  const url = best.base + parts.map(encodeURIComponent).join("/");
  return dir && parts.length > 0 ? `${url}/` : url;
}

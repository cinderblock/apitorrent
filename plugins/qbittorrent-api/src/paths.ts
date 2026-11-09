/**
 * Save paths. qBittorrent clients send absolute paths; apitorrent's plugins may only name
 * folders relative to the download directory. An absolute path inside it becomes the
 * relative path; one outside is refused (the plugin can't put files anywhere else).
 */

/** `C:\x`, `\\server\share`, or `/x`. */
export function isAbsolute(p: string): boolean {
  return /^([A-Za-z]:[\\/]|[\\/])/.test(p);
}

function windowsy(p: string): boolean {
  return /^[A-Za-z]:/.test(p) || p.startsWith("\\\\");
}

function normalise(p: string): string {
  let s = p.replace(/\\/g, "/");
  while (s.length > 1 && s.endsWith("/") && !/^[A-Za-z]:\/$/.test(s)) s = s.slice(0, -1);
  return s;
}

/**
 * The folder relative to `downloadDir` for a path a client sent: `""` for the download
 * directory itself, `null` for a path outside it or one that climbs out with `..`.
 */
export function relativeSavePath(path: string, downloadDir: string): string | null {
  const p = path.trim();
  if (p === "") return "";
  let rel: string;
  if (isAbsolute(p)) {
    const base = normalise(downloadDir);
    const target = normalise(p);
    const same = windowsy(downloadDir)
      ? (a: string, b: string) => a.toLowerCase() === b.toLowerCase()
      : (a: string, b: string) => a === b;
    if (same(target, base)) return "";
    const prefix = base.endsWith("/") ? base : `${base}/`;
    if (!same(target.slice(0, prefix.length), prefix)) return null;
    rel = target.slice(prefix.length);
  } else {
    rel = normalise(p);
  }
  const parts = rel.split("/").filter((s) => s !== "" && s !== ".");
  if (parts.some((s) => s === ".." || s.includes(":"))) return null;
  return parts.join("/");
}

/** `relative` under `base`, in `base`'s own style of separator. */
export function joinPath(base: string, relative: string): string {
  if (relative === "") return base;
  const sep = base.includes("\\") && !base.includes("/") ? "\\" : "/";
  const rel = relative.split("/").join(sep);
  return base.endsWith(sep) ? `${base}${rel}` : `${base}${sep}${rel}`;
}

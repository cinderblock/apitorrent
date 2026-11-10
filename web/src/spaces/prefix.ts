// Which space the page is in, from its URL. A space's pages live under `/s/<slug>/`; a
// page without the prefix is in the signed-in user's default space (the one they used last),
// which is all there is on an install with a single space.
//
// The API follows the page: `/s/<slug>/api/v1/…` for a space, `/api/v1/…` otherwise. The
// daemon strips the prefix before routing and puts the request in that space.

const PREFIX = /^\/s\/([a-z0-9](?:[a-z0-9-]{0,38}[a-z0-9])?)(?=\/|$)/;

/** The slug in `pathname` (`/s/tv/rules` → `tv`), or null. */
export function slugOf(pathname: string): string | null {
  return PREFIX.exec(pathname)?.[1] ?? null;
}

/** The space prefix of `pathname` (`/s/tv`), or "" for none. */
export function prefixOf(pathname: string): string {
  const slug = slugOf(pathname);
  return slug ? `/s/${slug}` : "";
}

/** `pathname` without its space prefix (`/s/tv/rules` → `/rules`, `/s/tv` → `/`). */
export function stripPrefix(pathname: string): string {
  const p = prefixOf(pathname);
  if (!p) return pathname;
  const rest = pathname.slice(p.length);
  return rest === "" ? "/" : rest;
}

/** An app path inside the space of `pathname` (`/rules` in `/s/tv/…` → `/s/tv/rules`). */
export function withPrefix(pathname: string, path: string): string {
  const p = prefixOf(pathname);
  if (!p || path.startsWith("/s/")) return path;
  return path === "/" ? `${p}/` : `${p}${path}`;
}

/** The current page's space prefix. */
export function currentPrefix(): string {
  return typeof location === "undefined" ? "" : prefixOf(location.pathname);
}

/** The address of a space's torrent list. */
export function spaceHome(slug: string): string {
  return `/s/${slug}/`;
}

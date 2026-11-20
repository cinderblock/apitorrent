// Where a torrent's details show on a wide screen: the panel under the list, or only in
// their own windows. Per device, like the theme. Double-click opens a window either way.

import { useSyncExternalStore } from "react";

import { desktop, isDesktop } from "../desktop/bridge";
import { withPrefix } from "../spaces/prefix";

export type DetailsMode = "panel" | "window";

export const DETAILS_KEY = "apitorrent.details";

export function parseDetailsMode(raw: string | null | undefined): DetailsMode {
  return raw === "window" ? "window" : "panel";
}

function load(): DetailsMode {
  try {
    return parseDetailsMode(localStorage.getItem(DETAILS_KEY));
  } catch {
    return "panel";
  }
}

let current: DetailsMode = load();
const listeners = new Set<() => void>();

export function setDetailsMode(mode: DetailsMode): void {
  current = mode;
  try {
    if (mode === "panel") localStorage.removeItem(DETAILS_KEY);
    else localStorage.setItem(DETAILS_KEY, mode);
  } catch {
    // Storage blocked: the choice lasts for this page only.
  }
  for (const l of listeners) l();
}

function subscribe(l: () => void): () => void {
  listeners.add(l);
  return () => listeners.delete(l);
}

export function useDetailsMode(): DetailsMode {
  return useSyncExternalStore(subscribe, () => current);
}

/** The app path of a torrent's own window. */
export function torrentPath(id: string): string {
  return `/torrent/${encodeURIComponent(id)}`;
}

/** The torrent id in an app path (`/torrent/<id>`), or null. */
export function torrentIdOf(path: string): string | null {
  const m = /^\/torrent\/([^/]+)\/?$/.exec(path);
  if (!m) return null;
  try {
    return decodeURIComponent(m[1]!);
  } catch {
    return null;
  }
}

/**
 * Open a torrent's details in a window of their own, in this page's space. One window per
 * torrent: opening it again brings the existing one forward. The window lives on its own,
 * so it stays open when the list's window closes.
 */
export function openTorrentWindow(id: string): void {
  const path = withPrefix(location.pathname, torrentPath(id));
  if (isDesktop()) {
    void desktop.openWindow(path);
    return;
  }
  // An empty address finds the window by name without reloading it if it's already open.
  const w = window.open("", `apitorrent-torrent-${id}`, "popup,width=900,height=680");
  if (!w) return;
  if (w.location.href === "about:blank") w.location.href = path;
  w.focus();
}

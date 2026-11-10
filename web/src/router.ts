// A minimal client-side router: the path, and a way to change it. The daemon serves
// index.html for unknown paths, so these survive a reload.

import { useSyncExternalStore } from "react";

import { stripPrefix, withPrefix } from "./spaces/prefix";

const listeners = new Set<() => void>();

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  window.addEventListener("popstate", listener);
  return () => {
    listeners.delete(listener);
    window.removeEventListener("popstate", listener);
  };
}

/** Go to an app path (`/rules`), staying in the current space (`/s/<slug>/rules`). */
export function navigate(path: string, { replace = false }: { replace?: boolean } = {}): void {
  const target = withPrefix(location.pathname, path);
  if (replace) history.replaceState(null, "", target);
  else history.pushState(null, "", target);
  for (const l of listeners) l();
}

/** The app path, without the space prefix. */
export function usePath(): string {
  return useSyncExternalStore(subscribe, () => stripPrefix(location.pathname));
}

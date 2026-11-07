// A minimal client-side router: the path, and a way to change it. The daemon serves
// index.html for unknown paths, so these survive a reload.

import { useSyncExternalStore } from "react";

const listeners = new Set<() => void>();

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  window.addEventListener("popstate", listener);
  return () => {
    listeners.delete(listener);
    window.removeEventListener("popstate", listener);
  };
}

export function navigate(path: string, { replace = false }: { replace?: boolean } = {}): void {
  if (replace) history.replaceState(null, "", path);
  else history.pushState(null, "", path);
  for (const l of listeners) l();
}

export function usePath(): string {
  return useSyncExternalStore(subscribe, () => location.pathname);
}

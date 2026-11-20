// How the wide torrent list shows selection: a checkbox on every row (the default), or
// just the highlighted rows and a count. Per device, like the theme.

import { useSyncExternalStore } from "react";

export type SelectionStyle = "boxes" | "count";

export const SELECTION_KEY = "apitorrent.selection";

export function parseSelectionStyle(raw: string | null | undefined): SelectionStyle {
  return raw === "count" ? "count" : "boxes";
}

function load(): SelectionStyle {
  try {
    return parseSelectionStyle(localStorage.getItem(SELECTION_KEY));
  } catch {
    return "boxes";
  }
}

let current: SelectionStyle = load();
const listeners = new Set<() => void>();

export function setSelectionStyle(style: SelectionStyle): void {
  current = style;
  try {
    if (style === "boxes") localStorage.removeItem(SELECTION_KEY);
    else localStorage.setItem(SELECTION_KEY, style);
  } catch {
    // Storage blocked: the choice lasts for this page only.
  }
  for (const l of listeners) l();
}

function subscribe(l: () => void): () => void {
  listeners.add(l);
  return () => listeners.delete(l);
}

export function useSelectionStyle(): SelectionStyle {
  return useSyncExternalStore(subscribe, () => current);
}

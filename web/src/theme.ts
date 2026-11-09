// The colour theme preference: System, Light or Dark, per device.
//
// public/theme-boot.js applies a stored choice before the page renders; this module owns it
// from then on: changing it, applying it, and telling listeners (the toolbar picker, the
// desktop window). Keep the storage key and values in step with theme-boot.js.

import { useSyncExternalStore } from "react";

export type ThemePreference = "system" | "light" | "dark";

export const THEME_PREFERENCES: readonly ThemePreference[] = ["system", "light", "dark"];

export const THEME_KEY = "apitorrent.theme";

export function parseTheme(raw: string | null | undefined): ThemePreference {
  return raw === "light" || raw === "dark" ? raw : "system";
}

function load(): ThemePreference {
  try {
    return parseTheme(localStorage.getItem(THEME_KEY));
  } catch {
    return "system";
  }
}

function store(theme: ThemePreference): void {
  try {
    if (theme === "system") localStorage.removeItem(THEME_KEY);
    else localStorage.setItem(THEME_KEY, theme);
  } catch {
    // Storage blocked: the choice lasts for this page only.
  }
}

/** Point the document at a theme: the same changes theme-boot.js makes, reversibly. */
export function applyTheme(theme: ThemePreference, doc: Document = document): void {
  const root = doc.documentElement;
  if (theme === "system") root.removeAttribute("data-theme");
  else root.setAttribute("data-theme", theme);

  doc
    .querySelector('meta[name="color-scheme"]')
    ?.setAttribute("content", theme === "system" ? "light dark" : theme);

  for (const meta of doc.querySelectorAll('meta[name="theme-color"][data-scheme]')) {
    const scheme = meta.getAttribute("data-scheme");
    meta.setAttribute(
      "media",
      theme === "system" ? `(prefers-color-scheme: ${scheme})` : scheme === theme ? "all" : "not all",
    );
  }
}

let current: ThemePreference = load();
const listeners = new Set<(theme: ThemePreference) => void>();

export function getTheme(): ThemePreference {
  return current;
}

function changed(theme: ThemePreference): void {
  if (theme === current) return;
  current = theme;
  applyTheme(theme);
  for (const listener of listeners) listener(theme);
}

/** Choose a theme for this device. */
export function setTheme(theme: ThemePreference): void {
  store(theme);
  changed(theme);
}

/** Called on every change of preference. Returns the unsubscribe. */
export function onThemeChange(listener: (theme: ThemePreference) => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

// Another tab of the same origin changed it: follow along.
if (typeof window !== "undefined" && typeof window.addEventListener === "function") {
  window.addEventListener("storage", (e) => {
    if (e.key === THEME_KEY || e.key === null) changed(load());
  });
}

/** The current preference, re-rendering on change. */
export function useTheme(): ThemePreference {
  return useSyncExternalStore(onThemeChange, getTheme, () => "system");
}

// Set the web app up for the desktop window: route the API through the app, and keep the
// window's own theme in step with the page's. Call once, before the first render. Does
// nothing in a browser.

import { setTransport } from "../api/transport";
import { getTheme, onThemeChange, type ThemePreference } from "../theme";
import { desktop, isDesktop } from "./bridge";
import { desktopTransport } from "./transport";

function syncWindowTheme(theme: ThemePreference): void {
  desktop.setTheme(theme).catch((e: unknown) => console.warn("[apitorrent] couldn't theme the window", e));
}

export function installDesktop(): boolean {
  if (!isDesktop()) return false;
  setTransport(desktopTransport);
  // The page's choice is the source of truth: the app's saved copy only colours the window
  // before the page loads, so bring it up to date now and on every change.
  syncWindowTheme(getTheme());
  onThemeChange(syncWindowTheme);
  return true;
}

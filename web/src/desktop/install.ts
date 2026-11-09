// Set the web app up for the desktop window: route the API through the app. Call once,
// before the first render. Does nothing in a browser.

import { setTransport } from "../api/transport";
import { isDesktop } from "./bridge";
import { desktopTransport } from "./transport";

export function installDesktop(): boolean {
  if (!isDesktop()) return false;
  setTransport(desktopTransport);
  return true;
}

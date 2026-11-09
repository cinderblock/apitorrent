// How the UI reaches the API. In a browser that's same-origin `fetch` and `WebSocket`. The
// desktop app swaps in its own transport (see `desktop/`), which goes through the app, so
// the page never talks to the network itself.

/** The part of a WebSocket the sync client uses. */
export interface SocketLike {
  onmessage: ((event: MessageEvent) => void) | null;
  onclose: ((event: CloseEvent) => void) | null;
  close(): void;
}

export interface Transport {
  fetch(url: string, init: RequestInit): Promise<Response>;
  openSocket(url: string): SocketLike;
}

const browser: Transport = {
  fetch: (url, init) => fetch(url, init),
  openSocket: (url) => new WebSocket(url),
};

let current: Transport = browser;

/** Replace the transport. Call before the UI first renders. */
export function setTransport(t: Transport): void {
  current = t;
}

export const transport: Transport = {
  fetch: (url, init) => current.fetch(url, init),
  openSocket: (url) => current.openSocket(url),
};

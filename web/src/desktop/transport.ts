// The API, through the desktop app: requests and the sync socket go to Rust, which talks to
// the daemon over verified TLS (adding the API token) or to the core in-process.

import type { SocketLike, Transport } from "../api/transport";
import { Channel, desktop, type SyncEvent } from "./bridge";

/** The path and query of a same-origin URL such as `/api/v1/torrents?x=1`. */
export function apiPath(url: string): string {
  const u = new URL(url, "http://app.invalid");
  return u.pathname + u.search;
}

/** Statuses whose responses may not carry a body (the Response constructor refuses one). */
const NULL_BODY = new Set([101, 103, 204, 205, 304]);

async function desktopFetch(url: string, init: RequestInit): Promise<Response> {
  const headers = new Headers(init.headers);
  const body = init.body;
  if (body !== undefined && body !== null && typeof body !== "string") {
    throw new TypeError("the desktop transport sends text bodies only");
  }
  const r = await desktop.request(init.method ?? "GET", apiPath(url), body ?? null, headers.get("content-type"));
  const responseHeaders = new Headers();
  if (r.contentType) responseHeaders.set("content-type", r.contentType);
  return new Response(NULL_BODY.has(r.status) ? null : r.body, { status: r.status, headers: responseHeaders });
}

/** The sync socket, as events on a Tauri channel. */
export class DesktopSocket implements SocketLike {
  onmessage: ((event: MessageEvent) => void) | null = null;
  onclose: ((event: CloseEvent) => void) | null = null;
  private id: number | null = null;
  private closed = false;

  constructor() {
    const channel = new Channel<SyncEvent>();
    channel.onmessage = (e) => this.handle(e);
    desktop.syncOpen(channel).then(
      (id) => {
        this.id = id;
        // Closed before the app answered: close the app's side too.
        if (this.closed) void desktop.syncClose(id);
      },
      (err: unknown) => this.finish(1006, err instanceof Error ? err.message : String(err)),
    );
  }

  private handle(e: SyncEvent): void {
    if (this.closed) return;
    if (e.event === "message") this.onmessage?.(new MessageEvent("message", { data: e.data }));
    else if (e.event === "close") this.finish(e.code, e.reason);
  }

  private finish(code: number, reason: string): void {
    if (this.closed) return;
    this.closed = true;
    if (reason) console.warn(`[apitorrent] sync closed (${code}): ${reason}`);
    this.onclose?.(new CloseEvent("close", { code, reason }));
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    if (this.id !== null) void desktop.syncClose(this.id);
    // A browser WebSocket reports its own close() too.
    this.onclose?.(new CloseEvent("close", { code: 1000 }));
  }
}

export const desktopTransport: Transport = {
  fetch: desktopFetch,
  openSocket: () => new DesktopSocket(),
};

// The live half of the API: a WebSocket that delivers a snapshot, then patches. The client
// reconnects with backoff and resynchronises from a fresh snapshot after any gap.

import { notifyAuthLost } from "./client";
import type { SyncMessage } from "./generated/SyncMessage";
import { applySync, initialSyncState, type SyncState } from "../state/torrents";
import { transport, type SocketLike } from "./transport";
import { currentPrefix } from "../spaces/prefix";

export type ConnectionStatus = "connecting" | "live" | "reconnecting";

export interface SyncSnapshot {
  state: SyncState;
  status: ConnectionStatus;
}

type Listener = () => void;

const MAX_BACKOFF_MS = 10_000;

/** How long the socket stays open after the last page using it goes (a trip to Settings),
 * so coming back shows live data at once instead of waiting for a fresh snapshot. */
const LINGER_MS = 2 * 60_000;

/** The server closed the socket because its credential ended (signed out, revoked, expired). */
export const CLOSE_UNAUTHORIZED = 4401;

export class SyncClient {
  private snapshot: SyncSnapshot = { state: initialSyncState, status: "connecting" };
  private listeners = new Set<Listener>();
  private socket: SocketLike | null = null;
  private retries = 0;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private stopped = true;
  private users = 0;
  private linger: ReturnType<typeof setTimeout> | null = null;

  constructor(private readonly url: string) {}

  static forCurrentPage(): SyncClient {
    const scheme = location.protocol === "https:" ? "wss:" : "ws:";
    // The first snapshot is large (about 6 MiB of JSON for 10,000 torrents), and WebSockets
    // here have no compression of their own: ask for big messages gzipped, as binary. And
    // ask for it in parts, so the list fills in as it arrives.
    const query = new URLSearchParams({ chunks: "1" });
    if (typeof DecompressionStream === "function") query.set("compress", "gzip");
    return new SyncClient(`${scheme}//${location.host}${currentPrefix()}/api/v1/sync?${query}`);
  }

  /** A page needs live torrents: connect, or keep a lingering connection. Pair with `release`. */
  acquire(): void {
    this.users++;
    if (this.linger) {
      clearTimeout(this.linger);
      this.linger = null;
    }
    if (this.stopped) this.start();
  }

  /** The page is done; the connection lingers a while in case it comes back. */
  release(): void {
    this.users = Math.max(0, this.users - 1);
    if (this.users > 0 || this.stopped) return;
    this.linger = setTimeout(() => {
      this.linger = null;
      this.stop();
    }, LINGER_MS);
  }

  start(): void {
    if (!this.stopped) return;
    this.stopped = false;
    // Whatever is shown is from before: say so until a fresh snapshot arrives.
    if (this.snapshot.status !== "connecting") this.set({ status: "connecting" });
    this.connect();
  }

  stop(): void {
    this.stopped = true;
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
    const ws = this.socket;
    this.socket = null;
    ws?.close();
  }

  /** For `useSyncExternalStore`. */
  subscribe = (listener: Listener): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  getSnapshot = (): SyncSnapshot => this.snapshot;

  private set(next: Partial<SyncSnapshot>): void {
    this.snapshot = { ...this.snapshot, ...next };
    for (const l of this.listeners) l();
  }

  private connect(): void {
    const ws = transport.openSocket(this.url);
    this.socket = ws;
    // Unpacking a gzipped message is asynchronous, so messages are handled strictly one
    // after another: a patch must never overtake the snapshot it follows.
    let queue: Promise<void> = Promise.resolve();
    ws.onmessage = (event) => {
      queue = queue.then(async () => {
        if (this.socket !== ws) return;
        let msg: SyncMessage;
        try {
          msg = JSON.parse(await messageText(event.data)) as SyncMessage;
        } catch {
          console.warn("[apitorrent] unreadable sync message");
          return;
        }
        this.apply(ws, msg);
      });
    };
    ws.onclose = (event) => this.closed(ws, event);
  }

  private apply(ws: SocketLike, msg: SyncMessage): void {
    const result = applySync(this.snapshot.state, msg);
    if (result.ok) {
      this.retries = 0;
      this.set({ state: result.state, status: "live" });
    } else {
      // We missed a patch. Reconnecting gets us a fresh snapshot.
      console.warn(`[apitorrent] resyncing: ${result.reason}`);
      ws.close();
    }
  }

  private closed(ws: SocketLike, event: CloseEvent): void {
    if (this.socket !== ws) return;
    // The session may have ended, or just been replaced by a newer one (after re-signing
    // in). Let the app re-check; reconnecting below picks up a newer session's cookie.
    if (event.code === CLOSE_UNAUTHORIZED) notifyAuthLost();
    this.socket = null;
    if (this.stopped) return;
    this.set({ status: "reconnecting" });
    const delay = Math.min(MAX_BACKOFF_MS, 250 * 2 ** this.retries);
    this.retries++;
    this.timer = setTimeout(() => this.connect(), delay);
  }
}

/** A message's text: as sent, or gunzipped from a binary message. */
async function messageText(data: unknown): Promise<string> {
  if (typeof data === "string") return data;
  const blob = data instanceof Blob ? data : new Blob([data as ArrayBuffer]);
  return new Response(blob.stream().pipeThrough(new DecompressionStream("gzip"))).text();
}

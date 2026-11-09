// The live half of the API: a WebSocket that delivers a snapshot, then patches. The client
// reconnects with backoff and resynchronises from a fresh snapshot after any gap.

import { notifyAuthLost } from "./client";
import type { SyncMessage } from "./generated/SyncMessage";
import { applySync, initialSyncState, type SyncState } from "../state/torrents";
import { transport, type SocketLike } from "./transport";

export type ConnectionStatus = "connecting" | "live" | "reconnecting";

export interface SyncSnapshot {
  state: SyncState;
  status: ConnectionStatus;
}

type Listener = () => void;

const MAX_BACKOFF_MS = 10_000;

/** The server closed the socket because its credential ended (signed out, revoked, expired). */
export const CLOSE_UNAUTHORIZED = 4401;

export class SyncClient {
  private snapshot: SyncSnapshot = { state: initialSyncState, status: "connecting" };
  private listeners = new Set<Listener>();
  private socket: SocketLike | null = null;
  private retries = 0;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private stopped = false;

  constructor(private readonly url: string) {}

  static forCurrentPage(): SyncClient {
    const scheme = location.protocol === "https:" ? "wss:" : "ws:";
    return new SyncClient(`${scheme}//${location.host}/api/v1/sync`);
  }

  start(): void {
    this.stopped = false;
    this.connect();
  }

  stop(): void {
    this.stopped = true;
    if (this.timer) clearTimeout(this.timer);
    this.socket?.close();
    this.socket = null;
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
    ws.onmessage = (event) => {
      let msg: SyncMessage;
      try {
        msg = JSON.parse(String(event.data)) as SyncMessage;
      } catch {
        console.warn("[apitorrent] unparseable sync message");
        return;
      }
      const result = applySync(this.snapshot.state, msg);
      if (result.ok) {
        this.retries = 0;
        this.set({ state: result.state, status: "live" });
      } else {
        // We missed a patch. Reconnecting gets us a fresh snapshot.
        console.warn(`[apitorrent] resyncing: ${result.reason}`);
        ws.close();
      }
    };
    ws.onclose = (event) => {
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
    };
  }
}

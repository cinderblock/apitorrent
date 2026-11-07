// The REST half of the API. Every call is same-origin: in development Vite proxies /api to
// the daemon, and in production the daemon serves this UI itself.

import type { AddTorrentRequest } from "./generated/AddTorrentRequest";
import type { AddTorrentResponse } from "./generated/AddTorrentResponse";
import type { ApiError } from "./generated/ApiError";
import type { FilePriority } from "./generated/FilePriority";
import type { QueueMove } from "./generated/QueueMove";
import type { Settings } from "./generated/Settings";
import type { TorrentLimits } from "./generated/TorrentLimits";
import type { ServerInfo } from "./generated/ServerInfo";
import type { TorrentDetail } from "./generated/TorrentDetail";
import type { UnwantedPolicy } from "./generated/UnwantedPolicy";

const BASE = "/api/v1";

export class ApiRequestError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message);
    this.name = "ApiRequestError";
  }
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const init: RequestInit = { method, headers: {} };
  if (body !== undefined) {
    init.body = JSON.stringify(body);
    init.headers = { "content-type": "application/json" };
  }
  const resp = await fetch(`${BASE}${path}`, init);
  if (!resp.ok) {
    let err: ApiError = { code: "http_error", message: `${resp.status} ${resp.statusText}` };
    try {
      err = (await resp.json()) as ApiError;
    } catch {
      // Not a JSON error body (a proxy error page, say); keep the status line.
    }
    throw new ApiRequestError(resp.status, err.code, err.message);
  }
  if (resp.status === 204) return undefined as T;
  return (await resp.json()) as T;
}

const enc = encodeURIComponent;

export const api = {
  info: () => request<ServerInfo>("GET", "/info"),
  detail: (id: string) => request<TorrentDetail>("GET", `/torrents/${enc(id)}`),
  add: (req: AddTorrentRequest) => request<AddTorrentResponse>("POST", "/torrents", req),
  pause: (id: string) => request<void>("POST", `/torrents/${enc(id)}/pause`),
  resume: (id: string) => request<void>("POST", `/torrents/${enc(id)}/resume`),
  remove: (id: string, deleteFiles: boolean) =>
    request<void>("DELETE", `/torrents/${enc(id)}?deleteFiles=${deleteFiles}`),
  setFilePriorities: (id: string, priorities: FilePriority[]) =>
    request<void>("PUT", `/torrents/${enc(id)}/files/priorities`, { priorities }),
  settings: () => request<Settings>("GET", "/settings"),
  saveSettings: (s: Settings) => request<void>("PUT", "/settings", s),
  setLimits: (id: string, limits: TorrentLimits) => request<void>("PUT", `/torrents/${enc(id)}/limits`, limits),
  setLabel: (id: string, label: string | null) => request<void>("PUT", `/torrents/${enc(id)}/label`, { label }),
  queueMove: (id: string, to: QueueMove) => request<void>("POST", `/torrents/${enc(id)}/queue`, { to }),
  setUnwantedPolicy: (id: string, policy: UnwantedPolicy) =>
    request<void>("PUT", `/torrents/${enc(id)}/unwanted-policy`, { policy }),
  /** Delete held copies of deselected files; all of them when `files` is omitted. */
  releaseHeld: (id: string, files?: number[]) =>
    request<void>("POST", `/torrents/${enc(id)}/held/release`, { files: files ?? null }),
};

/** Read a File as base64, for `AddSource.torrentFile`. */
export async function fileToBase64(file: File): Promise<string> {
  const bytes = new Uint8Array(await file.arrayBuffer());
  let binary = "";
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) {
    binary += String.fromCharCode(...bytes.subarray(i, i + chunk));
  }
  return btoa(binary);
}

// The REST half of the API. Every call is same-origin: in development Vite proxies /api to
// the daemon, and in production the daemon serves this UI itself. The session cookie is
// HttpOnly and SameSite=Strict, so the browser attaches it and scripts never see it.

import type { AddPasskeyFinishRequest } from "./generated/AddPasskeyFinishRequest";
import type { AddTorrentRequest } from "./generated/AddTorrentRequest";
import type { Ban } from "./generated/Ban";
import type { SeedLimits } from "./generated/SeedLimits";
import type { TlsStatus } from "./generated/TlsStatus";
import type { SessionStats } from "./generated/SessionStats";
import type { AddTorrentResponse } from "./generated/AddTorrentResponse";
import type { ApiError } from "./generated/ApiError";
import type { ApiTokenInfo } from "./generated/ApiTokenInfo";
import type { AuthStatus } from "./generated/AuthStatus";
import type { CeremonyStart } from "./generated/CeremonyStart";
import type { CreatedToken } from "./generated/CreatedToken";
import type { CreateTokenRequest } from "./generated/CreateTokenRequest";
import type { ApproveDeviceRequest } from "./generated/ApproveDeviceRequest";
import type { DeviceInfo } from "./generated/DeviceInfo";
import type { DeviceRequestInfo } from "./generated/DeviceRequestInfo";
import type { FilePriority } from "./generated/FilePriority";
import type { ImportDelugeRequest } from "./generated/ImportDelugeRequest";
import type { ImportReport } from "./generated/ImportReport";
import type { QueueMove } from "./generated/QueueMove";
import type { Settings } from "./generated/Settings";
import type { StorageRoot } from "./generated/StorageRoot";
import type { TorrentLimits } from "./generated/TorrentLimits";
import type { PasskeyInfo } from "./generated/PasskeyInfo";
import type { ServerInfo } from "./generated/ServerInfo";
import type { SessionInfo } from "./generated/SessionInfo";
import type { SetupPasswordRequest } from "./generated/SetupPasswordRequest";
import type { SetupStartRequest } from "./generated/SetupStartRequest";
import type { TorrentDetail } from "./generated/TorrentDetail";
import type { UnwantedPolicy } from "./generated/UnwantedPolicy";
import type { UpdateSettings } from "./generated/UpdateSettings";
import type { UpdateStatus } from "./generated/UpdateStatus";
import { currentPrefix } from "../spaces/prefix";
import { transport } from "./transport";

/** The API, in the page's space (`/s/<slug>/api/v1` under a space's pages). */
function base(): string {
  return `${currentPrefix()}/api/v1`;
}

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

// Errors that mean "you're not (fully) signed in any more": the UI re-checks its status.
const AUTH_LOST = new Set(["unauthenticated", "second_factor_required"]);
const authLostListeners = new Set<() => void>();

/** Called when a request shows the session has ended (expired, revoked, signed out). */
export function onAuthLost(listener: () => void): () => void {
  authLostListeners.add(listener);
  return () => authLostListeners.delete(listener);
}

export function notifyAuthLost(): void {
  for (const l of authLostListeners) l();
}

export async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const init: RequestInit = { method, headers: {}, credentials: "same-origin" };
  if (body !== undefined) {
    init.body = JSON.stringify(body);
    init.headers = { "content-type": "application/json" };
  }
  const resp = await transport.fetch(`${base()}${path}`, init);
  if (!resp.ok) {
    let err: ApiError = { code: "http_error", message: `${resp.status} ${resp.statusText}` };
    try {
      err = (await resp.json()) as ApiError;
    } catch {
      // Not a JSON error body (a proxy error page, say); keep the status line.
    }
    if (AUTH_LOST.has(err.code)) notifyAuthLost();
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
  stats: () => request<SessionStats>("GET", "/stats"),
  /** The instance's storage roots and where other computers reach them. */
  roots: () => request<StorageRoot[]>("GET", "/roots"),
  /** Replace a root's share locators (server admins); answers with the normalised root. */
  setRootLocators: (id: string, locators: string[]) => request<StorageRoot>("PUT", `/roots/${enc(id)}/locators`, { locators }),
  bans: () => request<Ban[]>("GET", "/bans"),
  ban: (ip: string) => request<void>("POST", "/bans", { ip }),
  unban: (ip: string) => request<void>("DELETE", `/bans/${enc(ip)}`),
  importDeluge: (req: ImportDelugeRequest) => request<ImportReport>("POST", "/import/deluge", req),
  saveSettings: (s: Settings) => request<void>("PUT", "/settings", s),
  setLimits: (id: string, limits: TorrentLimits) => request<void>("PUT", `/torrents/${enc(id)}/limits`, limits),
  setLabel: (id: string, label: string | null) => request<void>("PUT", `/torrents/${enc(id)}/label`, { label }),
  /** This space's announce tiers for the torrent (each URL once). */
  setTrackers: (id: string, tiers: string[][]) => request<void>("PUT", `/torrents/${enc(id)}/trackers`, { tiers }),
  /** The server's HTTPS: certificate, pairing code, ACME progress. */
  tls: () => request<TlsStatus>("GET", "/tls"),
  /** A torrent's own seed limits; null follows its group's or the instance's. */
  setSeedLimits: (id: string, limits: SeedLimits | null) => request<void>("PUT", `/torrents/${enc(id)}/seed-limits`, { limits }),
  queueMove: (id: string, to: QueueMove) => request<void>("POST", `/torrents/${enc(id)}/queue`, { to }),
  setUnwantedPolicy: (id: string, policy: UnwantedPolicy) =>
    request<void>("PUT", `/torrents/${enc(id)}/unwanted-policy`, { policy }),
  /** Delete held copies of deselected files; all of them when `files` is omitted. */
  releaseHeld: (id: string, files?: number[]) =>
    request<void>("POST", `/torrents/${enc(id)}/held/release`, { files: files ?? null }),
};

/** `/api/v1/system/update`: the daemon's own updates (admins only). */
export const updateApi = {
  status: () => request<UpdateStatus>("GET", "/system/update"),
  check: () => request<UpdateStatus>("POST", "/system/update/check"),
  /** Answers once the new binary is in place; the daemon then restarts. */
  install: () => request<UpdateStatus>("POST", "/system/update/install"),
  setSettings: (s: UpdateSettings) => request<UpdateStatus>("PUT", "/system/update/settings", s),
};

/** `/api/v1/auth`. Sign-in calls answer with the new status and set the session cookie. */
export const authApi = {
  status: () => request<AuthStatus>("GET", "/auth/status"),

  setupStart: (req: SetupStartRequest) => request<CeremonyStart>("POST", "/auth/setup/start", req),
  setupFinish: (ceremonyId: string, credential: unknown, passkeyName: string | null) =>
    request<AuthStatus>("POST", "/auth/setup/finish", { ceremonyId, credential, passkeyName }),
  setupPassword: (req: SetupPasswordRequest) => request<AuthStatus>("POST", "/auth/setup/password", req),

  loginStart: () => request<CeremonyStart>("POST", "/auth/login/passkey/start"),
  loginFinish: (ceremonyId: string, credential: unknown) =>
    request<AuthStatus>("POST", "/auth/login/passkey/finish", { ceremonyId, credential }),
  /** A password sign-in, or the password step after a passkey (second-factor mode). */
  loginPassword: (username: string | null, password: string) =>
    request<AuthStatus>("POST", "/auth/login/password", { username, password }),
  logout: () => request<void>("POST", "/auth/logout"),

  reauthStart: () => request<CeremonyStart>("POST", "/auth/reauth/passkey/start"),
  reauthFinish: (ceremonyId: string, credential: unknown) =>
    request<AuthStatus>("POST", "/auth/reauth/passkey/finish", { ceremonyId, credential }),
  reauthPassword: (password: string) => request<AuthStatus>("POST", "/auth/reauth/password", { password }),

  passkeys: () => request<PasskeyInfo[]>("GET", "/auth/passkeys"),
  passkeyStart: () => request<CeremonyStart>("POST", "/auth/passkeys/start"),
  passkeyFinish: (req: AddPasskeyFinishRequest) => request<PasskeyInfo>("POST", "/auth/passkeys/finish", req),
  renamePasskey: (id: string, name: string) => request<void>("PATCH", `/auth/passkeys/${enc(id)}`, { name }),
  removePasskey: (id: string) => request<void>("DELETE", `/auth/passkeys/${enc(id)}`),

  setPassword: (password: string) => request<void>("PUT", "/auth/password", { password }),
  removePassword: () => request<void>("DELETE", "/auth/password"),

  tokens: () => request<ApiTokenInfo[]>("GET", "/auth/tokens"),
  createToken: (req: CreateTokenRequest) => request<CreatedToken>("POST", "/auth/tokens", req),
  revokeToken: (id: string) => request<void>("DELETE", `/auth/tokens/${enc(id)}`),

  /** Paired devices (the desktop app); a device sees only itself. */
  devices: () => request<DeviceInfo[]>("GET", "/auth/devices"),
  removeDevice: (id: string) => request<void>("DELETE", `/auth/devices/${enc(id)}`),
  /** A pairing request by the code its app shows. */
  deviceLookup: (userCode: string) => request<DeviceRequestInfo>("POST", "/auth/devices/lookup", { userCode }),
  /** Needs a recent sign-in, like creating a token. */
  deviceApprove: (req: ApproveDeviceRequest) => request<void>("POST", "/auth/devices/approve", req),
  deviceDeny: (userCode: string) => request<void>("POST", "/auth/devices/deny", { userCode }),

  sessions: () => request<SessionInfo[]>("GET", "/auth/sessions"),
  revokeSession: (id: string) => request<void>("DELETE", `/auth/sessions/${enc(id)}`),
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

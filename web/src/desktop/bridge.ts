// The desktop app's commands, typed. Everything here exists only inside the desktop app:
// check `isDesktop()` first. Types mirror `apps/desktop/src-tauri/src` (profiles.rs,
// mapping.rs, commands.rs).

import { Channel, invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/** The capability check: true only in the desktop app's window. */
export function isDesktop(): boolean {
  return isTauri();
}

export type Trust =
  | { kind: "os" }
  | { kind: "privateCa"; caPem: string }
  | { kind: "pinned"; spkiSha256: string };

export interface PathMapping {
  /** A directory on the server, as the server writes it. */
  remote: string;
  /** Where it's reached from this machine. */
  local: string;
}

export interface LanSettings {
  listen: string;
  trustedProxies: string[];
  publicUrls: string[];
  allowInsecureLan: boolean;
}

export interface RemoteProfile {
  mode: "remote";
  baseUrl: string;
  trust: Trust;
  mappings: PathMapping[];
}

export interface LocalProfile {
  mode: "local";
  dataDir: string | null;
  downloadDir: string | null;
  peerPort: number;
  lan: LanSettings | null;
}

export type Profile = { id: string; name: string } & (RemoteProfile | LocalProfile);

export type ProfileView = Profile & { hasToken: boolean };

export interface LanStatus {
  listen: string;
  error: string | null;
  setupLink: string | null;
}

export interface ConnectionView {
  profileId: string;
  local: boolean;
  lan: LanStatus | null;
}

export interface DesktopState {
  version: string;
  platform: string;
  profiles: ProfileView[];
  activeId: string | null;
  connection: ConnectionView | null;
  connectError: string | null;
  notice: string | null;
}

export interface ProbeView {
  presented: string | null;
  presentedCode: string | null;
  accepted: boolean;
  error: string | null;
}

export type MarkerCheck =
  | { result: "ok" }
  | { result: "missing" }
  | { result: "wrongRoot"; found: string }
  | { result: "unreadable"; error: string };

export interface RootCheck {
  rootId: string;
  rootPath: string;
  marker: string;
  check: MarkerCheck;
}

export interface MappingStatus {
  verified: boolean;
  checks: RootCheck[];
  problem: string | null;
}

export interface Suggestion {
  remote: string;
  local: string | null;
}

export type IncomingLink =
  | { kind: "magnet"; uri: string }
  | { kind: "torrentFile"; name: string; data: string }
  | { kind: "problem"; message: string };

export type PathAction = "openFolder" | "reveal" | "open";

export interface ApiResponse {
  status: number;
  contentType: string | null;
  body: string;
}

export type SyncEvent =
  | { event: "open" }
  | { event: "message"; data: string }
  | { event: "close"; code: number; reason: string };

/** Command errors arrive as plain strings; make them Errors. */
async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(cmd, args);
  } catch (e) {
    throw e instanceof Error ? e : new Error(String(e));
  }
}

export const desktop = {
  state: () => call<DesktopState>("desktop_state"),
  /** `token`: undefined keeps the saved one, "" removes it. */
  saveProfile: (profile: Profile, token?: string) =>
    call<ProfileView>("profile_save", { profile, token: token ?? null }),
  deleteProfile: (id: string) => call<void>("profile_delete", { id }),
  activate: (id: string) => call<void>("profile_activate", { id }),
  disconnect: () => call<void>("profile_disconnect"),
  probe: (baseUrl: string, trust?: Trust) => call<ProbeView>("trust_probe", { baseUrl, trust: trust ?? null }),
  pair: (baseUrl: string, code: string) => call<Trust>("trust_pair", { baseUrl, code }),
  request: (method: string, path: string, body: string | null, contentType: string | null) =>
    call<ApiResponse>("api_request", { method, path, body, contentType }),
  syncOpen: (channel: Channel<SyncEvent>) => call<number>("sync_open", { channel }),
  syncClose: (id: number) => call<void>("sync_close", { id }),
  takeLinks: () => call<IncomingLink[]>("take_pending_links"),
  checkMappings: (mappings?: PathMapping[]) => call<MappingStatus[]>("mappings_check", { mappings: mappings ?? null }),
  suggestions: () => call<Suggestion[]>("mapping_suggestions"),
  pathAction: (action: PathAction, savePath: string, file?: string) =>
    call<void>("path_action", { action, savePath, file: file ?? null }),
  /** null when the path can be opened here, else why not. */
  pathAvailable: (savePath: string) => call<string | null>("path_available", { savePath }),
  /** Called when the OS hands the app magnet links or .torrent files. */
  onLinks: (handler: () => void) => listen("apitorrent://links", handler),
};

export { Channel };

// `/api/v1/plugins`: plugin management. Admin-only on the server; installing, approving
// and uninstalling also need a recent sign-in (`reauth_required` otherwise).

import { request } from "../api/client";
import type { InstallPluginResponse } from "../api/generated/InstallPluginResponse";
import type { PluginBundle } from "../api/generated/PluginBundle";
import type { PluginInfo } from "../api/generated/PluginInfo";
import type { PluginLogEntry } from "../api/generated/PluginLogEntry";
import type { PluginSettings } from "../api/generated/PluginSettings";

const enc = encodeURIComponent;

export const pluginsApi = {
  list: () => request<PluginInfo[]>("GET", "/plugins"),
  install: (bundle: PluginBundle) => request<InstallPluginResponse>("POST", "/plugins", bundle),
  approve: (id: string, version: string) =>
    request<PluginInfo>("POST", `/plugins/${enc(id)}/-/approve`, { version }),
  enable: (id: string) => request<PluginInfo>("POST", `/plugins/${enc(id)}/-/enable`),
  disable: (id: string) => request<PluginInfo>("POST", `/plugins/${enc(id)}/-/disable`),
  uninstall: (id: string) => request<void>("DELETE", `/plugins/${enc(id)}`),
  logs: (id: string, after?: number) =>
    request<PluginLogEntry[]>("GET", `/plugins/${enc(id)}/-/logs${after ? `?after=${after}` : ""}`),
  settings: (id: string) => request<PluginSettings>("GET", `/plugins/${enc(id)}/-/settings`),
  saveSettings: (id: string, values: Record<string, unknown>) =>
    request<PluginSettings>("PUT", `/plugins/${enc(id)}/-/settings`, { values }),
};

/** Read a `*.plugin.json` file chosen by the user. */
export async function readBundle(file: File): Promise<PluginBundle> {
  let parsed: unknown;
  try {
    parsed = JSON.parse(await file.text());
  } catch {
    throw new Error(`${file.name} isn't a plugin bundle (not JSON)`);
  }
  const b = parsed as Partial<PluginBundle>;
  if (typeof b !== "object" || b === null || typeof b.code !== "string" || !b.manifest) {
    throw new Error(`${file.name} isn't a plugin bundle (build one with apitorrent-plugin-build)`);
  }
  return b as PluginBundle;
}

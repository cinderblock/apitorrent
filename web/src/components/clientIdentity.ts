import type { ClientIdentity } from "../api/generated/ClientIdentity";
import type { IdentitySettings } from "../api/generated/IdentitySettings";
import type { PeerWire } from "../api/generated/PeerWire";
import type { TrackerIdentityRule } from "../api/generated/TrackerIdentityRule";

export type ClientKind = ClientIdentity["client"];

export const CLIENT_LABELS: Record<ClientKind, string> = {
  apitorrent: "apitorrent",
  deluge: "Deluge (libtorrent)",
  qBittorrent: "qBittorrent",
  transmission: "Transmission",
  custom: "Custom",
};

/** libtorrent 2.0's handshake (Deluge, qBittorrent), and the default for a custom identity. Mirrors `WireStyle` in the engine. */
export const LIBTORRENT_WIRE: PeerWire = { dhtBit: "always", v2Bit: true, requestQueue: 2000, libtorrentExtensions: true };
const TRANSMISSION_WIRE: PeerWire = { dhtBit: "publicWhenRunning", v2Bit: false, requestQueue: 512, libtorrentExtensions: false };
const APITORRENT_WIRE: PeerWire = { dhtBit: "whenRunning", v2Bit: true, requestQueue: 2000, libtorrentExtensions: true };

/** The largest request queue (`reqq`) an identity may give. */
export const MAX_REQUEST_QUEUE = 10000;

/** What peers see of an identity's handshakes. */
export function wireOf(id: ClientIdentity): PeerWire {
  switch (id.client) {
    case "apitorrent":
      return APITORRENT_WIRE;
    case "deluge":
    case "qBittorrent":
      return LIBTORRENT_WIRE;
    case "transmission":
      return TRANSMISSION_WIRE;
    case "custom":
      return id.wire;
  }
}

/** A fresh identity of each kind, at the newest stable versions this was written against. */
export function presetIdentity(kind: ClientKind): ClientIdentity {
  switch (kind) {
    case "apitorrent":
      return { client: "apitorrent" };
    case "deluge":
      return { client: "deluge", version: "2.1.1", libtorrent: "2.0.10.0" };
    case "qBittorrent":
      return { client: "qBittorrent", version: "4.6.5" };
    case "transmission":
      return { client: "transmission", version: "4.0.6" };
    case "custom":
      return { client: "custom", peerIdPrefix: "-XX0000-", userAgent: "", clientName: "", wire: { ...LIBTORRENT_WIRE } };
  }
}

/** `2.1.1` → [2, 1, 1] and the suffix (`.dev0`, `-beta.2`); null if it isn't a version. Mirrors the server. */
function splitVersion(version: string): { parts: number[]; suffix: string } | null {
  const v = version.trim();
  const m = /^[0-9.]*/.exec(v);
  const numeric = (m?.[0] ?? "").replace(/\.+$/, "");
  const suffix = v.slice(numeric.length);
  const pieces = numeric.split(".");
  if (numeric === "" || pieces.length > 4 || pieces.some((p) => p === "")) return null;
  if (!/^[\x21-\x7e]*$/.test(suffix)) return null;
  return { parts: pieces.map(Number), suffix: suffix.toLowerCase() };
}

const BASE36 = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const BASE62 = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

function azureus(code: string, parts: number[], alphabet: string, width: number): string | null {
  let digits = "";
  for (let i = 0; i < width; i++) {
    const c = alphabet[parts[i] ?? 0];
    if (c === undefined) return null;
    digits += c;
  }
  return `-${code}${digits}`;
}

const printable = (s: string) => s.trim() !== "" && s.length <= 200 && /^[\x20-\x7e]*$/.test(s);

/**
 * What trackers and peers see for an identity: the start of its peer id, its user agent and
 * the client name peers see. Null when a field is not valid (the server would refuse it).
 */
export function describeIdentity(id: ClientIdentity): { peerIdPrefix: string; userAgent: string; clientName: string } | null {
  switch (id.client) {
    case "apitorrent":
      return { peerIdPrefix: "-AT", userAgent: "apitorrent/…", clientName: "apitorrent …" };
    case "deluge": {
      const v = splitVersion(id.version);
      if (!v || !splitVersion(id.libtorrent)) return null;
      const base = azureus("DE", v.parts, BASE36, 4);
      if (!base) return null;
      const s = v.suffix.replace(/^[.\-_]+/, "");
      const release = v.suffix.includes("dev") ? "D" : /^[abr]/.test(s) ? s[0]! : "s";
      const ua = `Deluge/${id.version.trim()} libtorrent/${id.libtorrent.trim()}`;
      return { peerIdPrefix: `${base.slice(0, 6)}${release}-`, userAgent: ua, clientName: ua };
    }
    case "qBittorrent": {
      const v = splitVersion(id.version);
      const base = v && azureus("qB", v.parts, BASE36, 4);
      if (!base) return null;
      const ua = `qBittorrent/${id.version.trim()}`;
      return { peerIdPrefix: `${base}-`, userAgent: ua, clientName: ua };
    }
    case "transmission": {
      const v = splitVersion(id.version);
      const base = v && azureus("TR", v.parts, BASE62, 3);
      if (!v || !base) return null;
      const release = v.suffix.includes("dev") ? "Z" : v.suffix.includes("beta") ? "B" : "0";
      return {
        peerIdPrefix: `${base}${release}-`,
        userAgent: `Transmission/${id.version.trim()}`,
        clientName: `Transmission ${id.version.trim()}`,
      };
    }
    case "custom": {
      const p = id.peerIdPrefix;
      if (p.length === 0 || p.length > 20 || !/^[\x21-\x7e]+$/.test(p)) return null;
      if (!printable(id.userAgent) || !printable(id.clientName)) return null;
      const q = id.wire.requestQueue;
      if (!Number.isInteger(q) || q < 1 || q > MAX_REQUEST_QUEUE) return null;
      return { peerIdPrefix: p, userAgent: id.userAgent.trim(), clientName: id.clientName.trim() };
    }
  }
}

/** Why a host pattern won't do, or null. `tracker.example.org` or `*.example.org`. */
export function hostProblem(host: string): string | null {
  const h = host.trim().replace(/\.$/, "").toLowerCase();
  if (h === "") return "Enter the tracker's host name";
  if (h.includes("/") || (h.includes(":") && !h.startsWith("["))) return "Just the host name, without http:// or a port";
  const name = h.startsWith("*.") ? h.slice(2) : h;
  if (/^\[[0-9a-f:.]+\]$/.test(name) && !h.startsWith("*.")) return null;
  const ok = name.split(".").every((label) => /^[a-z0-9_-]+$/.test(label));
  return ok ? null : "Not a host name: use tracker.example.org, or *.example.org for every name under it";
}

/** Normalised for comparing: lower case, no trailing dot. */
export function normalizeHost(host: string): string {
  return host.trim().replace(/\.$/, "").toLowerCase();
}

/** Problems that stop the settings from saving, one line each. */
export function identityProblems(s: IdentitySettings): string[] {
  const out: string[] = [];
  if (!describeIdentity(s.default)) out.push("The default identity has a field that isn't valid");
  const seen = new Set<string>();
  s.rules.forEach((r, i) => {
    const label = r.host.trim() || `Rule ${i + 1}`;
    const p = hostProblem(r.host);
    if (p) out.push(`${label}: ${p}`);
    const key = normalizeHost(r.host);
    if (key !== "" && seen.has(key)) out.push(`${label} has two rules`);
    seen.add(key);
    if (!describeIdentity(r.identity)) out.push(`${label}: a field of its identity isn't valid`);
  });
  return out;
}

/** Move rule `i` up (-1) or down (+1). Order matters: the first rule that matches wins. */
export function moveRule(rules: TrackerIdentityRule[], i: number, delta: -1 | 1): TrackerIdentityRule[] {
  const j = i + delta;
  if (j < 0 || j >= rules.length) return rules;
  const out = [...rules];
  [out[i], out[j]] = [out[j]!, out[i]!];
  return out;
}

/**
 * `@apitorrent/plugin-sdk`: write apitorrent plugins in TypeScript.
 *
 * A plugin is one ES module whose default export is `definePlugin({ start(api) { … } })`.
 * apitorrent runs it in its own V8 isolate with a small set of web globals (`fetch`, `URL`,
 * timers, `crypto`, …; see `globals.d.ts`) and the `api` object below. Everything the
 * plugin does beyond computing is checked against the capabilities its manifest declares
 * and an admin approved; an undeclared call throws an error named `NotCapable`.
 *
 * Build with `apitorrent-plugin-build` (a single-file ESM bundle plus the manifest) and
 * test with `createTestHost` from `@apitorrent/plugin-sdk/testing`.
 */

import type { AddOutcome } from "./generated/AddOutcome";
import type { AppEvent } from "./generated/AppEvent";
import type { AppInfo } from "./generated/AppInfo";
import type { PackQuery } from "./generated/PackQuery";
import type { PluginCaller } from "./generated/PluginCaller";
import type { SubmitCandidatesOutcome } from "./generated/SubmitCandidatesOutcome";
import type { SeedLimitAction } from "./generated/SeedLimitAction";
import type { TorrentDetail } from "./generated/TorrentDetail";
import type { TorrentSummary } from "./generated/TorrentSummary";
import type { UnregisteredQuery } from "./generated/UnregisteredQuery";

export type { AddOutcome } from "./generated/AddOutcome";
export type { AppEvent } from "./generated/AppEvent";
export type { AppInfo } from "./generated/AppInfo";
export type { PackFile } from "./generated/PackFile";
export type { PackQuery } from "./generated/PackQuery";
export type { PluginCaller } from "./generated/PluginCaller";
export type { PluginCapabilities } from "./generated/PluginCapabilities";
export type { PluginManifest } from "./generated/PluginManifest";
export type { SubmitCandidatesOutcome } from "./generated/SubmitCandidatesOutcome";
export type { PluginRouteAuth } from "./generated/PluginRouteAuth";
export type { QueueSettings } from "./generated/QueueSettings";
export type { SeedLimitAction } from "./generated/SeedLimitAction";
export type { SeedLimits } from "./generated/SeedLimits";
export type { TorrentDetail } from "./generated/TorrentDetail";
export type { TorrentFile } from "./generated/TorrentFile";
export type { TorrentState } from "./generated/TorrentState";
export type { TorrentSummary } from "./generated/TorrentSummary";
export type { TrackerInfo } from "./generated/TrackerInfo";
export type { UnregisteredQuery } from "./generated/UnregisteredQuery";

/** What to add. Give `magnet` or `torrent`. Needs `torrents:add`. */
export interface AddOptions {
  /** A magnet link. */
  magnet?: string;
  /** The bytes of a `.torrent` file. */
  torrent?: Uint8Array | ArrayBuffer;
  /** A folder relative to the download directory; never an absolute path or `..`. */
  savePath?: string;
  label?: string;
  paused?: boolean;
  /** One per file (0 skips it). */
  filePriorities?: number[];
}

/** A torrent's own seed limits. 0 means no limit of that kind. */
export interface SeedLimitsInput {
  ratio?: number;
  seedMinutes?: number;
  /** What happens when they are reached; `pause` by default. */
  action?: SeedLimitAction;
}

export type QueueMove = "top" | "up" | "down" | "bottom";

export interface Torrents {
  /** Needs `torrents:read`. */
  list(): Promise<TorrentSummary[]>;
  /** Needs `torrents:read`. Throws an `ApitorrentError` with code `not_found`. */
  get(id: string): Promise<TorrentDetail>;
  /** Needs `torrents:add`. A torrent that already exists comes back as a duplicate. */
  add(options: AddOptions): Promise<AddOutcome>;
  /** Needs `torrents:control`. */
  pause(id: string): Promise<void>;
  resume(id: string): Promise<void>;
  setLabel(id: string, label: string | null): Promise<void>;
  /** The torrent's own seed limits; `null` follows its group's or the instance's. */
  setSeedLimits(id: string, limits: SeedLimitsInput | null): Promise<void>;
  /** The torrent's own rate limits in bytes per second (0: none of its own). */
  setRateLimits(id: string, limits: { download?: number; upload?: number }): Promise<void>;
  queue(id: string, to: QueueMove): Promise<void>;
  /** Needs `torrents:remove`. */
  remove(id: string, options?: { deleteFiles?: boolean }): Promise<void>;
}

export type EventType = AppEvent["type"] | "*";
export type EventOf<T extends EventType> = T extends "*" ? AppEvent
  : Extract<AppEvent, { type: T }>;

export interface Events {
  /** Needs `events:read`. Returns a function that unsubscribes. */
  on<T extends EventType>(
    type: T,
    handler: (event: EventOf<T>) => void | Promise<void>,
  ): () => void;
}

/** A private store of JSON values. Needs `kv`. */
export interface Kv {
  get<T = unknown>(key: string): Promise<T | undefined>;
  set(key: string, value: unknown): Promise<void>;
  delete(key: string): Promise<boolean>;
  /** Keys starting with `prefix`, sorted. */
  list(prefix?: string, options?: { limit?: number }): Promise<string[]>;
}

export type Interval =
  | number
  | { days?: number; hours?: number; minutes?: number; seconds?: number; milliseconds?: number };

/**
 * Jobs the host runs on an interval. Needs `schedule`. A job doesn't overlap itself: a
 * tick that arrives while the last run is still going is skipped. After a restart the next
 * run is due one interval after the last one.
 */
export interface Schedule {
  every(name: string, interval: Interval, handler: () => unknown): void;
  cancel(name: string): void;
  runNow(name: string): Promise<void>;
}

export interface Settings<S> {
  /** The current settings, with the schema's defaults filled in. */
  get(): S;
  /** Called when an admin changes them; returns a function that unsubscribes. */
  onChange(handler: (settings: S) => void): () => void;
}

export type HttpMethod = "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE";

export interface RouteContext {
  /**
   * The signed-in user who called. Always set on ordinary routes. On a route declared with
   * `"auth": "plugin"` it is `null` unless the request also signed in to apitorrent (a
   * bearer token, a browser session, or a loopback request with authentication off).
   */
  caller: PluginCaller | null;
  /** The path relative to `/api/v1/plugins/<id>` (or to the plugin's own listener). */
  path: string;
}

/** About this apitorrent instance. Needs `torrents:read`. */
export interface App {
  info(): Promise<AppInfo>;
}

export interface LoginResult {
  /** An opaque session id; give it to the client (as a cookie, say). */
  session: string;
  caller: PluginCaller;
}

/**
 * Signing clients in with apitorrent API tokens, for compatibility layers whose clients
 * log in their own way. Needs `tokens:verify` (and the `api-compat` role). The host keeps
 * the sessions: they last an hour unused, end when the plugin restarts, and end at once
 * when the token is revoked or expires.
 */
export interface Auth {
  /** Check a token a client presented. `null` when it isn't a live API token. */
  login(token: string): Promise<LoginResult | null>;
  /** Who a session belongs to, or `null` once it has ended. Using it keeps it alive. */
  session(id: string): Promise<PluginCaller | null>;
  logout(id: string): Promise<boolean>;
}

/**
 * Return a `Response`, or any JSON value (sent as `application/json`), or nothing (204).
 * Responses are served with a sandboxing Content-Security-Policy. Only routes declared with
 * `"auth": "plugin"` may set cookies (host-only, never named like apitorrent's).
 */
export type RouteHandler = (
  request: Request,
  context: RouteContext,
) => unknown | Promise<unknown>;

export interface Http {
  /**
   * Serve a route declared in the manifest's `routes` (same path and method). `/feeds/*`
   * matches `/feeds/` and everything below it.
   */
  route(method: HttpMethod, path: string, handler: RouteHandler): void;
}

/**
 * A release a `source` offers (see the architecture doc, "Automation"). The server's rules
 * match candidates, order them by version and keep the latest as torrents.
 */
export interface ReleaseCandidate {
  /** Unique within this plugin, and stable: the same release always has the same id. */
  id: string;
  title: string;
  /** As the upstream names it (`24.04.1`, `2026.11.08`); rules order by it. */
  version?: string;
  /** ISO 8601 / RFC 3339 (`2026-11-08T12:00:00Z`). */
  published?: string;
  /**
   * Where the data is. A `.torrent` link is fetched with this plugin's own network grants,
   * and only once a rule takes the candidate. A web seed is added to a magnet link.
   */
  links: { torrent?: string; magnet?: string; webseed?: string };
  /** Anything rules may match on: flavour, architecture, size, … */
  metadata?: Record<string, unknown>;
}

/** Hand release candidates to the server's rules. Needs the `source` role. */
export interface Candidates {
  /**
   * Offer candidates (at most 1000 per call). Submitting one again updates it. Candidates
   * the server refuses are listed in `rejected` with the reason; the rest are stored.
   */
  submit(candidates: ReleaseCandidate[]): Promise<SubmitCandidatesOutcome>;
}

export interface SourceRole {
  candidates(limit?: number): ReleaseCandidate[] | Promise<ReleaseCandidate[]>;
}

/** An ACME DNS-01 TXT record. */
export interface TxtRecord {
  /** The name being validated, without `*.` (`example.com` for `*.example.com`). */
  domain: string;
  /** The record's full name: `_acme-challenge.<domain>`, without a trailing dot. */
  name: string;
  value: string;
}

/**
 * Publishes ACME DNS-01 records. apitorrent keeps the ACME account, keys and certificates;
 * the provider only adds and removes records, and the daemon itself waits until the zone's
 * authoritative servers answer with a new record. Several records may share a name (a
 * wildcard and its base domain); `removeTxt` must remove only the one with that value.
 */
export interface DnsProviderRole {
  setTxt(record: TxtRecord): Promise<void>;
  removeTxt(record: TxtRecord): Promise<void>;
}

/** A pack that holds a torrent's files: a magnet link, or a `.torrent` file's bytes. */
export type PackSource = { magnet: string } | { torrent: Uint8Array | ArrayBuffer };

/**
 * Tracker-specific behaviour for the trackers named in `trackers` (host names, or `*.`
 * and a domain for its subdomains). For those trackers `isUnregistered` replaces the
 * built-in "unregistered" patterns, and `findPack` may name the pack that replaced a
 * torrent the tracker deleted; apitorrent adds it, reuses the files, and retires the old
 * torrent. Fetching the pack downloads a whole pack: let users turn that on in settings.
 */
export interface TrackerAdapterRole {
  trackers: string[];
  isUnregistered?(query: UnregisteredQuery): boolean | Promise<boolean>;
  findPack?(query: PackQuery): PackSource | null | Promise<PackSource | null>;
}

export interface RoleImpls {
  source: SourceRole;
  "dns-provider": DnsProviderRole;
  "tracker-adapter": TrackerAdapterRole;
  "api-compat": Record<string, (...args: never[]) => unknown>;
}

export interface Roles {
  /** Fill a role declared in the manifest's `roles`. The host calls these methods. */
  provide<R extends keyof RoleImpls>(role: R, impl: RoleImpls[R]): void;
}

export interface Api<S = Record<string, unknown>> {
  plugin: { readonly id: string; readonly version: string };
  app: App;
  auth: Auth;
  torrents: Torrents;
  candidates: Candidates;
  events: Events;
  kv: Kv;
  schedule: Schedule;
  settings: Settings<S>;
  http: Http;
  roles: Roles;
}

export interface Plugin<S = Record<string, unknown>> {
  /** Register routes, jobs, handlers and roles. The plugin runs once this resolves. */
  start(api: Api<S>): void | Promise<void>;
  /** Called before the plugin stops (disable, upgrade, shutdown); a few seconds at most. */
  stop?(api: Api<S>): void | Promise<void>;
}

/** Declare a plugin. Make it the module's default export. */
export function definePlugin<S = Record<string, unknown>>(plugin: Plugin<S>): Plugin<S> {
  return plugin;
}

/** A failed apitorrent call (`api.torrents.*`), with a stable `code`. */
export interface ApitorrentErrorLike extends Error {
  name: "ApitorrentError";
  code: string;
}

export function isApitorrentError(e: unknown): e is ApitorrentErrorLike {
  return e instanceof Error && e.name === "ApitorrentError";
}

/** Whether `e` is the error for a call the plugin's capabilities don't allow. */
export function isNotCapable(e: unknown): boolean {
  return e instanceof Error && e.name === "NotCapable";
}

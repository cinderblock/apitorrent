/**
 * A test harness for plugins, for `bun test`. It plays the host: it hands the plugin an
 * `api` backed by in-memory fakes, enforces the manifest's capabilities the way apitorrent
 * does (an undeclared call throws `NotCapable`), and swaps `fetch` for your handler while
 * the plugin runs, checking each URL against the network grants.
 *
 * ```ts
 * const host = createTestHost({ manifest, settings: { feeds: [...] }, fetch: (req) => … });
 * await host.start(plugin);
 * await host.runJob("poll");
 * expect(host.added).toHaveLength(1);
 * expect(host.candidates.size).toBe(3);
 * await host.stop();
 * ```
 *
 * Scheduled jobs never run on their own here; call `runJob`.
 */

import type {
  AddOptions,
  AddOutcome,
  Api,
  AppEvent,
  EventType,
  HttpMethod,
  Interval,
  Plugin,
  PluginCaller,
  PluginCapabilities,
  PluginManifest,
  ReleaseCandidate,
  RoleImpls,
  RouteHandler,
  TorrentDetail,
  TorrentSummary,
} from "./index";
import { allows, grantFromSetting, parseGrant, stringsAt } from "./grants";
import { withDefaults } from "./schema";

export interface TestHostOptions<S> {
  manifest: PluginManifest;
  /** Settings as an admin saved them; the schema's defaults are filled in. */
  settings?: Partial<S> | Record<string, unknown>;
  /** What `api.torrents.list()` returns. */
  torrents?: TorrentSummary[];
  /** Answers the plugin's `fetch` calls that its grants allow. */
  fetch?: (request: Request) => Response | Promise<Response>;
  /** Who calls the plugin's routes in `request()`. */
  caller?: PluginCaller;
}

export interface AddedTorrent extends AddOptions {
  id: string;
}

export class NotCapable extends Error {
  override name = "NotCapable";
}

class ApitorrentError extends Error {
  override name = "ApitorrentError";
  constructor(public code: string, message: string) {
    super(message);
  }
}

function infoHashOf(magnet: string): string | null {
  return /xt=urn:btih:([0-9a-fA-F]{40}|[A-Za-z2-7]{32})/.exec(magnet)?.[1]?.toLowerCase() ?? null;
}

async function digestHex(bytes: Uint8Array): Promise<string> {
  const d = await crypto.subtle.digest("SHA-1", bytes as Uint8Array<ArrayBuffer>);
  return Array.from(new Uint8Array(d), (b) => b.toString(16).padStart(2, "0")).join("");
}

function routeMatches(declared: string, path: string): boolean {
  return declared.endsWith("*") ? path.startsWith(declared.slice(0, -1)) : declared === path;
}

export function createTestHost<S = Record<string, unknown>>(options: TestHostOptions<S>) {
  const manifest = options.manifest;
  // Manifests on disk leave out what defaults to empty, as the host allows.
  const declared: Partial<PluginCapabilities> = manifest.capabilities ?? {};
  const caps: PluginCapabilities = {
    network: declared.network ?? [],
    networkFromSettings: declared.networkFromSettings ?? [],
    api: declared.api ?? [],
    routes: declared.routes ?? [],
    kv: declared.kv ?? false,
    schedule: declared.schedule ?? false,
    roles: declared.roles ?? [],
  };
  let settings = withDefaults(manifest.settings, options.settings ?? {}) as S;
  const settingsListeners = new Set<(s: S) => void>();
  const kv = new Map<string, unknown>();
  const added: AddedTorrent[] = [];
  const controls: { id: string; action: string; label?: string | null }[] = [];
  const removed: { id: string; deleteFiles: boolean }[] = [];
  const fetches: Request[] = [];
  const candidates = new Map<string, ReleaseCandidate>();
  const jobs = new Map<string, { handler: () => unknown; interval: Interval }>();
  const routes: { method: string; path: string; handler: RouteHandler }[] = [];
  const roles = new Map<string, unknown>();
  const handlers = new Map<string, Set<(e: AppEvent) => unknown>>();
  let plugin: Plugin<S> | null = null;
  let realFetch: typeof globalThis.fetch | null = null;

  const need = (ok: boolean, what: string) => {
    if (!ok) throw new NotCapable(`${what} isn't declared in the plugin's manifest`);
  };
  const scope = (s: string) => need((caps.api as string[]).includes(s), `the \`${s}\` capability`);

  const grants = () => [
    ...caps.network.map(parseGrant),
    ...caps.networkFromSettings.flatMap((p) => stringsAt(settings, p)).map(grantFromSetting),
  ].filter((g) => g !== null);

  const mockFetch = async (input: string | URL | Request, init?: RequestInit) => {
    const request = new Request(input, init);
    if (!grants().some((g) => allows(g, request.url))) {
      throw new NotCapable(
        `fetch to ${new URL(request.url).origin} is not allowed: the plugin has no network grant for it`,
      );
    }
    fetches.push(request.clone());
    if (!options.fetch) throw new TypeError(`fetch failed: no fetch handler for ${request.url}`);
    return await options.fetch(request);
  };

  const api: Api<S> = {
    plugin: { id: manifest.id, version: manifest.version },
    torrents: {
      async list() {
        scope("torrents:read");
        return structuredClone(options.torrents ?? []);
      },
      async get(id: string): Promise<TorrentDetail> {
        scope("torrents:read");
        throw new ApitorrentError("not_found", `no torrent ${id}`);
      },
      async add(o: AddOptions): Promise<AddOutcome> {
        scope("torrents:add");
        if (o.savePath !== undefined && (/^([/\\]|[A-Za-z]:)/.test(o.savePath) || /(^|[/\\])\.\.([/\\]|$)/.test(o.savePath))) {
          throw new NotCapable(`savePath ${o.savePath} must be relative to the download directory`);
        }
        let id: string | null;
        if (typeof o.magnet === "string") id = infoHashOf(o.magnet);
        else if (o.torrent) id = await digestHex(new Uint8Array(o.torrent as ArrayBuffer));
        else throw new TypeError("add needs `magnet` or `torrent`");
        if (!id) throw new ApitorrentError("invalid_request", "not a magnet link apitorrent can read");
        if (added.some((a) => a.id === id)) return { id: null, duplicate: true };
        added.push({ ...o, id });
        return { id, duplicate: false };
      },
      async pause(id: string) {
        scope("torrents:control");
        controls.push({ id, action: "pause" });
      },
      async resume(id: string) {
        scope("torrents:control");
        controls.push({ id, action: "resume" });
      },
      async setLabel(id: string, label: string | null) {
        scope("torrents:control");
        controls.push({ id, action: "setLabel", label });
      },
      async remove(id: string, o?: { deleteFiles?: boolean }) {
        scope("torrents:remove");
        removed.push({ id, deleteFiles: Boolean(o?.deleteFiles) });
      },
    },
    candidates: {
      async submit(list: ReleaseCandidate[]) {
        need((caps.roles as string[]).includes("source"), "the `source` role");
        if (!Array.isArray(list)) throw new TypeError("submit() needs an array of candidates");
        if (list.length > 1000) {
          throw new ApitorrentError("invalid_request", "at most 1000 candidates at a time");
        }
        const rejected: string[] = [];
        let accepted = 0;
        for (const c of list) {
          // The checks the server makes that a plugin can get wrong.
          const links = c?.links ?? {};
          const bad = !c?.id || !c.title ? "`id` and `title` are required"
            : [links.torrent, links.webseed].some((l) => l !== undefined && !/^https?:\/\//.test(l))
            ? "links must be http(s) URLs"
            : links.magnet !== undefined && !infoHashOf(links.magnet) && !/xt=urn:btmh:/.test(links.magnet)
            ? "`links.magnet` isn't a magnet link"
            : null;
          if (bad) {
            rejected.push(`${c?.id}: ${bad}`);
            continue;
          }
          candidates.set(c.id, structuredClone(c));
          accepted++;
        }
        return { accepted, rejected };
      },
    },
    events: {
      on(type: EventType, handler: (e: never) => unknown) {
        scope("events:read");
        const set = handlers.get(type) ?? new Set();
        set.add(handler as (e: AppEvent) => unknown);
        handlers.set(type, set);
        return () => set.delete(handler as (e: AppEvent) => unknown);
      },
    } as Api<S>["events"],
    kv: {
      async get<T>(key: string) {
        need(caps.kv, "storage (`kv`)");
        return structuredClone(kv.get(key)) as T | undefined;
      },
      async set(key: string, value: unknown) {
        need(caps.kv, "storage (`kv`)");
        if (JSON.stringify(value) === undefined) {
          throw new TypeError("kv values must be JSON-serialisable");
        }
        kv.set(key, JSON.parse(JSON.stringify(value)));
      },
      async delete(key: string) {
        need(caps.kv, "storage (`kv`)");
        return kv.delete(key);
      },
      async list(prefix = "", o?: { limit?: number }) {
        need(caps.kv, "storage (`kv`)");
        return [...kv.keys()].filter((k) => k.startsWith(prefix)).sort().slice(0, o?.limit ?? 1000);
      },
    },
    schedule: {
      every(name: string, interval: Interval, handler: () => unknown) {
        need(caps.schedule, "scheduling (`schedule`)");
        jobs.set(name, { handler, interval });
      },
      cancel(name: string) {
        jobs.delete(name);
      },
      async runNow(name: string) {
        await host.runJob(name);
      },
    },
    settings: {
      get: () => structuredClone(settings),
      onChange(handler: (s: S) => void) {
        settingsListeners.add(handler);
        return () => settingsListeners.delete(handler);
      },
    },
    http: {
      route(method: HttpMethod, path: string, handler: RouteHandler) {
        need(
          caps.routes.some((r) => r.path === path && r.methods.includes(method)),
          `route ${method} ${path}`,
        );
        routes.push({ method, path, handler });
      },
    },
    roles: {
      provide<R extends keyof RoleImpls>(role: R, impl: RoleImpls[R]) {
        need((caps.roles as string[]).includes(role), `the \`${role}\` role`);
        roles.set(role, impl);
      },
    },
  };

  const host = {
    api,
    /** Torrents the plugin added, in order. */
    added,
    controls,
    removed,
    /** Every request the plugin's `fetch` made (and was allowed to make). */
    fetches,
    /** Release candidates the plugin submitted, by id (the latest submission of each). */
    candidates,
    /** The plugin's key-value data. */
    kv,
    jobs,

    async start(p: Plugin<S>) {
      plugin = p;
      realFetch = globalThis.fetch;
      globalThis.fetch = mockFetch as typeof globalThis.fetch;
      await p.start(api);
    },

    async stop() {
      try {
        await plugin?.stop?.(api);
      } finally {
        if (realFetch) globalThis.fetch = realFetch;
        realFetch = null;
      }
    },

    async runJob(name: string) {
      const job = jobs.get(name);
      if (!job) throw new Error(`no scheduled job "${name}" (have: ${[...jobs.keys()].join(", ")})`);
      await job.handler();
    },

    /** Call one of the plugin's routes, as a signed-in user would. */
    async request(method: HttpMethod, path: string, init: RequestInit = {}): Promise<Response> {
      if (!caps.routes.some((r) => routeMatches(r.path, path) && r.methods.includes(method))) {
        return Response.json({ code: "no_route" }, { status: 404 });
      }
      const route = routes.find((r) => r.method === method && routeMatches(r.path, path));
      if (!route) return Response.json({ code: "no_route" }, { status: 404 });
      const req = new Request(`http://localhost:8742/api/v1/plugins/${manifest.id}${path}`, {
        ...init,
        method,
      });
      const caller = options.caller ??
        { userId: "u1", userName: "admin", role: "admin" } as PluginCaller;
      const r = await route.handler(req, { caller, path });
      if (r instanceof Response) return r;
      return r === undefined ? new Response(null, { status: 204 }) : Response.json(r);
    },

    /** Deliver an event to the plugin's handlers. */
    async emit(event: AppEvent) {
      for (const type of [event.type, "*"]) {
        for (const h of handlers.get(type) ?? []) await h(structuredClone(event));
      }
    },

    /** Call a method of a role the plugin provides, as the host would. */
    async callRole(role: keyof RoleImpls, method: string, ...args: unknown[]) {
      const impl = roles.get(role) as Record<string, (...a: unknown[]) => unknown> | undefined;
      const fn = impl?.[method];
      if (typeof fn !== "function") throw new Error(`the plugin doesn't provide ${role}.${method}()`);
      return await fn.apply(impl, args);
    },

    /** Change the settings, as an admin would; the plugin's onChange handlers run. */
    setSettings(values: Partial<S> | Record<string, unknown>) {
      settings = withDefaults(manifest.settings, values) as S;
      for (const h of settingsListeners) h(structuredClone(settings));
    },
  };
  return host;
}

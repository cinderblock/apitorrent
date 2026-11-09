// The `apitorrent` API object passed to a plugin's start(api), and the loop that delivers
// the host's messages (events, scheduled runs, HTTP requests, role calls, settings).
//
// The grants are checked in Rust by each op; these wrappers only shape arguments and
// results.

import {
  op_at_candidates_submit,
  op_at_events,
  op_at_info,
  op_at_kv_delete,
  op_at_kv_get,
  op_at_kv_list,
  op_at_kv_set,
  op_at_log,
  op_at_next,
  op_at_reply_error,
  op_at_reply_http,
  op_at_reply_role,
  op_at_role,
  op_at_route,
  op_at_schedule,
  op_at_settings,
  op_at_started,
  op_at_stopped,
  op_at_torrent_add,
  op_at_torrent_control,
  op_at_torrent_get,
  op_at_torrent_remove,
  op_at_torrents_list,
  op_at_unschedule,
} from "ext:core/ops";
import {
  bytesToBase64,
  callSafely,
  LEVEL,
  reportError,
  viewBytes,
} from "ext:apitorrent_plugin/00_util.js";
import { structuredClone, TextEncoder } from "ext:apitorrent_plugin/01_web.js";
import { Headers, Request, Response } from "ext:apitorrent_plugin/02_fetch.js";

/** A failed apitorrent call, with a stable code (such as `not_found`). */
export class ApitorrentError extends Error {
  constructor(code, message) {
    super(message);
    this.name = "ApitorrentError";
    this.code = code;
  }
}

function unwrap(json) {
  const r = JSON.parse(json);
  if (r.ok) return r.value;
  throw new ApitorrentError(r.code, r.message);
}

const info = JSON.parse(op_at_info());
let settings = JSON.parse(op_at_settings());
const settingsListeners = new Set();

const EVENT_TYPES = new Set([
  "torrent.added",
  "torrent.removed",
  "torrent.stateChanged",
  "torrent.finished",
  "*",
]);
const eventHandlers = new Map();

const jobs = new Map();
const routes = [];
const roles = new Map();

function intervalMs(v) {
  if (typeof v === "number" && Number.isFinite(v)) return Math.round(v);
  if (v && typeof v === "object") {
    return Math.round(
      (v.days ?? 0) * 86_400_000 + (v.hours ?? 0) * 3_600_000 + (v.minutes ?? 0) * 60_000 +
        (v.seconds ?? 0) * 1000 + (v.milliseconds ?? 0),
    );
  }
  throw new TypeError("an interval is milliseconds, or { days, hours, minutes, seconds }");
}

function routeMatches(declared, path) {
  return declared.endsWith("*") ? path.startsWith(declared.slice(0, -1)) : declared === path;
}

async function control(id, action) {
  unwrap(await op_at_torrent_control(String(id), JSON.stringify(action)));
}

/**
 * Run a scheduled job. A tick that arrives while the last run is still going is skipped;
 * `runNow` (`wait`) waits for that run instead of starting a second one.
 */
async function runJob(name, wait = false) {
  const job = jobs.get(name);
  if (!job) return;
  if (job.running) {
    if (wait) return await job.running;
    op_at_log(LEVEL.debug, `skipped scheduled job "${name}": the last run is still going`);
    return;
  }
  job.running = (async () => {
    try {
      await job.handler();
    } catch (e) {
      reportError(e, `scheduled job "${name}"`);
    }
  })();
  try {
    await job.running;
  } finally {
    job.running = null;
  }
}

export const api = Object.freeze({
  plugin: Object.freeze({ id: info.id, version: info.version }),

  torrents: Object.freeze({
    list: async () => JSON.parse(op_at_torrents_list()),
    get: async (id) => unwrap(await op_at_torrent_get(String(id))),
    async add(options) {
      if (!options || typeof options !== "object") {
        throw new TypeError("add({ magnet }) or add({ torrent: bytes })");
      }
      let source;
      if (typeof options.magnet === "string") {
        source = { kind: "magnet", uri: options.magnet };
      } else if (options.torrent !== undefined) {
        source = { kind: "torrentFile", data: bytesToBase64(viewBytes(options.torrent, "torrent")) };
      } else {
        throw new TypeError("add needs `magnet` (a magnet link) or `torrent` (.torrent bytes)");
      }
      const req = {
        source,
        savePath: options.savePath ?? null,
        label: options.label ?? null,
        paused: Boolean(options.paused),
        filePriorities: options.filePriorities ?? null,
      };
      return unwrap(await op_at_torrent_add(JSON.stringify(req)));
    },
    pause: (id) => control(id, { action: "pause" }),
    resume: (id) => control(id, { action: "resume" }),
    setLabel: (id, label) => control(id, { action: "setLabel", label: label ?? null }),
    async remove(id, { deleteFiles = false } = {}) {
      unwrap(await op_at_torrent_remove(String(id), Boolean(deleteFiles)));
    },
  }),

  candidates: Object.freeze({
    /**
     * Offer release candidates to the server's rules (the `source` role). Resolves to
     * { accepted, rejected }: rejected lists each refused candidate with the reason.
     */
    async submit(list) {
      if (!Array.isArray(list)) throw new TypeError("submit() needs an array of candidates");
      return unwrap(await op_at_candidates_submit(JSON.stringify(list)));
    },
  }),

  events: Object.freeze({
    on(type, handler) {
      if (!EVENT_TYPES.has(type)) {
        throw new TypeError(`unknown event "${type}"; one of ${[...EVENT_TYPES].join(", ")}`);
      }
      if (typeof handler !== "function") throw new TypeError("on() needs a handler function");
      op_at_events();
      const set = eventHandlers.get(type) ?? new Set();
      set.add(handler);
      eventHandlers.set(type, set);
      return () => set.delete(handler);
    },
  }),

  kv: Object.freeze({
    async get(key) {
      const v = op_at_kv_get(String(key));
      return v === "" ? undefined : JSON.parse(v);
    },
    async set(key, value) {
      const json = JSON.stringify(value);
      if (json === undefined) throw new TypeError("kv values must be JSON-serialisable");
      op_at_kv_set(String(key), json);
    },
    async delete(key) {
      return op_at_kv_delete(String(key));
    },
    async list(prefix = "", { limit = 1000 } = {}) {
      return op_at_kv_list(String(prefix), limit);
    },
  }),

  schedule: Object.freeze({
    every(name, interval, handler) {
      if (typeof handler !== "function") throw new TypeError("every() needs a handler function");
      op_at_schedule(String(name), intervalMs(interval));
      jobs.set(String(name), { handler, running: null });
    },
    cancel(name) {
      op_at_unschedule(String(name));
      jobs.delete(String(name));
    },
    async runNow(name) {
      if (!jobs.has(String(name))) throw new TypeError(`no scheduled job "${name}"`);
      await runJob(String(name), true);
    },
  }),

  settings: Object.freeze({
    get: () => structuredClone(settings),
    onChange(handler) {
      if (typeof handler !== "function") throw new TypeError("onChange() needs a function");
      settingsListeners.add(handler);
      return () => settingsListeners.delete(handler);
    },
  }),

  http: Object.freeze({
    route(method, path, handler) {
      if (typeof handler !== "function") throw new TypeError("route() needs a handler function");
      method = String(method).toUpperCase();
      op_at_route(method, String(path));
      routes.push({ method, path: String(path), handler });
    },
  }),

  roles: Object.freeze({
    provide(role, impl) {
      if (!impl || typeof impl !== "object") throw new TypeError("provide() needs an object");
      op_at_role(String(role));
      roles.set(String(role), impl);
    },
  }),
});

async function handleHttp(msg, body) {
  const route = routes.find((r) => r.method === msg.method && routeMatches(r.path, msg.path));
  if (!route) {
    op_at_reply_http(
      msg.id,
      JSON.stringify({ status: 404, headers: [["content-type", "application/json"]] }),
      new TextEncoder().encode(
        JSON.stringify({ code: "no_route", message: "the plugin has no handler for this route" }),
      ),
    );
    return;
  }
  try {
    const hasBody = body && body.length && msg.method !== "GET" && msg.method !== "HEAD";
    const request = new Request(msg.url, {
      method: msg.method,
      headers: msg.headers,
      body: hasBody ? body : undefined,
    });
    let res = await route.handler(request, { caller: msg.caller, path: msg.path });
    if (!(res instanceof Response)) {
      res = res === undefined ? new Response(null, { status: 204 }) : Response.json(res);
    }
    const bytes = await res.bytes();
    op_at_reply_http(
      msg.id,
      JSON.stringify({ status: res.status, headers: Headers.raw(res.headers) }),
      bytes,
    );
  } catch (e) {
    reportError(e, `${msg.method} ${msg.path}`);
    op_at_reply_error(msg.id, String(e?.message ?? e));
  }
}

async function handleRole(msg) {
  const impl = roles.get(msg.role);
  const fn = impl?.[msg.method];
  if (typeof fn !== "function") {
    op_at_reply_error(msg.id, `the plugin doesn't provide ${msg.role}.${msg.method}()`);
    return;
  }
  try {
    const args = Array.isArray(msg.args) ? msg.args : msg.args == null ? [] : [msg.args];
    const result = await fn.apply(impl, args);
    op_at_reply_role(msg.id, JSON.stringify(result ?? null));
  } catch (e) {
    reportError(e, `${msg.role}.${msg.method}()`);
    op_at_reply_error(msg.id, String(e?.message ?? e));
  }
}

function dispatch(msg, body) {
  switch (msg.kind) {
    case "event": {
      for (const type of [msg.event.type, "*"]) {
        for (const h of eventHandlers.get(type) ?? []) {
          callSafely(h, [structuredClone(msg.event)], `"${msg.event.type}" handler`);
        }
      }
      break;
    }
    case "schedule":
      runJob(msg.name);
      break;
    case "http":
      handleHttp(msg, body);
      break;
    case "role":
      handleRole(msg);
      break;
    case "settings":
      settings = msg.values;
      for (const h of settingsListeners) callSafely(h, [structuredClone(settings)], "onChange");
      break;
  }
}

let plugin = null;

async function pump() {
  for (;;) {
    const next = await op_at_next();
    if (next === null) return;
    const msg = JSON.parse(next.json);
    if (msg.kind === "stop") {
      try {
        await plugin?.stop?.(api);
      } catch (e) {
        reportError(e, "stop()");
      }
      op_at_stopped();
      return;
    }
    dispatch(msg, next.body);
  }
}

/** Called by the host with the plugin module's namespace. */
export async function start(namespace) {
  plugin = namespace.default;
  if (!plugin || typeof plugin !== "object" || typeof plugin.start !== "function") {
    throw new TypeError(
      "the plugin's default export must be definePlugin({ start(api) { ... } })",
    );
  }
  await plugin.start(api);
  op_at_started();
  pump().catch((e) => reportError(e, "the host message loop"));
}

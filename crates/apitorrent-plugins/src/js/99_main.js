// Bootstrap: install the curated globals, remove everything else the engine and deno_core
// put on globalThis, and hand the host the function that starts the plugin.

import { op_at_register } from "ext:core/ops";
import { core, reportError } from "ext:apitorrent_plugin/00_util.js";
import {
  AbortController,
  AbortSignal,
  atob,
  btoa,
  crypto,
  CryptoKey,
  DOMException,
  Event,
  EventTarget,
  queueMicrotask,
  structuredClone,
  TextDecoder,
  TextEncoder,
  timers,
  URL,
  URLSearchParams,
} from "ext:apitorrent_plugin/01_web.js";
import { fetch, Headers, Request, Response } from "ext:apitorrent_plugin/02_fetch.js";
import { console } from "ext:apitorrent_plugin/03_console.js";
import { start } from "ext:apitorrent_plugin/04_api.js";

const globals = {
  AbortController,
  AbortSignal,
  atob,
  btoa,
  clearInterval: timers.clearInterval,
  clearTimeout: timers.clearTimeout,
  console,
  crypto,
  CryptoKey,
  DOMException,
  Event,
  EventTarget,
  fetch,
  Headers,
  queueMicrotask,
  reportError: (e) => reportError(e),
  Request,
  Response,
  setInterval: timers.setInterval,
  setTimeout: timers.setTimeout,
  structuredClone,
  TextDecoder,
  TextEncoder,
  URL,
  URLSearchParams,
};

for (const [name, value] of Object.entries(globals)) {
  Object.defineProperty(globalThis, name, {
    value,
    writable: true,
    configurable: true,
    enumerable: false,
  });
}
globalThis.self = globalThis;

// What plugins must not reach: deno_core's internals (ops, resources, eval helpers) and
// engine features that escape the memory budget (WebAssembly memories) or exist only for
// threads plugins don't have.
for (
  const name of [
    "Deno",
    "__bootstrap",
    "__infra",
    "__timers",
    "WebAssembly",
    "SharedArrayBuffer",
    "Atomics",
  ]
) {
  delete globalThis[name];
}

// A promise nobody handled is logged, not fatal: one forgotten `catch` shouldn't stop a
// plugin.
core.setUnhandledPromiseRejectionHandler((_promise, reason) => {
  reportError(reason, "unhandled promise rejection");
  return true;
});

op_at_register(start);

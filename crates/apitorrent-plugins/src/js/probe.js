import { core } from "ext:core/mod.js";
import { op_probe_add } from "ext:core/ops";
globalThis.probeAdd = (a, b) => op_probe_add(a, b);
globalThis.setTimeout = (cb, ms) => core.createTimer(cb, ms, undefined, false, true);
delete globalThis.Deno;
delete globalThis.__bootstrap;

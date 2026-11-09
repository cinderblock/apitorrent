// console, writing to the plugin's log (not stdout).

import { op_at_log } from "ext:core/ops";
import { format, inspect, LEVEL } from "ext:apitorrent_plugin/00_util.js";

const counts = new Map();
const started = new Map();
let indent = "";

function write(level, args) {
  const text = format(args);
  op_at_log(level, indent ? text.replace(/^/gm, indent) : text);
}

export const console = {
  log: (...args) => write(LEVEL.info, args),
  info: (...args) => write(LEVEL.info, args),
  debug: (...args) => write(LEVEL.debug, args),
  warn: (...args) => write(LEVEL.warn, args),
  error: (...args) => write(LEVEL.error, args),
  trace(...args) {
    const stack = (new Error().stack ?? "").split("\n").slice(2).join("\n");
    write(LEVEL.error, [...args, `\n${stack}`]);
  },
  assert(condition, ...args) {
    if (!condition) write(LEVEL.error, ["Assertion failed" + (args.length ? ":" : ""), ...args]);
  },
  dir: (value) => write(LEVEL.info, [inspect(value)]),
  table: (value) => write(LEVEL.info, [inspect(value)]),
  count(label = "default") {
    const n = (counts.get(label) ?? 0) + 1;
    counts.set(label, n);
    write(LEVEL.info, [`${label}: ${n}`]);
  },
  countReset(label = "default") {
    counts.delete(label);
  },
  time(label = "default") {
    started.set(label, Date.now());
  },
  timeLog(label = "default", ...args) {
    const t = started.get(label);
    if (t !== undefined) write(LEVEL.info, [`${label}: ${Date.now() - t} ms`, ...args]);
  },
  timeEnd(label = "default") {
    console.timeLog(label);
    started.delete(label);
  },
  group(...args) {
    if (args.length) write(LEVEL.info, args);
    indent += "  ";
  },
  groupCollapsed(...args) {
    console.group(...args);
  },
  groupEnd() {
    indent = indent.slice(2);
  },
};

// Shared helpers for the plugin runtime's JavaScript. Nothing here is a global.

import { core } from "ext:core/mod.js";
import { op_at_log } from "ext:core/ops";

export { core };

export const LEVEL = { debug: 0, info: 1, warn: 2, error: 3 };

/** A Uint8Array view of any BufferSource; throws TypeError for anything else. */
export function viewBytes(data, what = "data") {
  if (data instanceof Uint8Array) return data;
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  if (ArrayBuffer.isView(data)) {
    return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  }
  throw new TypeError(`${what} must be an ArrayBuffer or a typed array`);
}

/** A copy, so later changes by the caller don't show through. */
export function copyBytes(data, what) {
  return viewBytes(data, what).slice();
}

const B64 =
  "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64_INDEX = new Map([...B64].map((c, i) => [c, i]));

export function bytesToBase64(bytes) {
  let out = "";
  let i = 0;
  for (; i + 2 < bytes.length; i += 3) {
    const n = (bytes[i] << 16) | (bytes[i + 1] << 8) | bytes[i + 2];
    out += B64[n >> 18] + B64[(n >> 12) & 63] + B64[(n >> 6) & 63] +
      B64[n & 63];
  }
  if (i < bytes.length) {
    const n = (bytes[i] << 16) | ((bytes[i + 1] ?? 0) << 8);
    out += B64[n >> 18] + B64[(n >> 12) & 63];
    out += i + 1 < bytes.length ? B64[(n >> 6) & 63] + "=" : "==";
  }
  return out;
}

/** Forgiving base64 decode (WHATWG); returns null for invalid input. */
export function base64ToBytes(text) {
  let s = String(text).replace(/[\t\n\f\r ]/g, "");
  if (s.length % 4 === 0) s = s.replace(/={1,2}$/, "");
  if (s.length % 4 === 1 || /[^A-Za-z0-9+/]/.test(s)) return null;
  const out = new Uint8Array(Math.floor((s.length * 3) / 4));
  let o = 0;
  let buf = 0;
  let bits = 0;
  for (const c of s) {
    buf = (buf << 6) | B64_INDEX.get(c);
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      out[o++] = (buf >> bits) & 0xff;
    }
  }
  return out.subarray(0, o);
}

/** Format values the way `console.log` does: strings as is, everything else inspected. */
export function format(args) {
  if (args.length === 0) return "";
  let first = args[0];
  let rest = args.slice(1);
  let out;
  if (typeof first === "string" && /%[sdifoOjc%]/.test(first)) {
    out = first.replace(/%([sdifoOjc%])/g, (m, f) => {
      if (f === "%") return "%";
      if (rest.length === 0) return m;
      const v = rest.shift();
      switch (f) {
        case "s":
          return typeof v === "string" ? v : inspect(v);
        case "d":
        case "i":
          return typeof v === "bigint" ? `${v}n` : String(Math.trunc(Number(v)));
        case "f":
          return String(Number(v));
        case "c":
          return "";
        default:
          return inspect(v);
      }
    });
  } else {
    out = typeof first === "string" ? first : inspect(first);
  }
  for (const v of rest) out += " " + (typeof v === "string" ? v : inspect(v));
  return out;
}

const MAX_DEPTH = 4;
const MAX_ITEMS = 100;

/** A readable, bounded rendering of any value (cycles, depth and length limited). */
export function inspect(value, depth = 0, seen = new Set()) {
  switch (typeof value) {
    case "string":
      return depth === 0 ? value : JSON.stringify(value);
    case "number":
      return Object.is(value, -0) ? "-0" : String(value);
    case "bigint":
      return `${value}n`;
    case "undefined":
      return "undefined";
    case "boolean":
      return String(value);
    case "symbol":
      return value.toString();
    case "function":
      return `[Function: ${value.name || "(anonymous)"}]`;
  }
  if (value === null) return "null";
  if (value instanceof Error) {
    const stack = typeof value.stack === "string" ? value.stack : "";
    const head = `${value.name}: ${value.message}`;
    let text = stack.startsWith(value.name) || stack.includes(value.message)
      ? stack
      : stack
      ? `${head}\n${stack}`
      : head;
    if (value.cause !== undefined && depth < MAX_DEPTH) {
      text += `\nCaused by: ${inspect(value.cause, depth + 1, seen)}`;
    }
    return text;
  }
  if (seen.has(value)) return "[Circular]";
  if (depth >= MAX_DEPTH) return Array.isArray(value) ? "[Array]" : "[Object]";
  seen.add(value);
  try {
    if (value instanceof Date) return value.toISOString();
    if (value instanceof RegExp) return String(value);
    if (ArrayBuffer.isView(value) && !(value instanceof DataView)) {
      const items = Array.from(value.subarray(0, MAX_ITEMS), String);
      const more = value.length > MAX_ITEMS ? `, ... ${value.length - MAX_ITEMS} more` : "";
      return `${value.constructor.name}(${value.length}) [${items.join(", ")}${more}]`;
    }
    if (value instanceof Map) {
      const items = [...value].slice(0, MAX_ITEMS).map(([k, v]) =>
        `${inspect(k, depth + 1, seen)} => ${inspect(v, depth + 1, seen)}`
      );
      return `Map(${value.size}) {${items.length ? " " + items.join(", ") + " " : ""}}`;
    }
    if (value instanceof Set) {
      const items = [...value].slice(0, MAX_ITEMS).map((v) =>
        inspect(v, depth + 1, seen)
      );
      return `Set(${value.size}) {${items.length ? " " + items.join(", ") + " " : ""}}`;
    }
    if (Array.isArray(value)) {
      const items = value.slice(0, MAX_ITEMS).map((v) => inspect(v, depth + 1, seen));
      if (value.length > MAX_ITEMS) items.push(`... ${value.length - MAX_ITEMS} more`);
      return `[${items.join(", ")}]`;
    }
    if (typeof value.then === "function") return "Promise { ... }";
    const keys = Object.keys(value);
    const name = value.constructor && value.constructor !== Object
      ? `${value.constructor.name} `
      : "";
    const items = keys.slice(0, MAX_ITEMS).map((k) =>
      `${/^[A-Za-z_$][\w$]*$/.test(k) ? k : JSON.stringify(k)}: ${
        inspect(value[k], depth + 1, seen)
      }`
    );
    if (keys.length > MAX_ITEMS) items.push(`... ${keys.length - MAX_ITEMS} more`);
    return `${name}{${items.length ? " " + items.join(", ") + " " : ""}}`;
  } catch {
    return "[Uninspectable]";
  } finally {
    seen.delete(value);
  }
}

/** Log an error that nothing caught, without ending the plugin. */
export function reportError(e, where) {
  op_at_log(LEVEL.error, `${where ? `${where}: ` : ""}${inspect(e)}`);
}

/** Call `fn`, reporting (not throwing) a synchronous error or a rejected promise. */
export function callSafely(fn, args, where) {
  try {
    const r = fn(...args);
    if (r && typeof r.then === "function") {
      r.then(undefined, (e) => reportError(e, where));
    }
  } catch (e) {
    reportError(e, where);
  }
}

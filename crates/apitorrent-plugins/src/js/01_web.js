// Web platform globals: DOMException, events, abort signals, text encoding, URL,
// base64, timers, structuredClone and crypto. Parsing and decoding go to Rust (the `url`
// and `encoding_rs` crates), so they follow the WHATWG specs.

import {
  op_at_digest,
  op_at_encoding_name,
  op_at_hmac,
  op_at_hmac_verify,
  op_at_random,
  op_at_text_decode,
  op_at_url_parse,
  op_at_url_set,
  op_at_uuid,
} from "ext:core/ops";
import {
  base64ToBytes,
  bytesToBase64,
  callSafely,
  copyBytes,
  core,
  viewBytes,
} from "ext:apitorrent_plugin/00_util.js";

// --- DOMException -------------------------------------------------------------------

const DOM_CODES = {
  IndexSizeError: 1,
  NotFoundError: 8,
  NotSupportedError: 9,
  InvalidStateError: 11,
  SyntaxError: 12,
  InvalidAccessError: 15,
  TypeMismatchError: 17,
  SecurityError: 18,
  NetworkError: 19,
  AbortError: 20,
  QuotaExceededError: 22,
  TimeoutError: 23,
  DataCloneError: 25,
  InvalidCharacterError: 5,
};

export class DOMException extends Error {
  #name;
  constructor(message = "", name = "Error") {
    super(String(message));
    this.#name = String(name);
  }
  get name() {
    return this.#name;
  }
  get code() {
    return DOM_CODES[this.#name] ?? 0;
  }
}
for (const name of ["AbortError", "NotSupportedError", "QuotaExceededError", "TimeoutError"]) {
  core.registerErrorBuilder(`DOMException${name}`, (msg) => new DOMException(msg, name));
}

// --- Events -------------------------------------------------------------------------

export class Event {
  #type;
  #cancelable;
  #defaultPrevented = false;
  #stopped = false;
  #timeStamp = Date.now();
  target = null;
  currentTarget = null;
  constructor(type, init = {}) {
    if (arguments.length === 0) throw new TypeError("Event needs a type");
    this.#type = String(type);
    this.#cancelable = Boolean(init.cancelable);
  }
  get type() {
    return this.#type;
  }
  get cancelable() {
    return this.#cancelable;
  }
  get defaultPrevented() {
    return this.#defaultPrevented;
  }
  get timeStamp() {
    return this.#timeStamp;
  }
  get bubbles() {
    return false;
  }
  preventDefault() {
    if (this.#cancelable) this.#defaultPrevented = true;
  }
  stopImmediatePropagation() {
    this.#stopped = true;
  }
  stopPropagation() {}
  static isStopped(e) {
    return e.#stopped;
  }
}

export class EventTarget {
  #listeners = new Map();

  addEventListener(type, callback, options) {
    if (callback == null) return;
    const once = typeof options === "object" && options !== null && Boolean(options.once);
    const signal = typeof options === "object" && options !== null ? options.signal : undefined;
    if (signal?.aborted) return;
    type = String(type);
    const list = this.#listeners.get(type) ?? [];
    if (list.some((l) => l.callback === callback)) return;
    const entry = { callback, once };
    list.push(entry);
    this.#listeners.set(type, list);
    signal?.addEventListener("abort", () => this.removeEventListener(type, callback));
  }

  removeEventListener(type, callback) {
    const list = this.#listeners.get(String(type));
    if (!list) return;
    const i = list.findIndex((l) => l.callback === callback);
    if (i >= 0) list.splice(i, 1);
  }

  dispatchEvent(event) {
    if (!(event instanceof Event)) throw new TypeError("dispatchEvent needs an Event");
    event.target = this;
    event.currentTarget = this;
    const handler = this[`on${event.type}`];
    const list = [...(this.#listeners.get(event.type) ?? [])];
    if (typeof handler === "function") list.unshift({ callback: handler, once: false });
    for (const l of list) {
      if (l.once) this.removeEventListener(event.type, l.callback);
      const fn = typeof l.callback === "function"
        ? l.callback
        : l.callback?.handleEvent?.bind(l.callback);
      if (fn) callSafely(fn.bind(this), [event], `"${event.type}" listener`);
      if (Event.isStopped(event)) break;
    }
    event.currentTarget = null;
    return !event.defaultPrevented;
  }
}

// --- Abort ----------------------------------------------------------------------------

const PRIVATE = Symbol("internal");

export class AbortSignal extends EventTarget {
  #aborted = false;
  #reason = undefined;
  onabort = null;

  constructor(key) {
    if (key !== PRIVATE) throw new TypeError("use AbortController to make an AbortSignal");
    super();
  }
  get aborted() {
    return this.#aborted;
  }
  get reason() {
    return this.#reason;
  }
  throwIfAborted() {
    if (this.#aborted) throw this.#reason;
  }
  static abort(reason) {
    const s = new AbortSignal(PRIVATE);
    s.#aborted = true;
    s.#reason = reason ?? new DOMException("This operation was aborted", "AbortError");
    return s;
  }
  static timeout(ms) {
    const s = new AbortSignal(PRIVATE);
    timers.setTimeout(() => {
      AbortSignal.signalAbort(
        s,
        new DOMException("The operation timed out", "TimeoutError"),
      );
    }, ms);
    return s;
  }
  static any(signals) {
    const s = new AbortSignal(PRIVATE);
    for (const other of signals) {
      if (other.aborted) {
        AbortSignal.signalAbort(s, other.reason);
        return s;
      }
    }
    for (const other of signals) {
      other.addEventListener("abort", () => AbortSignal.signalAbort(s, other.reason), {
        once: true,
      });
    }
    return s;
  }
  static signalAbort(s, reason) {
    if (s.#aborted) return;
    s.#aborted = true;
    s.#reason = reason ?? new DOMException("This operation was aborted", "AbortError");
    s.dispatchEvent(new Event("abort"));
  }
}

export class AbortController {
  #signal = new AbortSignal(PRIVATE);
  get signal() {
    return this.#signal;
  }
  abort(reason) {
    AbortSignal.signalAbort(this.#signal, reason);
  }
}

// --- Text encoding --------------------------------------------------------------------

export class TextEncoder {
  get encoding() {
    return "utf-8";
  }
  encode(input = "") {
    return core.encode(String(input));
  }
  encodeInto(source, destination) {
    if (!(destination instanceof Uint8Array)) {
      throw new TypeError("encodeInto needs a Uint8Array");
    }
    let read = 0;
    let written = 0;
    for (const ch of String(source)) {
      const cp = ch.codePointAt(0);
      const bytes = core.encode(ch);
      if (written + bytes.length > destination.length) break;
      destination.set(bytes, written);
      written += bytes.length;
      read += cp > 0xffff ? 2 : 1;
    }
    return { read, written };
  }
}

export class TextDecoder {
  #encoding;
  #fatal;
  #ignoreBOM;
  #pending = [];
  constructor(label = "utf-8", options = {}) {
    const name = op_at_encoding_name(String(label));
    if (!name) throw new RangeError(`The encoding "${label}" is not supported`);
    this.#encoding = name;
    this.#fatal = Boolean(options.fatal);
    this.#ignoreBOM = Boolean(options.ignoreBOM);
  }
  get encoding() {
    return this.#encoding;
  }
  get fatal() {
    return this.#fatal;
  }
  get ignoreBOM() {
    return this.#ignoreBOM;
  }
  // `stream: true` holds chunks back and decodes them with the final call, so a character
  // split across chunks still decodes; the concatenated output is the same.
  decode(input, options = {}) {
    if (input !== undefined) this.#pending.push(copyBytes(input, "input"));
    if (options.stream) return "";
    const total = this.#pending.reduce((n, b) => n + b.length, 0);
    const all = new Uint8Array(total);
    let o = 0;
    for (const b of this.#pending) {
      all.set(b, o);
      o += b.length;
    }
    this.#pending = [];
    return op_at_text_decode(this.#encoding, all, this.#fatal, this.#ignoreBOM);
  }
}

// --- URL and URLSearchParams ----------------------------------------------------------

function parseUrl(href, base) {
  const json = op_at_url_parse(href, base ?? "");
  return json ? JSON.parse(json) : null;
}

const UTF8 = new TextDecoder();

function percentDecode(text) {
  const bytes = core.encode(text);
  const out = new Uint8Array(bytes.length);
  let o = 0;
  const hex = (b) =>
    b >= 48 && b <= 57 ? b - 48 : b >= 65 && b <= 70 ? b - 55 : b >= 97 && b <= 102 ? b - 87 : -1;
  for (let i = 0; i < bytes.length; i++) {
    if (bytes[i] === 37 && i + 2 < bytes.length) {
      const h = hex(bytes[i + 1]);
      const l = hex(bytes[i + 2]);
      if (h >= 0 && l >= 0) {
        out[o++] = (h << 4) | l;
        i += 2;
        continue;
      }
    }
    out[o++] = bytes[i];
  }
  return UTF8.decode(out.subarray(0, o));
}

function formEncode(text) {
  let out = "";
  for (const b of core.encode(String(text))) {
    if (
      (b >= 0x30 && b <= 0x39) || (b >= 0x41 && b <= 0x5a) || (b >= 0x61 && b <= 0x7a) ||
      b === 0x2a || b === 0x2d || b === 0x2e || b === 0x5f
    ) {
      out += String.fromCharCode(b);
    } else if (b === 0x20) {
      out += "+";
    } else {
      out += "%" + b.toString(16).toUpperCase().padStart(2, "0");
    }
  }
  return out;
}

function parseForm(text) {
  const list = [];
  for (const part of text.split("&")) {
    if (part === "") continue;
    const eq = part.indexOf("=");
    const name = eq < 0 ? part : part.slice(0, eq);
    const value = eq < 0 ? "" : part.slice(eq + 1);
    list.push([
      percentDecode(name.replaceAll("+", " ")),
      percentDecode(value.replaceAll("+", " ")),
    ]);
  }
  return list;
}

export class URLSearchParams {
  #list = [];
  #url = null;

  constructor(init = "", url = undefined) {
    if (url !== undefined && init === PRIVATE) {
      this.#url = url;
      return;
    }
    if (init instanceof URLSearchParams) {
      this.#list = init.#list.map(([k, v]) => [k, v]);
    } else if (typeof init === "object" && init !== null) {
      if (typeof init[Symbol.iterator] === "function") {
        for (const pair of init) {
          const p = [...pair];
          if (p.length !== 2) throw new TypeError("each pair needs exactly two items");
          this.#list.push([String(p[0]), String(p[1])]);
        }
      } else {
        for (const k of Object.keys(init)) this.#list.push([k, String(init[k])]);
      }
    } else {
      const s = String(init);
      this.#list = parseForm(s.startsWith("?") ? s.slice(1) : s);
    }
  }

  static linked(url, search) {
    const p = new URLSearchParams(PRIVATE, url);
    p.#list = parseForm(search.startsWith("?") ? search.slice(1) : search);
    return p;
  }
  static reparse(p, search) {
    p.#list = parseForm(search.startsWith("?") ? search.slice(1) : search);
  }

  #update() {
    if (this.#url) URL.setSearchFromParams(this.#url, this.toString());
  }

  get size() {
    return this.#list.length;
  }
  append(name, value) {
    this.#list.push([String(name), String(value)]);
    this.#update();
  }
  delete(name, value) {
    name = String(name);
    this.#list = this.#list.filter(([k, v]) =>
      k !== name || (value !== undefined && v !== String(value))
    );
    this.#update();
  }
  get(name) {
    return this.#list.find(([k]) => k === String(name))?.[1] ?? null;
  }
  getAll(name) {
    return this.#list.filter(([k]) => k === String(name)).map(([, v]) => v);
  }
  has(name, value) {
    return this.#list.some(([k, v]) =>
      k === String(name) && (value === undefined || v === String(value))
    );
  }
  set(name, value) {
    name = String(name);
    const i = this.#list.findIndex(([k]) => k === name);
    if (i < 0) {
      this.#list.push([name, String(value)]);
    } else {
      this.#list[i][1] = String(value);
      this.#list = this.#list.filter(([k], j) => j <= i || k !== name);
    }
    this.#update();
  }
  sort() {
    // Stable, by UTF-16 code units, as the spec says.
    this.#list = this.#list
      .map((e, i) => [e, i])
      .sort(([a, i], [b, j]) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : i - j))
      .map(([e]) => e);
    this.#update();
  }
  forEach(fn, thisArg) {
    for (const [k, v] of this.#list) fn.call(thisArg, v, k, this);
  }
  *keys() {
    for (const [k] of this.#list) yield k;
  }
  *values() {
    for (const [, v] of this.#list) yield v;
  }
  *entries() {
    for (const [k, v] of this.#list) yield [k, v];
  }
  [Symbol.iterator]() {
    return this.entries();
  }
  toString() {
    return this.#list.map(([k, v]) => `${formEncode(k)}=${formEncode(v)}`).join("&");
  }
}

export class URL {
  #p;
  #params = null;

  constructor(url, base = undefined) {
    const p = parseUrl(String(url), base === undefined ? undefined : String(base));
    if (!p) throw new TypeError(`Invalid URL: ${url}`);
    this.#p = p;
  }
  static canParse(url, base = undefined) {
    return parseUrl(String(url), base === undefined ? undefined : String(base)) !== null;
  }
  static parse(url, base = undefined) {
    try {
      return new URL(url, base);
    } catch {
      return null;
    }
  }
  static setSearchFromParams(u, search) {
    u.#p = JSON.parse(op_at_url_set(u.#p.href, "search", search));
  }

  #set(part, value) {
    this.#p = JSON.parse(op_at_url_set(this.#p.href, part, String(value)));
    if (this.#params && (part === "search" || part === "href")) {
      URLSearchParams.reparse(this.#params, this.#p.search);
    }
  }

  get href() {
    return this.#p.href;
  }
  set href(v) {
    this.#set("href", v);
  }
  get origin() {
    return this.#p.origin;
  }
  get protocol() {
    return this.#p.protocol;
  }
  set protocol(v) {
    this.#set("protocol", v);
  }
  get username() {
    return this.#p.username;
  }
  set username(v) {
    this.#set("username", v);
  }
  get password() {
    return this.#p.password;
  }
  set password(v) {
    this.#set("password", v);
  }
  get host() {
    return this.#p.host;
  }
  set host(v) {
    this.#set("host", v);
  }
  get hostname() {
    return this.#p.hostname;
  }
  set hostname(v) {
    this.#set("hostname", v);
  }
  get port() {
    return this.#p.port;
  }
  set port(v) {
    this.#set("port", v);
  }
  get pathname() {
    return this.#p.pathname;
  }
  set pathname(v) {
    this.#set("pathname", v);
  }
  get search() {
    return this.#p.search;
  }
  set search(v) {
    this.#set("search", v);
  }
  get hash() {
    return this.#p.hash;
  }
  set hash(v) {
    this.#set("hash", v);
  }
  get searchParams() {
    this.#params ??= URLSearchParams.linked(this, this.#p.search);
    return this.#params;
  }
  toString() {
    return this.#p.href;
  }
  toJSON() {
    return this.#p.href;
  }
}

// --- base64 ---------------------------------------------------------------------------

export function btoa(data) {
  const s = String(data);
  const bytes = new Uint8Array(s.length);
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    if (c > 0xff) {
      throw new DOMException(
        "btoa takes Latin-1 text only; encode other text to bytes first",
        "InvalidCharacterError",
      );
    }
    bytes[i] = c;
  }
  return bytesToBase64(bytes);
}

export function atob(data) {
  const bytes = base64ToBytes(data);
  if (bytes === null) {
    throw new DOMException("The string is not valid base64", "InvalidCharacterError");
  }
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return s;
}

// --- Timers and cloning ---------------------------------------------------------------

const active = new Map();
let nextTimer = 1;

function schedule(callback, delay, args, repeat) {
  if (typeof callback !== "function") {
    throw new TypeError("timers need a function (strings are not evaluated)");
  }
  const id = nextTimer++;
  const timer = core.createTimer(
    () => {
      if (!repeat) active.delete(id);
      callSafely(callback, args, "timer callback");
    },
    Number(delay) || 0,
    undefined,
    repeat,
    true,
  );
  active.set(id, timer);
  return id;
}

export const timers = {
  setTimeout: (callback, delay = 0, ...args) => schedule(callback, delay, args, false),
  setInterval: (callback, delay = 0, ...args) => schedule(callback, delay, args, true),
  clearTimeout(id) {
    const t = active.get(id);
    if (t) {
      core.cancelTimer(t);
      active.delete(id);
    }
  },
  clearInterval(id) {
    timers.clearTimeout(id);
  },
};

const v8QueueMicrotask = globalThis.queueMicrotask;
export function queueMicrotask(callback) {
  if (typeof callback !== "function") throw new TypeError("queueMicrotask needs a function");
  v8QueueMicrotask(() => callSafely(callback, [], "microtask"));
}

export function structuredClone(value, _options = undefined) {
  try {
    return core.structuredClone(value);
  } catch (e) {
    throw new DOMException(e?.message ?? String(e), "DataCloneError");
  }
}

// --- crypto ---------------------------------------------------------------------------

const INTEGER_ARRAYS = [
  Int8Array,
  Uint8Array,
  Uint8ClampedArray,
  Int16Array,
  Uint16Array,
  Int32Array,
  Uint32Array,
  BigInt64Array,
  BigUint64Array,
];

const HASHES = ["SHA-1", "SHA-256", "SHA-384", "SHA-512"];

function hashName(alg) {
  const name = String(typeof alg === "object" && alg !== null ? alg.name : alg).toUpperCase();
  if (!HASHES.includes(name)) {
    throw new DOMException(`Unsupported hash: ${name}`, "NotSupportedError");
  }
  return name;
}

const keyBytes = new WeakMap();

export class CryptoKey {
  #algorithm;
  #extractable;
  #usages;
  constructor(key, algorithm, extractable, usages) {
    if (key !== PRIVATE) throw new TypeError("use crypto.subtle.importKey");
    this.#algorithm = algorithm;
    this.#extractable = extractable;
    this.#usages = usages;
  }
  get type() {
    return "secret";
  }
  get algorithm() {
    return structuredClone(this.#algorithm);
  }
  get extractable() {
    return this.#extractable;
  }
  get usages() {
    return [...this.#usages];
  }
}

function hmacKey(key, usage) {
  if (!(key instanceof CryptoKey) || key.algorithm.name !== "HMAC") {
    throw new DOMException("The key is not an HMAC key", "InvalidAccessError");
  }
  if (!key.usages.includes(usage)) {
    throw new DOMException(`The key can't be used to ${usage}`, "InvalidAccessError");
  }
  return keyBytes.get(key);
}

function algorithmName(alg) {
  return String(typeof alg === "object" && alg !== null ? alg.name : alg).toUpperCase();
}

const subtle = {
  async digest(algorithm, data) {
    const out = op_at_digest(hashName(algorithm), viewBytes(data));
    return out.buffer.slice(out.byteOffset, out.byteOffset + out.byteLength);
  },
  async importKey(format, keyData, algorithm, extractable, usages) {
    if (format !== "raw") {
      throw new DOMException("Only raw keys are supported", "NotSupportedError");
    }
    if (algorithmName(algorithm) !== "HMAC") {
      throw new DOMException("Only HMAC keys are supported", "NotSupportedError");
    }
    const hash = hashName(algorithm.hash);
    const bytes = copyBytes(keyData, "keyData");
    if (bytes.length === 0) throw new DOMException("Empty key", "DataError");
    for (const u of usages) {
      if (u !== "sign" && u !== "verify") {
        throw new SyntaxError(`HMAC keys can't be used to ${u}`);
      }
    }
    const key = new CryptoKey(
      PRIVATE,
      { name: "HMAC", hash: { name: hash }, length: bytes.length * 8 },
      Boolean(extractable),
      [...usages],
    );
    keyBytes.set(key, bytes);
    return key;
  },
  async exportKey(format, key) {
    if (format !== "raw") {
      throw new DOMException("Only raw keys are supported", "NotSupportedError");
    }
    if (!key.extractable) throw new DOMException("The key isn't extractable", "InvalidAccessError");
    return keyBytes.get(key).slice().buffer;
  },
  async sign(algorithm, key, data) {
    if (algorithmName(algorithm) !== "HMAC") {
      throw new DOMException("Only HMAC is supported", "NotSupportedError");
    }
    const raw = hmacKey(key, "sign");
    const out = op_at_hmac(key.algorithm.hash.name, raw, viewBytes(data));
    return out.buffer.slice(out.byteOffset, out.byteOffset + out.byteLength);
  },
  async verify(algorithm, key, signature, data) {
    if (algorithmName(algorithm) !== "HMAC") {
      throw new DOMException("Only HMAC is supported", "NotSupportedError");
    }
    const raw = hmacKey(key, "verify");
    return op_at_hmac_verify(key.algorithm.hash.name, raw, viewBytes(signature), viewBytes(data));
  },
};

export const crypto = {
  getRandomValues(array) {
    if (!INTEGER_ARRAYS.some((T) => array instanceof T)) {
      throw new DOMException(
        "getRandomValues needs an integer typed array",
        "TypeMismatchError",
      );
    }
    if (array.byteLength > 65536) {
      throw new DOMException(
        "getRandomValues fills at most 65536 bytes at a time",
        "QuotaExceededError",
      );
    }
    op_at_random(new Uint8Array(array.buffer, array.byteOffset, array.byteLength));
    return array;
  },
  randomUUID() {
    return op_at_uuid();
  },
  subtle,
};

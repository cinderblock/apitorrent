// Headers, Request, Response and fetch. Bodies are whole byte arrays (no streams): the
// host caps their size, so buffering is bounded.

import { op_at_fetch, op_at_fetch_abort } from "ext:core/ops";
import { copyBytes, core } from "ext:apitorrent_plugin/00_util.js";
import {
  AbortController,
  DOMException,
  TextDecoder,
  URL,
  URLSearchParams,
} from "ext:apitorrent_plugin/01_web.js";

const TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;

function normalizeValue(value) {
  const v = String(value).replace(/^[\t\n\r ]+|[\t\n\r ]+$/g, "");
  if (/[\0\r\n]/.test(v)) throw new TypeError(`invalid header value ${JSON.stringify(v)}`);
  return v;
}

function checkName(name) {
  const n = String(name);
  if (!TOKEN.test(n)) throw new TypeError(`invalid header name ${JSON.stringify(n)}`);
  return n.toLowerCase();
}

export class Headers {
  #list = [];

  constructor(init = undefined) {
    if (init == null) return;
    if (init instanceof Headers) {
      this.#list = init.#list.map(([k, v]) => [k, v]);
    } else if (typeof init[Symbol.iterator] === "function") {
      for (const pair of init) {
        const p = [...pair];
        if (p.length !== 2) throw new TypeError("each header needs a name and a value");
        this.append(p[0], p[1]);
      }
    } else if (typeof init === "object") {
      for (const k of Object.keys(init)) this.append(k, init[k]);
    } else {
      throw new TypeError("headers must be a Headers, a list of pairs or an object");
    }
  }

  append(name, value) {
    this.#list.push([checkName(name), normalizeValue(value)]);
  }
  delete(name) {
    const n = checkName(name);
    this.#list = this.#list.filter(([k]) => k !== n);
  }
  get(name) {
    const n = checkName(name);
    const values = this.#list.filter(([k]) => k === n).map(([, v]) => v);
    return values.length ? values.join(", ") : null;
  }
  getSetCookie() {
    return this.#list.filter(([k]) => k === "set-cookie").map(([, v]) => v);
  }
  has(name) {
    const n = checkName(name);
    return this.#list.some(([k]) => k === n);
  }
  set(name, value) {
    const n = checkName(name);
    const v = normalizeValue(value);
    const i = this.#list.findIndex(([k]) => k === n);
    if (i < 0) {
      this.#list.push([n, v]);
    } else {
      this.#list[i][1] = v;
      this.#list = this.#list.filter(([k], j) => j <= i || k !== n);
    }
  }
  /** Sorted by name, values combined, except that each set-cookie stays separate. */
  #sorted() {
    const names = [...new Set(this.#list.map(([k]) => k))].sort();
    const out = [];
    for (const n of names) {
      if (n === "set-cookie") {
        for (const v of this.getSetCookie()) out.push([n, v]);
      } else {
        out.push([n, this.get(n)]);
      }
    }
    return out;
  }
  forEach(fn, thisArg) {
    for (const [k, v] of this.#sorted()) fn.call(thisArg, v, k, this);
  }
  *keys() {
    for (const [k] of this.#sorted()) yield k;
  }
  *values() {
    for (const [, v] of this.#sorted()) yield v;
  }
  *entries() {
    yield* this.#sorted();
  }
  [Symbol.iterator]() {
    return this.entries();
  }
  /** Every header line in order, for the host. */
  static raw(h) {
    return h.#list.map(([k, v]) => [k, v]);
  }
}

/** A body and its default content type. */
function extractBody(body) {
  if (body == null) return { bytes: null, type: null };
  if (typeof body === "string") {
    return { bytes: core.encode(body), type: "text/plain;charset=UTF-8" };
  }
  if (body instanceof URLSearchParams) {
    return {
      bytes: core.encode(body.toString()),
      type: "application/x-www-form-urlencoded;charset=UTF-8",
    };
  }
  if (body instanceof ArrayBuffer || ArrayBuffer.isView(body)) {
    return { bytes: copyBytes(body), type: null };
  }
  return { bytes: core.encode(String(body)), type: "text/plain;charset=UTF-8" };
}

class Body {
  #bytes;
  #used = false;

  constructor(bytes) {
    this.#bytes = bytes;
  }
  get bodyUsed() {
    return this.#used;
  }
  #take() {
    if (this.#used) throw new TypeError("the body has already been read");
    this.#used = true;
    return this.#bytes ?? new Uint8Array(0);
  }
  async bytes() {
    return this.#take().slice();
  }
  async arrayBuffer() {
    return this.#take().slice().buffer;
  }
  async text() {
    return new TextDecoder().decode(this.#take());
  }
  async json() {
    return JSON.parse(await this.text());
  }
  /** The unread body, without reading it. */
  static peek(b) {
    return b.#used ? null : b.#bytes;
  }
  /** Read the body for sending it on (null when there is none). */
  static take(b) {
    return b.#bytes === null ? null : b.#take();
  }
  static replace(b, bytes) {
    b.#bytes = bytes;
    b.#used = false;
  }
}

const NULL_BODY_STATUS = [101, 103, 204, 205, 304];
const METHODS = ["DELETE", "GET", "HEAD", "OPTIONS", "POST", "PUT", "PATCH"];

function normalizeMethod(m) {
  const s = String(m);
  if (!TOKEN.test(s)) throw new TypeError(`invalid method ${JSON.stringify(s)}`);
  const upper = s.toUpperCase();
  if (["CONNECT", "TRACE", "TRACK"].includes(upper)) {
    throw new TypeError(`method ${upper} is not allowed`);
  }
  return METHODS.includes(upper) ? upper : s;
}

export class Request extends Body {
  #method;
  #url;
  #headers;
  #signal;
  #redirect;

  constructor(input, init = {}) {
    const base = input instanceof Request ? input : null;
    let url;
    if (base) {
      url = base.url;
    } else {
      const parsed = URL.parse(String(input));
      if (!parsed) {
        throw new TypeError(
          `invalid URL ${JSON.stringify(String(input))}: plugins need absolute URLs`,
        );
      }
      if (parsed.username || parsed.password) {
        throw new TypeError("URLs with credentials aren't allowed; use an Authorization header");
      }
      url = parsed.href;
    }
    const method = normalizeMethod(init.method ?? base?.method ?? "GET");
    const headers = new Headers(init.headers ?? base?.headers);
    let bytes = null;
    if (init.body !== undefined) {
      const extracted = extractBody(init.body);
      bytes = extracted.bytes;
      if (extracted.type && !headers.has("content-type")) {
        headers.set("content-type", extracted.type);
      }
    } else if (base) {
      bytes = Body.take(base);
    }
    if (bytes !== null && (method === "GET" || method === "HEAD")) {
      throw new TypeError(`a ${method} request can't have a body`);
    }
    super(bytes);
    this.#method = method;
    this.#url = url;
    this.#headers = headers;
    this.#signal = init.signal ?? base?.signal ?? new AbortController().signal;
    this.#redirect = init.redirect ?? base?.redirect ?? "follow";
    if (!["follow", "error", "manual"].includes(this.#redirect)) {
      throw new TypeError(`invalid redirect mode ${this.#redirect}`);
    }
  }
  get method() {
    return this.#method;
  }
  get url() {
    return this.#url;
  }
  get headers() {
    return this.#headers;
  }
  get signal() {
    return this.#signal;
  }
  get redirect() {
    return this.#redirect;
  }
  clone() {
    if (this.bodyUsed) throw new TypeError("can't clone a request whose body was read");
    const body = Body.peek(this);
    return new Request(this.#url, {
      method: this.#method,
      headers: this.#headers,
      body: body === null ? undefined : body,
      signal: this.#signal,
      redirect: this.#redirect,
    });
  }
}

export class Response extends Body {
  #status;
  #statusText;
  #headers;
  #url = "";
  #redirected = false;
  #type = "default";

  constructor(body = null, init = {}) {
    const status = init.status ?? 200;
    if (!Number.isInteger(status) || status < 200 || status > 599) {
      throw new RangeError(`status ${status} is not between 200 and 599`);
    }
    const headers = new Headers(init.headers);
    const { bytes, type } = extractBody(body);
    if (bytes !== null && NULL_BODY_STATUS.includes(status)) {
      throw new TypeError(`a ${status} response can't have a body`);
    }
    if (type && !headers.has("content-type")) headers.set("content-type", type);
    super(bytes);
    this.#status = status;
    this.#statusText = String(init.statusText ?? "");
    this.#headers = headers;
  }
  get status() {
    return this.#status;
  }
  get statusText() {
    return this.#statusText;
  }
  get ok() {
    return this.#status >= 200 && this.#status <= 299;
  }
  get headers() {
    return this.#headers;
  }
  get url() {
    return this.#url;
  }
  get redirected() {
    return this.#redirected;
  }
  get type() {
    return this.#type;
  }
  clone() {
    if (this.bodyUsed) throw new TypeError("can't clone a response whose body was read");
    return Response.fromHost(
      {
        status: this.#status,
        statusText: this.#statusText,
        headers: Headers.raw(this.#headers),
        url: this.#url,
        redirected: this.#redirected,
      },
      Body.peek(this),
    );
  }
  static json(data, init = {}) {
    const text = JSON.stringify(data);
    if (text === undefined) throw new TypeError("the value can't be turned into JSON");
    const headers = new Headers(init.headers);
    if (!headers.has("content-type")) headers.set("content-type", "application/json");
    return new Response(text, { ...init, headers });
  }
  static redirect(url, status = 302) {
    if (![301, 302, 303, 307, 308].includes(status)) {
      throw new RangeError(`${status} is not a redirect status`);
    }
    const r = new Response(null, { status });
    r.#headers.set("location", new URL(String(url)).href);
    return r;
  }
  static error() {
    const r = Response.fromHost({ status: 0, statusText: "", headers: [], url: "" }, null);
    r.#type = "error";
    return r;
  }
  /** A response as the host delivered it (any status). */
  static fromHost(meta, body) {
    const r = new Response(null);
    r.#status = meta.status;
    r.#statusText = meta.statusText ?? "";
    r.#headers = new Headers(meta.headers);
    r.#url = meta.url ?? "";
    r.#redirected = Boolean(meta.redirected);
    r.#type = "basic";
    Body.replace(r, body);
    return r;
  }
}

let nextAbortId = 1;

export async function fetch(input, init = undefined) {
  const request = new Request(input, init);
  const signal = request.signal;
  if (signal.aborted) throw signal.reason;
  const body = Body.take(request);
  const abortId = nextAbortId++;
  const args = JSON.stringify({
    url: request.url,
    method: request.method,
    headers: Headers.raw(request.headers),
    redirect: request.redirect,
  });
  const pending = op_at_fetch(args, body !== null, body ?? new Uint8Array(0), abortId);
  return await new Promise((resolve, reject) => {
    const onAbort = () => {
      op_at_fetch_abort(abortId);
      reject(signal.reason ?? new DOMException("The request was aborted", "AbortError"));
    };
    signal.addEventListener("abort", onAbort, { once: true });
    pending.then(
      (r) => {
        signal.removeEventListener("abort", onAbort);
        if (signal.aborted) return;
        resolve(Response.fromHost(JSON.parse(r.meta), r.body));
      },
      (e) => {
        signal.removeEventListener("abort", onAbort);
        if (!signal.aborted) reject(e);
      },
    );
  });
}


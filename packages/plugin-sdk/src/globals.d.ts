// The globals an apitorrent plugin has, and no others: no DOM, no Node, no Bun, no Deno,
// no WebAssembly, no SharedArrayBuffer. Plugin sources type-check against this file (see
// tsconfig.plugin.json); the standard ECMAScript library comes from `lib: ["ES2023"]`.
//
// Differences from browsers: bodies are whole byte arrays (no streams, no Blob or
// FormData); `fetch` is limited by the plugin's network grants, follows redirects only to
// granted hosts, never downgrades from HTTPS, and refuses bad certificates; `console`
// writes to the plugin's log.

type BufferSource = ArrayBuffer | ArrayBufferView;
type HeadersInit = Headers | [string, string][] | Record<string, string>;
type BodyInit = string | BufferSource | URLSearchParams;

declare class DOMException extends Error {
  constructor(message?: string, name?: string);
  readonly name: string;
  readonly code: number;
}

declare class Event {
  constructor(type: string, init?: { cancelable?: boolean });
  readonly type: string;
  readonly cancelable: boolean;
  readonly defaultPrevented: boolean;
  readonly timeStamp: number;
  target: EventTarget | null;
  currentTarget: EventTarget | null;
  preventDefault(): void;
  stopImmediatePropagation(): void;
  stopPropagation(): void;
}

type EventListener = ((event: Event) => void) | { handleEvent(event: Event): void };

declare class EventTarget {
  addEventListener(
    type: string,
    listener: EventListener | null,
    options?: boolean | { once?: boolean; signal?: AbortSignal },
  ): void;
  removeEventListener(type: string, listener: EventListener | null): void;
  dispatchEvent(event: Event): boolean;
}

declare class AbortSignal extends EventTarget {
  private constructor();
  readonly aborted: boolean;
  readonly reason: unknown;
  onabort: ((event: Event) => void) | null;
  throwIfAborted(): void;
  static abort(reason?: unknown): AbortSignal;
  static timeout(milliseconds: number): AbortSignal;
  static any(signals: Iterable<AbortSignal>): AbortSignal;
}

declare class AbortController {
  readonly signal: AbortSignal;
  abort(reason?: unknown): void;
}

declare class TextEncoder {
  readonly encoding: "utf-8";
  encode(input?: string): Uint8Array;
  encodeInto(source: string, destination: Uint8Array): { read: number; written: number };
}

declare class TextDecoder {
  /** Any WHATWG encoding label: `utf-8`, `windows-1252`, `iso-8859-1`, `shift_jis`, … */
  constructor(label?: string, options?: { fatal?: boolean; ignoreBOM?: boolean });
  readonly encoding: string;
  readonly fatal: boolean;
  readonly ignoreBOM: boolean;
  decode(input?: BufferSource, options?: { stream?: boolean }): string;
}

declare class URLSearchParams implements Iterable<[string, string]> {
  constructor(init?: string | Record<string, string> | Iterable<[string, string]> | URLSearchParams);
  readonly size: number;
  append(name: string, value: string): void;
  delete(name: string, value?: string): void;
  get(name: string): string | null;
  getAll(name: string): string[];
  has(name: string, value?: string): boolean;
  set(name: string, value: string): void;
  sort(): void;
  forEach(callback: (value: string, key: string, parent: URLSearchParams) => void): void;
  keys(): IterableIterator<string>;
  values(): IterableIterator<string>;
  entries(): IterableIterator<[string, string]>;
  [Symbol.iterator](): IterableIterator<[string, string]>;
  toString(): string;
}

declare class URL {
  constructor(url: string | URL, base?: string | URL);
  static canParse(url: string | URL, base?: string | URL): boolean;
  static parse(url: string | URL, base?: string | URL): URL | null;
  href: string;
  readonly origin: string;
  protocol: string;
  username: string;
  password: string;
  host: string;
  hostname: string;
  port: string;
  pathname: string;
  search: string;
  hash: string;
  readonly searchParams: URLSearchParams;
  toString(): string;
  toJSON(): string;
}

declare class Headers implements Iterable<[string, string]> {
  constructor(init?: HeadersInit);
  append(name: string, value: string): void;
  delete(name: string): void;
  get(name: string): string | null;
  getSetCookie(): string[];
  has(name: string): boolean;
  set(name: string, value: string): void;
  forEach(callback: (value: string, key: string, parent: Headers) => void): void;
  keys(): IterableIterator<string>;
  values(): IterableIterator<string>;
  entries(): IterableIterator<[string, string]>;
  [Symbol.iterator](): IterableIterator<[string, string]>;
}

interface Body {
  readonly bodyUsed: boolean;
  arrayBuffer(): Promise<ArrayBuffer>;
  bytes(): Promise<Uint8Array>;
  json(): Promise<any>;
  text(): Promise<string>;
}

interface RequestInit {
  method?: string;
  headers?: HeadersInit;
  body?: BodyInit | null;
  signal?: AbortSignal;
  redirect?: "follow" | "error" | "manual";
}

declare class Request implements Body {
  constructor(input: string | URL | Request, init?: RequestInit);
  readonly method: string;
  readonly url: string;
  readonly headers: Headers;
  readonly signal: AbortSignal;
  readonly redirect: "follow" | "error" | "manual";
  readonly bodyUsed: boolean;
  arrayBuffer(): Promise<ArrayBuffer>;
  bytes(): Promise<Uint8Array>;
  json(): Promise<any>;
  text(): Promise<string>;
  clone(): Request;
}

interface ResponseInit {
  status?: number;
  statusText?: string;
  headers?: HeadersInit;
}

declare class Response implements Body {
  constructor(body?: BodyInit | null, init?: ResponseInit);
  readonly status: number;
  readonly statusText: string;
  readonly ok: boolean;
  readonly headers: Headers;
  readonly url: string;
  readonly redirected: boolean;
  readonly type: "basic" | "default" | "error";
  readonly bodyUsed: boolean;
  arrayBuffer(): Promise<ArrayBuffer>;
  bytes(): Promise<Uint8Array>;
  json(): Promise<any>;
  text(): Promise<string>;
  clone(): Response;
  static json(data: unknown, init?: ResponseInit): Response;
  static redirect(url: string | URL, status?: number): Response;
  static error(): Response;
}

/**
 * Only URLs the plugin's network grants allow. A denied URL throws `NotCapable`; network
 * failures, bad certificates and responses over the size limit throw `TypeError`.
 */
declare function fetch(input: string | URL | Request, init?: RequestInit): Promise<Response>;

declare class CryptoKey {
  private constructor();
  readonly type: "secret";
  readonly extractable: boolean;
  readonly algorithm: { name: "HMAC"; hash: { name: string }; length: number };
  readonly usages: ("sign" | "verify")[];
}

type HashName = "SHA-1" | "SHA-256" | "SHA-384" | "SHA-512";

declare const crypto: {
  getRandomValues<T extends Exclude<ArrayBufferView, Float32Array | Float64Array | DataView>>(array: T): T;
  randomUUID(): string;
  subtle: {
    digest(algorithm: HashName | { name: HashName }, data: BufferSource): Promise<ArrayBuffer>;
    importKey(
      format: "raw",
      keyData: BufferSource,
      algorithm: { name: "HMAC"; hash: HashName | { name: HashName } },
      extractable: boolean,
      usages: ("sign" | "verify")[],
    ): Promise<CryptoKey>;
    exportKey(format: "raw", key: CryptoKey): Promise<ArrayBuffer>;
    sign(algorithm: "HMAC" | { name: "HMAC" }, key: CryptoKey, data: BufferSource): Promise<ArrayBuffer>;
    verify(
      algorithm: "HMAC" | { name: "HMAC" },
      key: CryptoKey,
      signature: BufferSource,
      data: BufferSource,
    ): Promise<boolean>;
  };
};

declare const console: {
  log(...data: unknown[]): void;
  info(...data: unknown[]): void;
  debug(...data: unknown[]): void;
  warn(...data: unknown[]): void;
  error(...data: unknown[]): void;
  trace(...data: unknown[]): void;
  assert(condition?: boolean, ...data: unknown[]): void;
  dir(value: unknown): void;
  table(value: unknown): void;
  count(label?: string): void;
  countReset(label?: string): void;
  time(label?: string): void;
  timeLog(label?: string, ...data: unknown[]): void;
  timeEnd(label?: string): void;
  group(...data: unknown[]): void;
  groupCollapsed(...data: unknown[]): void;
  groupEnd(): void;
};

declare function setTimeout<A extends unknown[]>(callback: (...args: A) => void, ms?: number, ...args: A): number;
declare function setInterval<A extends unknown[]>(callback: (...args: A) => void, ms?: number, ...args: A): number;
declare function clearTimeout(id: number | undefined): void;
declare function clearInterval(id: number | undefined): void;
declare function queueMicrotask(callback: () => void): void;
declare function structuredClone<T>(value: T): T;
declare function reportError(error: unknown): void;
declare function atob(data: string): string;
declare function btoa(data: string): string;
declare var self: typeof globalThis;

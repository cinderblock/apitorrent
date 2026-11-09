/**
 * Request parameters as qBittorrent reads them: the query string, then the body, which is
 * `application/x-www-form-urlencoded` or `multipart/form-data`. Files (the `torrents`
 * parts of an add) come back as bytes.
 */

export interface FilePart {
  name: string;
  filename: string;
  data: Uint8Array;
}

export interface Params {
  /** The last value of each field. */
  get(name: string): string | undefined;
  /** Every value of a field, in order. */
  all(name: string): string[];
  files(name: string): FilePart[];
}

class ParamSet implements Params {
  readonly fields: [string, string][] = [];
  readonly parts: FilePart[] = [];

  get(name: string) {
    for (let i = this.fields.length - 1; i >= 0; i--) {
      if (this.fields[i]![0] === name) return this.fields[i]![1];
    }
    return undefined;
  }

  all(name: string) {
    return this.fields.filter(([k]) => k === name).map(([, v]) => v);
  }

  files(name: string) {
    return this.parts.filter((p) => p.name === name);
  }
}

const utf8 = new TextDecoder();

function indexOf(hay: Uint8Array, needle: Uint8Array, from: number): number {
  outer: for (let i = from; i <= hay.length - needle.length; i++) {
    for (let j = 0; j < needle.length; j++) {
      if (hay[i + j] !== needle[j]) continue outer;
    }
    return i;
  }
  return -1;
}

/** A header parameter (`name="x"`), quoted or not. */
function headerParam(header: string, key: string): string | undefined {
  const re = new RegExp(`(?:^|;)\\s*${key}\\s*=\\s*(?:"((?:[^"\\\\]|\\\\.)*)"|([^;]*))`, "i");
  const m = re.exec(header);
  if (!m) return undefined;
  return m[1] !== undefined ? m[1].replace(/\\(.)/g, "$1") : m[2]!.trim();
}

/** Parse a multipart body (RFC 7578) into `set`. */
export function parseMultipart(body: Uint8Array, boundary: string, set: ParamSet) {
  const enc = new TextEncoder();
  const delimiter = enc.encode(`--${boundary}`);
  let at = indexOf(body, delimiter, 0);
  while (at >= 0) {
    let start = at + delimiter.length;
    // `--` after the delimiter ends the body.
    if (body[start] === 0x2d && body[start + 1] === 0x2d) return;
    if (body[start] === 0x0d && body[start + 1] === 0x0a) start += 2;
    else if (body[start] === 0x0a) start += 1;
    const next = indexOf(body, delimiter, start);
    if (next < 0) return;
    // The part ends before the CRLF that precedes the next delimiter.
    let end = next;
    if (body[end - 1] === 0x0a) end--;
    if (body[end - 1] === 0x0d) end--;
    const part = body.subarray(start, end);
    let split = indexOf(part, enc.encode("\r\n\r\n"), 0);
    let skip = 4;
    if (split < 0) {
      split = indexOf(part, enc.encode("\n\n"), 0);
      skip = 2;
    }
    if (split >= 0) {
      const headers = utf8.decode(part.subarray(0, split)).split(/\r?\n/);
      const data = part.subarray(split + skip);
      const disposition = headers.find((h) => /^content-disposition\s*:/i.test(h)) ?? "";
      const value = disposition.slice(disposition.indexOf(":") + 1);
      const name = headerParam(value, "name");
      const filename = headerParam(value, "filename");
      if (name !== undefined) {
        if (filename !== undefined) set.parts.push({ name, filename, data: data.slice() });
        else set.fields.push([name, utf8.decode(data)]);
      }
    }
    at = next;
  }
}

/** All of a request's parameters. */
export async function readParams(request: Request): Promise<Params> {
  const set = new ParamSet();
  for (const [k, v] of new URL(request.url).searchParams) set.fields.push([k, v]);
  if (request.method === "GET" || request.method === "HEAD") return set;
  const type = request.headers.get("content-type") ?? "";
  const body = await request.bytes();
  if (body.length === 0) return set;
  if (/^multipart\/form-data/i.test(type)) {
    const boundary = headerParam(type.slice(type.indexOf(";") + 1), "boundary");
    if (boundary) parseMultipart(body, boundary, set);
  } else {
    // qBittorrent reads a body without a content type as a form too.
    for (const [k, v] of new URLSearchParams(utf8.decode(body))) set.fields.push([k, v]);
  }
  return set;
}

/** A cookie from the request's `Cookie` header. */
export function cookie(request: Request, name: string): string | undefined {
  for (const pair of (request.headers.get("cookie") ?? "").split(";")) {
    const i = pair.indexOf("=");
    if (i > 0 && pair.slice(0, i).trim() === name) return pair.slice(i + 1).trim();
  }
  return undefined;
}

/** qBittorrent's booleans: `true`/`false`, any case. */
export function bool(v: string | undefined, fallback = false): boolean {
  if (v === undefined || v === "") return fallback;
  return v.toLowerCase() === "true";
}

/** A number parameter, or `fallback` when absent or not a number. */
export function num(v: string | undefined, fallback: number): number {
  if (v === undefined || v.trim() === "") return fallback;
  const n = Number(v);
  return Number.isFinite(n) ? n : fallback;
}

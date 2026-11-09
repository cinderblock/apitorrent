/**
 * Network grants, as the host reads them (a port of `apitorrent_plugin_api::network`), so
 * the test harness refuses the same URLs the host would.
 *
 * `example.com` (HTTPS, port 443), `*.example.com` (subdomains only), `example.com:8443`,
 * `example.com:*`, `http://example.com` (plain HTTP too), IP addresses, `[::1]:80`.
 */

export interface Grant {
  plainHttp: boolean;
  host: { kind: "name" | "subdomains" | "ip"; value: string };
  port: "default" | "any" | number;
}

export function parseGrant(text: string): Grant | null {
  let rest = text.trim();
  let plainHttp = false;
  if (rest.startsWith("http://")) {
    plainHttp = true;
    rest = rest.slice(7);
  } else if (rest.startsWith("https://")) {
    rest = rest.slice(8);
  } else if (rest.includes("://")) {
    return null;
  }
  if (rest.endsWith("/")) rest = rest.slice(0, -1);
  if (!rest || /[/?#@]/.test(rest)) return null;
  let hostText: string;
  let portText: string | undefined;
  if (rest.startsWith("[")) {
    const end = rest.indexOf("]");
    if (end < 0) return null;
    hostText = rest.slice(0, end + 1);
    const tail = rest.slice(end + 1);
    if (tail) {
      if (!tail.startsWith(":")) return null;
      portText = tail.slice(1);
    }
  } else {
    const colon = rest.lastIndexOf(":");
    hostText = colon < 0 ? rest : rest.slice(0, colon);
    portText = colon < 0 ? undefined : rest.slice(colon + 1);
  }
  let port: Grant["port"] = "default";
  if (portText === "*") port = "any";
  else if (portText !== undefined) {
    const n = Number(portText);
    if (!Number.isInteger(n) || n < 1 || n > 65535) return null;
    port = n;
  }
  const wildcard = hostText.startsWith("*.");
  const hostname = (wildcard ? hostText.slice(2) : hostText).toLowerCase();
  if (hostname.includes("*")) return null;
  const parsed = URL.parse(`https://${hostname}/`);
  if (!parsed) return null;
  const isIp = /^\d+\.\d+\.\d+\.\d+$/.test(parsed.hostname) || parsed.hostname.startsWith("[");
  if (wildcard && (isIp || !parsed.hostname.includes("."))) return null;
  return {
    plainHttp,
    host: { kind: wildcard ? "subdomains" : isIp ? "ip" : "name", value: parsed.hostname },
    port,
  };
}

/** A settings value as a grant: a URL grants its origin, anything else is a pattern. */
export function grantFromSetting(value: string): Grant | null {
  const url = URL.parse(value.trim());
  if (url && (url.protocol === "http:" || url.protocol === "https:") && url.hostname) {
    const isIp = /^\d+\.\d+\.\d+\.\d+$/.test(url.hostname) || url.hostname.startsWith("[");
    return {
      plainHttp: url.protocol === "http:",
      host: { kind: isIp ? "ip" : "name", value: url.hostname },
      port: url.port ? Number(url.port) : "default",
    };
  }
  return parseGrant(value);
}

export function allows(grant: Grant, target: string | URL): boolean {
  const url = typeof target === "string" ? URL.parse(target) : target;
  if (!url) return false;
  const https = url.protocol === "https:";
  if (!https && !(url.protocol === "http:" && grant.plainHttp)) return false;
  const port = url.port ? Number(url.port) : https ? 443 : 80;
  const portOk = grant.port === "any" ||
    (grant.port === "default" ? port === (https ? 443 : 80) : grant.port === port);
  if (!portOk) return false;
  const host = url.hostname.toLowerCase();
  switch (grant.host.kind) {
    case "name":
    case "ip":
      return host === grant.host.value;
    case "subdomains":
      return host.length > grant.host.value.length + 1 && host.endsWith(`.${grant.host.value}`);
  }
}

/** Every value at a settings path such as `feeds[].url` or `hosts[]`. */
export function stringsAt(value: unknown, path: string): string[] {
  let current: unknown[] = [value];
  for (const seg of path.split(".")) {
    const array = seg.endsWith("[]");
    const name = array ? seg.slice(0, -2) : seg;
    current = current.flatMap((v) => {
      const next = v && typeof v === "object" ? (v as Record<string, unknown>)[name] : undefined;
      if (next === undefined) return [];
      if (array) return Array.isArray(next) ? next : [];
      return [next];
    });
  }
  return current.filter((v): v is string => typeof v === "string" && v.trim() !== "");
}

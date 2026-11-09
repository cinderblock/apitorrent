/**
 * ACME DNS-01 through Cloudflare's DNS API (v4).
 *
 * apitorrent keeps the ACME account, the keys and the certificates; this plugin only adds
 * and removes the `_acme-challenge` TXT records, with a token scoped to the zone. The
 * daemon then waits until Cloudflare's name servers serve a new record before the CA looks.
 *
 * - The zone is the `zoneId` setting, or found by walking up the record's domain
 *   (`GET /zones?name=`), which needs Zone > Zone > Read.
 * - Records are created with quoted content, as Cloudflare asks for TXT records. A
 *   wildcard and its base domain share a record name; removal deletes only the record
 *   with the given value.
 * - `POST /api/v1/plugins/cloudflare-dns/verify` checks the token (`/user/tokens/verify`).
 */

import { definePlugin, type Api, type TxtRecord } from "@apitorrent/plugin-sdk";

export interface Settings {
  apiToken?: string;
  zoneId: string;
  ttl: number;
  apiBase: string;
}

interface CfResponse<T> {
  success: boolean;
  errors?: { code: number; message: string }[];
  result: T;
}

interface DnsRecord {
  id: string;
  name: string;
  type: string;
  content: string;
}

/** Cloudflare's "an identical record already exists". */
const ALREADY_EXISTS = 81058;

export class CloudflareError extends Error {
  override name = "CloudflareError";
  constructor(
    message: string,
    public codes: number[],
  ) {
    super(message);
  }
}

export function quote(value: string): string {
  return `"${value.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`;
}

export function unquote(content: string): string {
  const c = content.trim();
  if (c.length >= 2 && c.startsWith('"') && c.endsWith('"')) {
    return c.slice(1, -1).replace(/\\(.)/g, "$1");
  }
  return c;
}

export class Cloudflare {
  private zones = new Map<string, string>();

  constructor(private api: Api<Settings>) {}

  private async call<T>(method: string, path: string, body?: unknown): Promise<T> {
    const s = this.api.settings.get();
    if (!s.apiToken) throw new CloudflareError("no API token is set in the plugin's settings", []);
    const base = s.apiBase.replace(/\/+$/, "");
    const headers: Record<string, string> = {
      authorization: `Bearer ${s.apiToken}`,
      accept: "application/json",
    };
    if (body !== undefined) headers["content-type"] = "application/json";
    const r = await fetch(`${base}${path}`, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    let parsed: CfResponse<T>;
    try {
      parsed = (await r.json()) as CfResponse<T>;
    } catch {
      throw new CloudflareError(`Cloudflare answered ${method} ${path} with HTTP ${r.status} and no JSON`, []);
    }
    if (!r.ok || !parsed.success) {
      const errors = parsed.errors ?? [];
      const text = errors.map((e) => `${e.message} (${e.code})`).join("; ") || `HTTP ${r.status}`;
      throw new CloudflareError(`Cloudflare refused ${method} ${path}: ${text}`, errors.map((e) => e.code));
    }
    return parsed.result;
  }

  /** The zone holding `domain`: the setting, or the closest enclosing zone the token sees. */
  async zoneFor(domain: string): Promise<string> {
    const configured = this.api.settings.get().zoneId.trim();
    if (configured) return configured;
    const labels = domain.toLowerCase().replace(/\.$/, "").split(".");
    for (let i = 0; i < labels.length - 1; i++) {
      const name = labels.slice(i).join(".");
      const cached = this.zones.get(name);
      if (cached) return cached;
      const found = await this.call<{ id: string; name: string }[]>(
        "GET",
        `/zones?name=${encodeURIComponent(name)}&status=active`,
      );
      const zone = found.find((z) => z.name.toLowerCase() === name);
      if (zone) {
        // Remember it for every name walked on the way, so the next lookup is free.
        for (let j = 0; j <= i; j++) this.zones.set(labels.slice(j).join("."), zone.id);
        return zone.id;
      }
    }
    throw new CloudflareError(
      `no Cloudflare zone this token can see holds ${domain}; set the zone ID, or give the token Zone > Zone > Read`,
      [],
    );
  }

  async setTxt(r: TxtRecord): Promise<void> {
    const zone = await this.zoneFor(r.domain);
    try {
      await this.call("POST", `/zones/${zone}/dns_records`, {
        type: "TXT",
        name: r.name,
        content: quote(r.value),
        ttl: this.api.settings.get().ttl,
        comment: "apitorrent ACME challenge",
      });
      console.info(`published ${r.name}`);
    } catch (e) {
      // The same record twice (a retried order) is fine.
      if (e instanceof CloudflareError && e.codes.includes(ALREADY_EXISTS)) return;
      throw e;
    }
  }

  async removeTxt(r: TxtRecord): Promise<void> {
    const zone = await this.zoneFor(r.domain);
    const records = await this.call<DnsRecord[]>(
      "GET",
      `/zones/${zone}/dns_records?type=TXT&name=${encodeURIComponent(r.name)}&per_page=100`,
    );
    for (const rec of records) {
      if (rec.type === "TXT" && rec.name.toLowerCase() === r.name.toLowerCase() && unquote(rec.content) === r.value) {
        await this.call("DELETE", `/zones/${zone}/dns_records/${rec.id}`);
        console.info(`removed ${r.name}`);
      }
    }
  }

  async verify(): Promise<{ ok: boolean; status?: string; message?: string }> {
    try {
      const r = await this.call<{ status: string }>("GET", "/user/tokens/verify");
      return { ok: r.status === "active", status: r.status };
    } catch (e) {
      return { ok: false, message: e instanceof Error ? e.message : String(e) };
    }
  }
}

export default definePlugin<Settings>({
  start(api) {
    const cf = new Cloudflare(api);
    api.roles.provide("dns-provider", {
      setTxt: (r) => cf.setTxt(r),
      removeTxt: (r) => cf.removeTxt(r),
    });
    api.http.route("POST", "/verify", () => cf.verify());
  },
});

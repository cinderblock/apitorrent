import { describe, expect, test } from "bun:test";

import type { PluginManifest, TxtRecord } from "@apitorrent/plugin-sdk";
import { createTestHost } from "@apitorrent/plugin-sdk/testing";

import plugin, { quote, unquote } from "../src/index";
import manifestJson from "../manifest.json";

const manifest = manifestJson as unknown as PluginManifest;
const TOKEN = "cf-test-token";

/** A mock of the parts of Cloudflare's v4 API the plugin uses. No real network. */
function mockCloudflare() {
  const records: { id: string; zone: string; type: string; name: string; content: string; ttl: number }[] = [];
  const calls: string[] = [];
  let next = 1;
  const handler = async (req: Request): Promise<Response> => {
    const url = new URL(req.url);
    calls.push(`${req.method} ${url.pathname}${url.search}`);
    const ok = (result: unknown) => Response.json({ success: true, errors: [], result });
    const fail = (status: number, code: number, message: string) =>
      Response.json({ success: false, errors: [{ code, message }], result: null }, { status });
    if (req.headers.get("authorization") !== `Bearer ${TOKEN}`) return fail(403, 10000, "Authentication error");
    const path = url.pathname.replace(/^\/client\/v4/, "");
    if (req.method === "GET" && path === "/zones") {
      const name = url.searchParams.get("name");
      return ok(name === "example.com" ? [{ id: "zone1", name: "example.com" }] : []);
    }
    if (req.method === "GET" && path === "/user/tokens/verify") return ok({ id: "t", status: "active" });
    const m = /^\/zones\/([^/]+)\/dns_records(?:\/([^/]+))?$/.exec(path);
    if (!m) return fail(404, 7003, "No route");
    const [, zone, id] = m;
    if (zone !== "zone1") return fail(404, 7003, "Could not route to zone");
    if (req.method === "POST" && !id) {
      const body = (await req.json()) as { type: string; name: string; content: string; ttl: number };
      if (records.some((r) => r.name === body.name && r.content === body.content)) {
        return fail(400, 81058, "An identical record already exists.");
      }
      const rec = { id: `r${next++}`, zone, ...body };
      records.push(rec);
      return ok(rec);
    }
    if (req.method === "GET" && !id) {
      return ok(records.filter((r) => r.type === url.searchParams.get("type") && r.name === url.searchParams.get("name")));
    }
    if (req.method === "DELETE" && id) {
      const i = records.findIndex((r) => r.id === id);
      if (i < 0) return fail(404, 81044, "Record does not exist.");
      records.splice(i, 1);
      return ok({ id });
    }
    return fail(405, 10000, "Method not allowed");
  };
  return { records, calls, handler };
}

const record = (name: string, value: string): TxtRecord => ({
  domain: name.replace(/^_acme-challenge\./, ""),
  name,
  value,
});

describe("cloudflare-dns", () => {
  test("publishes and removes DNS-01 records, finding the zone from the name", async () => {
    const cf = mockCloudflare();
    const host = createTestHost({ manifest, settings: { apiToken: TOKEN }, fetch: cf.handler });
    await host.start(plugin);
    // A wildcard and its base domain share a name, with two values.
    const base = record("_acme-challenge.www.example.com", "value-one");
    const wild = record("_acme-challenge.www.example.com", "value-two");
    await host.callRole("dns-provider", "setTxt", base);
    await host.callRole("dns-provider", "setTxt", wild);
    // A retried publish of the same record is fine.
    await host.callRole("dns-provider", "setTxt", base);
    expect(cf.records.map((r) => [r.name, r.content, r.ttl])).toEqual([
      ["_acme-challenge.www.example.com", '"value-one"', 120],
      ["_acme-challenge.www.example.com", '"value-two"', 120],
    ]);
    // The zone was found by walking up: www.example.com, then example.com (cached after).
    expect(cf.calls.filter((c) => c.startsWith("GET /client/v4/zones?"))).toEqual([
      "GET /client/v4/zones?name=www.example.com&status=active",
      "GET /client/v4/zones?name=example.com&status=active",
    ]);
    // Removal takes only the record with that value.
    await host.callRole("dns-provider", "removeTxt", base);
    expect(cf.records.map((r) => r.content)).toEqual(['"value-two"']);
    await host.callRole("dns-provider", "removeTxt", wild);
    expect(cf.records).toEqual([]);
    expect(await (await host.request("POST", "/verify")).json()).toEqual({ ok: true, status: "active" });
    await host.stop();
  });

  test("Cloudflare's refusals come back as errors with its messages", async () => {
    const cf = mockCloudflare();
    const host = createTestHost({ manifest, settings: { apiToken: "wrong" }, fetch: cf.handler });
    await host.start(plugin);
    const e = await host.callRole("dns-provider", "setTxt", record("_acme-challenge.example.com", "v")).catch((x) => x);
    expect(String(e)).toContain("Authentication error (10000)");
    expect((await (await host.request("POST", "/verify")).json()).ok).toBe(false);
    await host.stop();

    const other = createTestHost({ manifest, settings: { apiToken: TOKEN }, fetch: cf.handler });
    await other.start(plugin);
    const e2 = await other.callRole("dns-provider", "setTxt", record("_acme-challenge.example.org", "v")).catch((x) => x);
    expect(String(e2)).toContain("no Cloudflare zone");
    await other.stop();

    const unset = createTestHost({ manifest, settings: {}, fetch: cf.handler });
    await unset.start(plugin);
    const e3 = await unset.callRole("dns-provider", "setTxt", record("_acme-challenge.example.com", "v")).catch((x) => x);
    expect(String(e3)).toContain("no API token");
    await unset.stop();
  });

  test("a configured zone id skips the lookup; another API address must be granted by settings", async () => {
    const cf = mockCloudflare();
    const host = createTestHost({
      manifest,
      settings: { apiToken: TOKEN, zoneId: "zone1", apiBase: "http://127.0.0.1:8787/client/v4" },
      fetch: cf.handler,
    });
    await host.start(plugin);
    await host.callRole("dns-provider", "setTxt", record("_acme-challenge.example.com", "v"));
    expect(cf.calls).toEqual(["POST /client/v4/zones/zone1/dns_records"]);
    await host.stop();
  });

  test("TXT content quoting", () => {
    expect(quote("abc")).toBe('"abc"');
    expect(unquote('"abc"')).toBe("abc");
    expect(unquote("abc")).toBe("abc");
    expect(unquote(quote('a"b\\c'))).toBe('a"b\\c');
  });
});

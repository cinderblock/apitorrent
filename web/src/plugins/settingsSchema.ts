// The pure side of the Plugins page: reading a plugin's settings schema (the JSON Schema
// subset the server validates, see apitorrent_plugin_api::schema), making values for new
// array items, and describing capabilities in words.

import type { PluginCapabilities } from "../api/generated/PluginCapabilities";

export interface JsonSchema {
  type: "object" | "array" | "string" | "number" | "integer" | "boolean";
  title?: string;
  description?: string;
  default?: unknown;
  enum?: (string | number | boolean)[];
  properties?: Record<string, JsonSchema>;
  required?: string[];
  items?: JsonSchema;
  minItems?: number;
  maxItems?: number;
  minLength?: number;
  maxLength?: number;
  format?: "uri" | "multiline";
  minimum?: number;
  maximum?: number;
  writeOnly?: boolean;
}

export type Values = Record<string, unknown>;

/** A label for a property: its title, or its name made readable. */
export function labelFor(name: string, schema: JsonSchema): string {
  if (schema.title) return schema.title;
  const words = name.replace(/([a-z])([A-Z])/g, "$1 $2").replace(/[_-]+/g, " ");
  return words.charAt(0).toUpperCase() + words.slice(1).toLowerCase();
}

/** A fresh value for a schema: its default, or an empty value of its type. */
export function emptyValue(schema: JsonSchema): unknown {
  if (schema.default !== undefined) return structuredClone(schema.default);
  switch (schema.type) {
    case "object": {
      const out: Values = {};
      for (const [k, sub] of Object.entries(schema.properties ?? {})) {
        const v = emptyValue(sub);
        if (v !== undefined) out[k] = v;
      }
      return out;
    }
    case "array":
      return [];
    case "boolean":
      return false;
    case "string":
      return schema.enum ? schema.enum[0] : "";
    default:
      return undefined;
  }
}

/** Top-level properties that are secrets: never shown, only set or cleared. */
export function secretNames(schema: JsonSchema | null): string[] {
  return Object.entries(schema?.properties ?? {})
    .filter(([, s]) => s.writeOnly === true)
    .map(([k]) => k);
}

/** What a secret field holds in the form: untouched, a new value, or cleared. */
export type SecretEdit = { kind: "keep" } | { kind: "set"; value: string } | { kind: "clear" };

/**
 * The request body for saving: the form's values, minus empty optional strings (so
 * "nothing" stays absent rather than an empty string), with secrets the user didn't touch
 * left out (the server keeps them) and cleared ones sent as `null`.
 */
export function toRequest(schema: JsonSchema, values: Values, secrets: Record<string, SecretEdit>): Values {
  const out = prune(schema, values) as Values;
  for (const [name, edit] of Object.entries(secrets)) {
    if (edit.kind === "set") out[name] = edit.value;
    else if (edit.kind === "clear") out[name] = null;
    else delete out[name];
  }
  return out;
}

function prune(schema: JsonSchema, value: unknown): unknown {
  if (schema.type === "object" && value && typeof value === "object" && !Array.isArray(value)) {
    const out: Values = {};
    const required = new Set(schema.required ?? []);
    for (const [k, v] of Object.entries(value as Values)) {
      const sub = schema.properties?.[k];
      if (!sub) continue;
      if (v === undefined || (v === "" && sub.type === "string" && !required.has(k) && sub.default === undefined)) {
        continue;
      }
      out[k] = prune(sub, v);
    }
    return out;
  }
  if (schema.type === "array" && Array.isArray(value) && schema.items) {
    return value.map((v) => prune(schema.items!, v));
  }
  return value;
}

/** Parse a number field's text; empty is "unset". */
export function parseNumber(text: string, integer: boolean): number | undefined {
  if (text.trim() === "") return undefined;
  const n = Number(text);
  if (!Number.isFinite(n)) return undefined;
  return integer ? Math.trunc(n) : n;
}

const SCOPES: Record<string, string> = {
  "torrents:read": "see all torrents and their details",
  "torrents:add": "add torrents and magnet links",
  "torrents:control": "pause, resume and label torrents",
  "torrents:remove": "remove torrents, and delete their data",
  "events:read": "be told when torrents are added, change state or finish",
  "tokens:verify": "check the apitorrent API tokens its clients sign in with, and learn whose they are",
};

const ROLES: Record<string, string> = {
  source: "act as a release source",
  "api-compat": "act as another client's API (compatibility layer)",
  "dns-provider": "publish DNS records for certificates (DNS provider)",
  "tracker-adapter": "adapt behaviour to specific trackers",
};

function grantText(g: string): string {
  if (g.startsWith("http://") || g.startsWith("https://")) return g;
  return `https://${g}`;
}

/** Everything a set of capabilities allows, in words (the server's wording for approvals). */
export function describeCapabilities(c: PluginCapabilities): string[] {
  const out: string[] = [];
  for (const g of c.network ?? []) out.push(`connect to ${grantText(g)}`);
  for (const p of c.networkFromSettings ?? []) out.push(`connect to the hosts you enter in its \`${p}\` setting`);
  for (const s of c.api ?? []) out.push(SCOPES[s] ?? s);
  for (const r of c.routes ?? []) {
    for (const m of r.methods) {
      out.push(
        r.auth === "plugin"
          ? `serve ${m} ${r.path} under its API path to clients that haven't signed in to apitorrent (it checks their credentials itself, and may set cookies there)`
          : `serve ${m} ${r.path} under its API path`,
      );
    }
  }
  if (c.kv) out.push("store its own data");
  if (c.schedule) out.push("run jobs on a schedule");
  for (const r of c.roles ?? []) out.push(ROLES[r] ?? r);
  return out;
}

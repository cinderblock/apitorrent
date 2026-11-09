/**
 * Settings defaults, filled in the way the host fills them (a port of
 * `apitorrent_plugin_api::schema::with_defaults`): missing properties take their `default`,
 * missing objects become objects with their own defaults, and array items that are
 * objects get theirs.
 */

type Schema = {
  type?: string;
  default?: unknown;
  properties?: Record<string, Schema>;
  items?: Schema;
};

export function withDefaults(schema: unknown, value: unknown): unknown {
  const s = (schema ?? {}) as Schema;
  if (s.type === "object" && value && typeof value === "object" && !Array.isArray(value)) {
    const out: Record<string, unknown> = { ...(value as Record<string, unknown>) };
    for (const [k, sub] of Object.entries(s.properties ?? {})) {
      if (k in out) out[k] = withDefaults(sub, out[k]);
      else if (sub.default !== undefined) out[k] = withDefaults(sub, structuredClone(sub.default));
      else if (sub.type === "object") out[k] = withDefaults(sub, {});
    }
    return out;
  }
  if (s.type === "array" && Array.isArray(value) && s.items) {
    return value.map((v) => withDefaults(s.items, v));
  }
  return value;
}

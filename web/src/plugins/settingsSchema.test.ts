import { describe, expect, test } from "bun:test";

import {
  describeCapabilities,
  emptyValue,
  labelFor,
  parseNumber,
  secretNames,
  toRequest,
  type JsonSchema,
} from "./settingsSchema";

const schema: JsonSchema = {
  type: "object",
  properties: {
    token: { type: "string", writeOnly: true },
    interval: { type: "integer", default: 15 },
    mode: { type: "string", enum: ["lts", "all"] },
    label: { type: "string" },
    feeds: {
      type: "array",
      items: {
        type: "object",
        required: ["url"],
        properties: {
          url: { type: "string", format: "uri" },
          enabled: { type: "boolean", default: true },
          name: { type: "string" },
        },
      },
    },
  },
};

describe("settings forms", () => {
  test("labels", () => {
    expect(labelFor("intervalMinutes", { type: "integer" })).toBe("Interval minutes");
    expect(labelFor("x", { type: "string", title: "Feed URL" })).toBe("Feed URL");
  });

  test("new array items start from defaults", () => {
    expect(emptyValue(schema.properties!.feeds!.items!)).toEqual({ url: "", enabled: true, name: "" });
    expect(emptyValue(schema.properties!.mode!)).toBe("lts");
    expect(emptyValue({ type: "integer" })).toBeUndefined();
  });

  test("saving drops empty optional strings and handles secrets", () => {
    expect(secretNames(schema)).toEqual(["token"]);
    const values = {
      interval: 20,
      label: "",
      mode: "all",
      feeds: [{ url: "", enabled: true, name: "" }],
      token: "should not be sent as-is",
    };
    expect(toRequest(schema, values, { token: { kind: "keep" } })).toEqual({
      interval: 20,
      mode: "all",
      // A required string stays, empty, so the server can say it's missing.
      feeds: [{ url: "", enabled: true }],
    });
    expect(toRequest(schema, values, { token: { kind: "set", value: "s3cret" } }).token).toBe("s3cret");
    expect(toRequest(schema, values, { token: { kind: "clear" } }).token).toBeNull();
  });

  test("numbers", () => {
    expect(parseNumber("", true)).toBeUndefined();
    expect(parseNumber("12.7", true)).toBe(12);
    expect(parseNumber("12.7", false)).toBe(12.7);
    expect(parseNumber("x", false)).toBeUndefined();
  });
});

test("capabilities in words", () => {
  expect(
    describeCapabilities({
      network: ["changelogs.ubuntu.com", "http://127.0.0.1:8080"],
      networkFromSettings: ["feeds[].url"],
      api: ["torrents:add"],
      routes: [{ path: "/status", methods: ["GET"] }],
      kv: true,
      schedule: false,
      roles: ["source"],
    }),
  ).toEqual([
    "connect to https://changelogs.ubuntu.com",
    "connect to http://127.0.0.1:8080",
    "connect to the hosts you enter in its `feeds[].url` setting",
    "add torrents and magnet links",
    "serve GET /status under its API path",
    "store its own data",
    "act as a release source",
  ]);
});

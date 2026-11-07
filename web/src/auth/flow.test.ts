import { expect, test } from "bun:test";

import { ApiRequestError } from "../api/client";
import type { AuthStatus } from "../api/generated/AuthStatus";
import { Cancelled, describeError, describeUserAgent, readSetupToken, viewFor, withReauth } from "./flow";

const base: AuthStatus = {
  setupRequired: false,
  authDisabled: false,
  user: null,
  session: null,
  secondFactorRequired: false,
  passkeysAvailable: true,
  passkeyHint: null,
  passwordMode: "off",
  insecureTransport: false,
};

const admin = { id: "u", name: "admin", displayName: "Admin", role: "admin" as const, hasPassword: false };

test("view for status", () => {
  expect(viewFor(null)).toBe("loading");
  expect(viewFor(undefined)).toBe("unreachable");
  expect(viewFor({ ...base, setupRequired: true })).toBe("setup");
  expect(viewFor(base)).toBe("login");
  expect(viewFor({ ...base, secondFactorRequired: true })).toBe("secondFactor");
  expect(viewFor({ ...base, user: admin })).toBe("app");
  // Signed in beats everything else.
  expect(viewFor({ ...base, user: admin, setupRequired: true })).toBe("app");
});

test("setup token from the fragment", () => {
  expect(readSetupToken("#token=abcd-efgh")).toBe("abcd-efgh");
  expect(readSetupToken("token=abcd")).toBe("abcd");
  expect(readSetupToken("#other=1&token=%20x%20")).toBe("x");
  expect(readSetupToken("")).toBeNull();
  expect(readSetupToken("#token=")).toBeNull();
});

test("withReauth retries once after re-authenticating", async () => {
  let calls = 0;
  let reauths = 0;
  const result = await withReauth(
    async () => {
      calls++;
      if (calls === 1) throw new ApiRequestError(403, "reauth_required", "sign in again");
      return "done";
    },
    async () => {
      reauths++;
    },
  );
  expect(result).toBe("done");
  expect(calls).toBe(2);
  expect(reauths).toBe(1);
});

test("withReauth passes other errors through, and doesn't loop", async () => {
  const other = new ApiRequestError(409, "conflict", "nope");
  await expect(withReauth(() => Promise.reject(other), async () => {})).rejects.toBe(other);

  let reauths = 0;
  const always = new ApiRequestError(403, "reauth_required", "again");
  await expect(
    withReauth(
      () => Promise.reject(always),
      async () => {
        reauths++;
      },
    ),
  ).rejects.toBe(always);
  expect(reauths).toBe(1);

  // A cancelled re-auth stops there.
  await expect(
    withReauth(
      () => Promise.reject(always),
      () => Promise.reject(new Cancelled()),
    ),
  ).rejects.toBeInstanceOf(Cancelled);
});

test("error messages", () => {
  const dom = (name: string) => Object.assign(new Error("x"), { name });
  expect(describeError(dom("NotAllowedError"))).toContain("cancelled");
  expect(describeError(dom("InvalidStateError"))).toContain("already");
  expect(describeError(dom("AbortError"))).toBeNull();
  expect(describeError(new Cancelled())).toBeNull();
  expect(describeError(new ApiRequestError(401, "unauthorized", "authentication failed"))).toContain("didn't work");
  expect(describeError(new ApiRequestError(429, "rate_limited", "try again in 60 seconds"))).toBe(
    "try again in 60 seconds",
  );
});

test("user agents", () => {
  expect(
    describeUserAgent(
      "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0 Safari/537.36",
    ),
  ).toBe("Chrome on Windows");
  expect(
    describeUserAgent(
      "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1",
    ),
  ).toBe("Safari on iOS");
  expect(describeUserAgent("curl/8.9")).toBe("curl/8.9");
  expect(describeUserAgent(null)).toBe("Unknown device");
});

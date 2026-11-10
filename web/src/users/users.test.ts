import { describe, expect, test } from "bun:test";

import { describeGrants, enrollLinkAt, timeLeft, validUsername } from "./format";

describe("invitation words", () => {
  test("grants", () => {
    expect(describeGrants({ spaces: [] })).toBe("no spaces");
    expect(
      describeGrants({
        spaces: [
          { space: 2, slug: "tv", name: "TV", role: "user" },
          { space: 3, slug: "movies", name: "Movies", role: "viewer" },
        ],
      }),
    ).toBe("TV (user), Movies (viewer)");
  });

  test("time left", () => {
    const now = 1_000_000;
    expect(timeLeft(now, now)).toBe("expired");
    expect(timeLeft(now + 30_000, now)).toBe("in 1 minute");
    expect(timeLeft(now + 5 * 60_000, now)).toBe("in 5 minutes");
    expect(timeLeft(now + 3_600_000, now)).toBe("in 1 hour");
    expect(timeLeft(now + 47 * 3_600_000, now)).toBe("in 47 hours");
    expect(timeLeft(now + 7 * 86_400_000, now)).toBe("in 7 days");
  });

  test("the link elsewhere keeps its secret in the fragment", () => {
    expect(enrollLinkAt("http://localhost:8742/", "a-b_c")).toBe("http://localhost:8742/enroll#token=a-b_c");
  });

  test("usernames", () => {
    for (const ok of ["bob", "Bob", "a.b-c_d", "0"]) expect(validUsername(ok)).toBe(true);
    for (const bad of ["", "-a", "a b", "é", "a".repeat(65)]) expect(validUsername(bad)).toBe(false);
  });
});

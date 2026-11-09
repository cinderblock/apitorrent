import { expect, test } from "bun:test";

import { describeRule, lines, newRule, orNull, problems, textMatch } from "./ruleForm";

test("list and text fields", () => {
  expect(lines(" *.iso \n\n**/SHA256SUMS\r\n")).toEqual(["*.iso", "**/SHA256SUMS"]);
  expect(textMatch("contains", "  ")).toBeNull();
  expect(textMatch("regex", " \\d+ ")).toEqual({ mode: "regex", pattern: "\\d+" });
  expect(orNull("  ")).toBeNull();
  expect(orNull(" tv ")).toBe("tv");
});

test("rules are described in words", () => {
  const r = newRule();
  r.name = "Ubuntu";
  expect(describeRule(r)).toBe("Keeps the latest 1 · older ones: keep seeding");
  r.keepLatest = 2;
  r.label = "ubuntu";
  r.match.sources = ["plugin:ubuntu-releases"];
  r.superseded = { action: "seedLimits", limits: { ratio: 2, seedMinutes: 0, action: "remove" } };
  r.alias = { path: "library/ubuntu/latest.iso", file: "*.iso" };
  expect(describeRule(r)).toBe(
    "Keeps the latest 2 · from plugin:ubuntu-releases · label ubuntu · " +
      "older ones: Ratio 2, then remove it (files stay) · latest at library/ubuntu/latest.iso",
  );
  r.keepLatest = 0;
  expect(describeRule(r)).toStartWith("Keeps every match");
});

test("problems the server would refuse are caught first", () => {
  const r = newRule();
  expect(problems(r)).toEqual(["Give the rule a name."]);
  r.name = "x";
  r.alias = { path: "../outside.iso", file: null };
  expect(problems(r)).toHaveLength(1);
  r.alias = { path: "C:\\x.iso", file: null };
  expect(problems(r)).toHaveLength(1);
  r.alias = { path: "library/latest.iso", file: null };
  r.keepLatest = 1.5;
  expect(problems(r)).toEqual(["Keep between 0 (all) and 1000 releases."]);
});

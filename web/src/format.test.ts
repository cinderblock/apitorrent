import { expect, test } from "bun:test";

import { formatBytes, formatEta, formatPercent, formatRatio, fromKiB, toKiB } from "./format";

test("bytes", () => {
  expect(formatBytes(0)).toBe("0 B");
  expect(formatBytes(1023)).toBe("1023 B");
  expect(formatBytes(1024)).toBe("1.00 KiB");
  expect(formatBytes(5.5 * 1024 ** 3)).toBe("5.50 GiB");
  expect(formatBytes(123.4 * 1024 ** 2)).toBe("123 MiB");
});

test("percent never rounds up to done", () => {
  expect(formatPercent(0.9999)).toBe("99.9%");
  expect(formatPercent(1)).toBe("100%");
  expect(formatPercent(0.5)).toBe("50.0%");
});

test("eta", () => {
  expect(formatEta(42)).toBe("42s");
  expect(formatEta(3600 + 5 * 60)).toBe("1h 05m");
  expect(formatEta(Infinity)).toBe("∞");
  expect(formatEta(3 * 86400 + 7200)).toBe("3d 2h");
});

test("ratio", () => {
  expect(formatRatio(0, 0)).toBe("0.00");
  expect(formatRatio(10, 0)).toBe("∞");
  expect(formatRatio(3, 2)).toBe("1.50");
});

test("KiB conversions round-trip", () => {
  expect(toKiB(0)).toBe(0);
  expect(fromKiB(0)).toBe(0);
  expect(fromKiB(500)).toBe(512000);
  expect(toKiB(fromKiB(1234))).toBe(1234);
  expect(fromKiB(-3)).toBe(0);
});

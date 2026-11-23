import { describe, expect, test } from "bun:test";

import type { FamilyReachability } from "../api/generated/FamilyReachability";
import type { PortReachability } from "../api/generated/PortReachability";
import type { Reachability } from "../api/generated/Reachability";
import { ago, familyLines, reachSummary } from "./reachability";

const NOW = 1_800_000_000_000;

function port(state: PortReachability["state"], extra: Partial<PortReachability> = {}): PortReachability {
  return { port: 40000, state, incoming: 0, lastIncoming: null, waitingSince: NOW, ...extra };
}

function family(extra: Partial<FamilyReachability> = {}): FamilyReachability {
  return {
    externalIp: null,
    fromDht: null,
    fromPeers: null,
    fromTracker: null,
    fromRouter: null,
    doubleNat: false,
    tcp: null,
    utp: null,
    dht: null,
    ...extra,
  };
}

function reach(ipv4: Partial<FamilyReachability>, ipv6: Partial<FamilyReachability> = {}): Reachability {
  return { ipv4: family(ipv4), ipv6: family(ipv6) };
}

describe("reachSummary", () => {
  test("open on any family wins", () => {
    expect(reachSummary(reach({ tcp: port("probablyClosed") }, { tcp: port("open") }))).toEqual({
      text: "open",
      tone: "ok",
    });
  });

  test("probably closed, then not confirmed", () => {
    expect(reachSummary(reach({ tcp: port("probablyClosed") }))?.tone).toBe("warn");
    expect(reachSummary(reach({ tcp: port("waiting") }))?.text).toBe("not confirmed yet");
  });

  test("only TCP decides", () => {
    expect(reachSummary(reach({ tcp: port("waiting"), utp: port("open"), dht: port("open") }))?.tone).toBe(
      "muted",
    );
  });

  test("nothing to say without a listener", () => {
    expect(reachSummary(reach({}))).toBeNull();
  });
});

describe("familyLines", () => {
  test("address, ports and when the last connection came", () => {
    const lines = familyLines(
      "IPv4",
      family({
        externalIp: "203.0.113.7",
        tcp: port("open", { lastIncoming: NOW - 3 * 60_000 }),
        utp: port("waiting", { waitingSince: null }),
      }),
      NOW,
    );
    expect(lines).toEqual([
      "Public IPv4 address: 203.0.113.7",
      "IPv4 TCP 40000: open, last incoming connection 3 min ago",
      "IPv4 uTP 40000: no incoming connection yet; waiting for a tracker or the DHT to announce us",
    ]);
  });

  test("a NAT in front of the router", () => {
    const [, warning] = familyLines(
      "IPv4",
      family({ externalIp: "203.0.113.7", fromRouter: "100.70.1.2", doubleNat: true }),
      NOW,
    );
    expect(warning).toContain("The router's public address is 100.70.1.2, not 203.0.113.7");
    const [only] = familyLines("IPv4", family({ fromRouter: "100.70.1.2", doubleNat: true }), NOW);
    expect(only).toContain("100.70.1.2, isn't a public one");
  });
});

test("ago", () => {
  expect(ago(NOW - 10_000, NOW)).toBe("just now");
  expect(ago(NOW - 59 * 60_000, NOW)).toBe("59 min ago");
  expect(ago(NOW - 5 * 3_600_000, NOW)).toBe("5 h ago");
  expect(ago(NOW - 72 * 3_600_000, NOW)).toBe("3 days ago");
});

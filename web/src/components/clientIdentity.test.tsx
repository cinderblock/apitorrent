import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";

import type { IdentitySettings } from "../api/generated/IdentitySettings";
import { ClientIdentitySection } from "./ClientIdentitySection";
import { describeIdentity, hostProblem, identityProblems, moveRule, presetIdentity } from "./clientIdentity";

describe("client identity", () => {
  test("previews match what the server sends", () => {
    // The same examples as the engine's tests (crates/apitorrent-engine/src/identity.rs).
    expect(describeIdentity(presetIdentity("deluge"))).toEqual({
      peerIdPrefix: "-DE211s-",
      userAgent: "Deluge/2.1.1 libtorrent/2.0.10.0",
      clientName: "Deluge/2.1.1 libtorrent/2.0.10.0",
    });
    expect(describeIdentity({ client: "deluge", version: "2.1.2.dev0", libtorrent: "2.0.10.0" })?.peerIdPrefix).toBe("-DE212D-");
    expect(describeIdentity({ client: "deluge", version: "2.0.0b2", libtorrent: "1.2.0.0" })?.peerIdPrefix).toBe("-DE200b-");
    expect(describeIdentity(presetIdentity("qBittorrent"))?.peerIdPrefix).toBe("-qB4650-");
    expect(describeIdentity({ client: "qBittorrent", version: "4.6.10" })?.peerIdPrefix).toBe("-qB46A0-");
    expect(describeIdentity({ client: "qBittorrent", version: "4.6.36" })).toBeNull();
    expect(describeIdentity(presetIdentity("transmission"))).toEqual({
      peerIdPrefix: "-TR4060-",
      userAgent: "Transmission/4.0.6",
      clientName: "Transmission 4.0.6",
    });
    expect(describeIdentity({ client: "transmission", version: "4.1.0-beta.2" })?.peerIdPrefix).toBe("-TR410B-");
    expect(describeIdentity({ client: "transmission", version: "4.0.40" })?.peerIdPrefix).toBe("-TR40e0-");
    expect(describeIdentity({ client: "deluge", version: "two", libtorrent: "2.0.10.0" })).toBeNull();
    expect(describeIdentity({ client: "custom", peerIdPrefix: "-XX1000-", userAgent: "X/1.0\r\nEvil: 1", clientName: "X" })).toBeNull();
  });

  test("host patterns", () => {
    expect(hostProblem("tracker.example.org")).toBeNull();
    expect(hostProblem("*.example.org")).toBeNull();
    expect(hostProblem("Tracker.Example.org.")).toBeNull();
    expect(hostProblem("[::1]")).toBeNull();
    expect(hostProblem("https://tracker.example.org/announce")).not.toBeNull();
    expect(hostProblem("tracker.example.org:443")).not.toBeNull();
    expect(hostProblem("a b")).not.toBeNull();
    expect(hostProblem("")).not.toBeNull();
  });

  test("problems and ordering", () => {
    const s: IdentitySettings = {
      default: presetIdentity("apitorrent"),
      rules: [
        { host: "t.example", identity: presetIdentity("deluge") },
        { host: "T.example.", identity: presetIdentity("transmission") },
        { host: "u.example", identity: { client: "qBittorrent", version: "x" } },
      ],
    };
    const problems = identityProblems(s);
    expect(problems).toHaveLength(2);
    expect(problems[0]).toContain("two rules");
    expect(problems[1]).toContain("u.example");
    expect(identityProblems({ default: presetIdentity("apitorrent"), rules: [] })).toEqual([]);
    expect(moveRule(s.rules, 2, -1).map((r) => r.host)).toEqual(["t.example", "u.example", "T.example."]);
    expect(moveRule(s.rules, 0, -1)).toBe(s.rules);
  });

  test("the settings section warns, and shows each rule and what trackers will see", () => {
    const html = renderToStaticMarkup(
      <ClientIdentitySection
        value={{ default: presetIdentity("apitorrent"), rules: [{ host: "tracker.example.org", identity: presetIdentity("deluge") }] }}
        onChange={() => {}}
      />,
    );
    expect(html).toContain("<h3>Per tracker</h3>");
    expect(html).toContain("can ban your account");
    expect(html).toContain('value="tracker.example.org"');
    expect(html).toContain("<code>-DE211s-…</code>");
    expect(html).toContain("<code>Deluge/2.1.1 libtorrent/2.0.10.0</code>");
    expect(html).toContain('value="2.0.10.0"');
    expect(html).not.toContain("error-text");
    const bad = renderToStaticMarkup(
      <ClientIdentitySection value={{ default: presetIdentity("apitorrent"), rules: [{ host: "http://x/", identity: presetIdentity("qBittorrent") }] }} onChange={() => {}} />,
    );
    expect(bad).toContain("without http://");
  });
});

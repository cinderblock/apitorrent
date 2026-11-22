import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";

import type { Settings } from "../api/generated/Settings";
import { presetIdentity } from "../components/clientIdentity";
import {
  SECTIONS,
  dirtySections,
  draftProblems,
  filterSections,
  forSaving,
  listTitles,
  parseAmount,
  parseLabels,
  pickRateUnit,
  rateFrom,
  rateIn,
  visibleSections,
} from "./model";
import { FilesSection, GroupsSection, NetworkSection, TransfersSection } from "./ServerSections";

const BASE: Settings = {
  downloadRate: 0,
  uploadRate: 0,
  maxConnections: 200,
  maxHalfOpen: 0,
  connectRate: 0,
  queue: { maxActiveDownloads: 3, maxActiveSeeds: 0, maxActive: 0, ignoreSlow: false, slowDownloadRate: 0, slowUploadRate: 0, slowAfterSecs: 60 },
  groups: [],
  naming: "portable",
  library: {
    enabled: true,
    allowCopy: false,
    partialReuse: false,
    fingerprintRate: 0,
    retireSuperseded: false,
    dedupe: "report",
    folderRescanHours: 24,
    shareRate: 0,
  },
  encryption: "enabled",
  seedLimits: { ratio: 0, seedMinutes: 0, action: "pause" },
  peerPort: 51413,
  dhtPort: 51414,
  discovery: { dht: true, lsd: true, upnp: true, natpmp: true },
  identity: { default: presetIdentity("apitorrent"), rules: [] },
  fileOrder: { by: "off" },
  moveCompleted: { enabled: false, to: "", when: "eachFile", rules: [] },
};

const group = (name: string, labels: string[]) => ({ name, labels, downloadRate: 0, uploadRate: 0, maxConnections: 0, seedLimits: null, fileOrder: null, moveCompleted: null });

describe("settings page model", () => {
  test("every Settings field belongs to exactly one section", () => {
    const owned = SECTIONS.flatMap((s) => s.fields);
    expect(new Set(owned).size).toBe(owned.length);
    expect([...owned].sort()).toEqual(Object.keys(BASE).sort() as (keyof Settings)[]);
  });

  test("unsaved changes are reported by section, in nav order", () => {
    expect(dirtySections(BASE, BASE)).toEqual([]);
    const draft = { ...BASE, encryption: "required" as const, uploadRate: 1024, queue: { ...BASE.queue, ignoreSlow: true } };
    expect(dirtySections(BASE, draft)).toEqual(["transfers", "network"]);
    expect(listTitles(["transfers"])).toBe("Transfers");
    expect(listTitles(["transfers", "network"])).toBe("Transfers and Network");
    expect(listTitles(["transfers", "seeding", "network"])).toBe("Transfers, Seeding and Network");
  });

  test("problems match the daemon's checks and point at their section", () => {
    expect(draftProblems(BASE)).toEqual([]);
    expect(draftProblems({ ...BASE, peerPort: 7000, dhtPort: 7000 })).toEqual([{ section: "network", message: "The peer port and the DHT port must differ." }]);
    // Both automatic is fine.
    expect(draftProblems({ ...BASE, peerPort: 0, dhtPort: 0 })).toEqual([]);
    const groups = [group(" ", ["a"]), group("TV", ["tv"]), group("TV ", ["b"]), group("tv", ["tv"])];
    const problems = draftProblems({ ...BASE, groups });
    expect(problems.every((p) => p.section === "groups")).toBe(true);
    expect(problems.map((p) => p.message)).toEqual([
      "Group 1 needs a name.",
      'Two groups are called "TV".',
      'Label "tv" is in two groups (TV and tv).',
    ]);
    const identity = { default: presetIdentity("apitorrent"), rules: [{ host: "http://x/", identity: presetIdentity("deluge") }] };
    expect(draftProblems({ ...BASE, identity })[0]?.section).toBe("identity");
  });

  test("move destinations must be full paths, in the settings and in groups", () => {
    const moveCompleted = { enabled: true, to: "public", when: "eachFile" as const, rules: [] };
    expect(draftProblems({ ...BASE, moveCompleted })).toEqual([{ section: "files", message: "The destination must be a full path: public" }]);
    // Checked even while off: the daemon refuses it either way.
    expect(draftProblems({ ...BASE, moveCompleted: { ...moveCompleted, enabled: false } })).toHaveLength(1);
    expect(draftProblems({ ...BASE, groups: [{ ...group("TV", ["tv"]), moveCompleted }] })).toEqual([
      { section: "groups", message: "TV: The destination must be a full path: public" },
    ]);
  });

  test("saving trims group names and tracker hosts", () => {
    const s = forSaving({
      ...BASE,
      groups: [group(" TV ", ["tv"])],
      identity: { default: presetIdentity("apitorrent"), rules: [{ host: " t.example ", identity: presetIdentity("deluge") }] },
    });
    expect(s.groups[0]?.name).toBe("TV");
    expect(forSaving({ ...BASE, moveCompleted: { ...BASE.moveCompleted, to: " /srv/public " } }).moveCompleted.to).toBe("/srv/public");
    expect(s.identity.rules[0]?.host).toBe("t.example");
  });

  test("who sees which sections", () => {
    const ids = (a: { serverAdmin: boolean; spaceAdmin: boolean }) => visibleSections(a).map((s) => s.id);
    expect(ids({ serverAdmin: false, spaceAdmin: false })).not.toContain("updates");
    expect(ids({ serverAdmin: false, spaceAdmin: false })).not.toContain("passkeys");
    expect(ids({ serverAdmin: false, spaceAdmin: true })).toContain("passkeys");
    expect(ids({ serverAdmin: true, spaceAdmin: true })).toContain("updates");
  });

  test("the nav filter matches titles, summaries and keywords, every word", () => {
    const ids = (q: string) => filterSections(SECTIONS, q).map((s) => s.id);
    expect(ids("")).toHaveLength(SECTIONS.length);
    // Word starts: "port" finds ports (and portable names), not "import".
    expect(ids("port")).toEqual(["network", "files"]);
    expect(ids("ports dht")).toEqual(["network"]);
    expect(ids("RATIO")).toEqual(["seeding", "groups"]);
    expect(ids("dark")).toEqual(["appearance"]);
    expect(ids("deluge")).toEqual(["identity", "deluge"]);
    expect(ids("deluge import")).toEqual(["deluge"]);
    expect(ids("nothing like this")).toEqual([]);
  });

  test("rates read in the unit that fits, and store whole KiB/s", () => {
    expect(pickRateUnit(0)).toBe("KiB");
    expect(pickRateUnit(500 * 1024)).toBe("KiB");
    expect(pickRateUnit(5 * 1024 * 1024)).toBe("MiB");
    expect(pickRateUnit(1.5 * 1024 * 1024)).toBe("MiB");
    // 1000 KiB/s isn't a round number of MiB/s: stays in KiB/s.
    expect(pickRateUnit(1000 * 1024)).toBe("KiB");
    expect(rateIn(5 * 1024 * 1024, "MiB")).toBe(5);
    expect(rateIn(1536 * 1024, "KiB")).toBe(1536);
    expect(rateFrom(1.5, "MiB")).toBe(1536 * 1024);
    expect(rateFrom(100, "KiB")).toBe(100 * 1024);
    expect(rateFrom(0.4, "KiB")).toBe(1024);
    expect(rateFrom(0, "MiB")).toBe(0);
    expect(parseAmount("")).toBe(0);
    expect(parseAmount(" 2,5 ")).toBe(2.5);
    expect(parseAmount("abc")).toBe(0);
    expect(parseAmount("-3")).toBe(0);
  });

  test("labels typed with commas", () => {
    expect(parseLabels("")).toEqual([]);
    expect(parseLabels("tv,")).toEqual(["tv"]);
    expect(parseLabels(" tv , movies,, tv ")).toEqual(["tv", "movies"]);
  });
});

describe("settings sections render", () => {
  const props = { draft: BASE, saved: BASE, update: () => {}, serverAdmin: true };

  test("transfers: empty limits read as unlimited, the slow rows only when on", () => {
    const html = renderToStaticMarkup(<TransfersSection {...props} />);
    expect(html).toContain('placeholder="Unlimited"');
    expect(html).toContain('value="3"');
    expect(html).not.toContain("Slow while downloading below");
    const on = renderToStaticMarkup(<TransfersSection {...props} draft={{ ...BASE, queue: { ...BASE.queue, ignoreSlow: true } }} />);
    expect(on).toContain("Slow while downloading below");
  });

  test("network: encryption as radio cards, the current one picked", () => {
    const html = renderToStaticMarkup(<NetworkSection {...props} />);
    expect(html).toContain('role="radiogroup"');
    expect(html).toMatch(/class="s-choice picked"><input type="radio"[^>]*checked=""[^>]*value="enabled"/);
    expect(html).toContain('value="51413"');
  });

  test("groups: an empty state, then collapsed cards with their labels", () => {
    expect(renderToStaticMarkup(<GroupsSection {...props} />)).toContain("No groups yet");
    const html = renderToStaticMarkup(<GroupsSection {...props} draft={{ ...BASE, groups: [{ ...group("TV", ["tv", "anime"]), uploadRate: 2 * 1024 * 1024 }] }} />);
    expect(html).toContain('aria-expanded="false"');
    expect(html).toContain('<span class="chip">anime</span>');
    expect(html).toContain("↑ 2.00 MiB/s");
    const own = renderToStaticMarkup(
      <GroupsSection {...props} draft={{ ...BASE, groups: [{ ...group("TV", ["tv"]), fileOrder: { by: "name" }, moveCompleted: { ...BASE.moveCompleted, enabled: true, to: "/srv/tv" } }] }} />,
    );
    expect(own).toContain("order: by name");
    expect(own).toContain("move: to /srv/tv as each file finishes");
  });

  test("files: download order and moving, the move fields only while it is on", () => {
    const html = renderToStaticMarkup(<FilesSection {...props} />);
    expect(html).toContain("Download order");
    expect(html).toMatch(/<option value="off" selected="">No preference<\/option>/);
    expect(html).not.toContain("Move to");
    const on = renderToStaticMarkup(
      <FilesSection {...props} draft={{ ...BASE, moveCompleted: { enabled: true, to: "/srv/public", when: "wholeTorrent", rules: [{ pattern: "*.mkv", to: "" }] } }} />,
    );
    expect(on).toContain('value="/srv/public"');
    expect(on).toContain('aria-label="Rule 1 pattern"');
    expect(on).toMatch(/<option value="wholeTorrent" selected="">/);
  });
});

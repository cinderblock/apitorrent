import { describe, expect, test } from "bun:test";

import { NO_MOVE, describeFileOrder, describeMove, fileOrderOf, isFullPath, moveProblems, moveRule, movesAnything, splitMovedPath, trimMove } from "./fileSettings";

describe("file order", () => {
  test("picking an order keeps a manual list, and drops it for any other", () => {
    const manual = { by: "manual" as const, files: [2, 0, 1] };
    expect(fileOrderOf("manual", manual)).toBe(manual);
    expect(fileOrderOf("manual", { by: "name" })).toEqual({ by: "manual", files: [] });
    expect(fileOrderOf("smallestFirst", manual)).toEqual({ by: "smallestFirst" });
    expect(describeFileOrder({ by: "torrent" })).toBe("As listed in the torrent");
  });
});

describe("moving completed files", () => {
  test("full paths on either OS; relative ones are not", () => {
    for (const p of ["/srv/public", "D:\\public", "d:/public", "\\\\nas\\share", "  /srv  "]) expect(isFullPath(p)).toBe(true);
    for (const p of ["public", "./x", "~/x", "D:x", ""]) expect(isFullPath(p)).toBe(false);
  });

  test("problems match the daemon's checks", () => {
    expect(moveProblems(NO_MOVE)).toEqual([]);
    expect(
      moveProblems({
        enabled: true,
        to: "public",
        when: "eachFile",
        rules: [
          { pattern: " ", to: "/srv/a" },
          { pattern: "*.mkv", to: "video" },
          { pattern: "*.nfo", to: "" },
        ],
      }),
    ).toEqual(["The destination must be a full path: public", "Rule 1 needs a pattern.", "Rule 2's destination must be a full path: video"]);
  });

  test("described by what it does", () => {
    expect(describeMove(NO_MOVE)).toBe("Off");
    expect(describeMove({ ...NO_MOVE, enabled: true })).toBe("On, but no destination is set");
    expect(describeMove({ ...NO_MOVE, enabled: true, to: "/srv/public" })).toBe("To /srv/public as each file finishes");
    const rules = [{ pattern: "*.mkv", to: "/srv/video" }];
    expect(describeMove({ enabled: true, to: "", when: "wholeTorrent", rules })).toBe("Only by its rules, when the whole torrent is done, 1 rule");
    // A rule that keeps files in place moves nothing by itself.
    expect(movesAnything({ ...NO_MOVE, enabled: true, rules: [{ pattern: "*.nfo", to: "" }] })).toBe(false);
  });

  test("rules reorder within range, and everything is trimmed for saving", () => {
    const rules = [
      { pattern: "a", to: "" },
      { pattern: "b", to: "" },
    ];
    expect(moveRule(rules, 1, -1).map((r) => r.pattern)).toEqual(["b", "a"]);
    expect(moveRule(rules, 1, 1)).toBe(rules);
    expect(trimMove({ enabled: true, to: " /x ", when: "eachFile", rules: [{ pattern: " *.mkv ", to: " /v " }] })).toEqual({
      enabled: true,
      to: "/x",
      when: "eachFile",
      rules: [{ pattern: "*.mkv", to: "/v" }],
    });
  });

  test("a moved file's folder and name", () => {
    expect(splitMovedPath("/srv/public/Show/ep1.mkv")).toEqual({ dir: "/srv/public/Show", name: "ep1.mkv" });
    expect(splitMovedPath("/ep1.mkv")).toEqual({ dir: "/", name: "ep1.mkv" });
    expect(splitMovedPath("D:\\public\\ep1.mkv")).toEqual({ dir: "D:\\public", name: "ep1.mkv" });
    expect(splitMovedPath("D:\\ep1.mkv")).toEqual({ dir: "D:\\", name: "ep1.mkv" });
  });
});

import { describe, expect, test } from "bun:test";

import { emptyForm, formOf, settingsOf, slugFrom } from "./form";
import { prefixOf, slugOf, stripPrefix, withPrefix } from "./prefix";

describe("space prefixes", () => {
  test("are read from the path", () => {
    expect(slugOf("/s/tv/rules")).toBe("tv");
    expect(slugOf("/s/tv")).toBe("tv");
    expect(slugOf("/s/a-b2/")).toBe("a-b2");
    expect(slugOf("/rules")).toBeNull();
    expect(slugOf("/s/TV/x")).toBeNull();
    expect(slugOf("/s/-x/")).toBeNull();
    expect(slugOf("/spaces")).toBeNull();
  });

  test("are kept when moving inside a space", () => {
    expect(prefixOf("/s/tv/rules")).toBe("/s/tv");
    expect(stripPrefix("/s/tv/rules")).toBe("/rules");
    expect(stripPrefix("/s/tv")).toBe("/");
    expect(stripPrefix("/plugins")).toBe("/plugins");
    expect(withPrefix("/s/tv/", "/rules")).toBe("/s/tv/rules");
    expect(withPrefix("/s/tv/rules", "/")).toBe("/s/tv/");
    expect(withPrefix("/rules", "/")).toBe("/");
    expect(withPrefix("/s/tv/", "/s/movies/")).toBe("/s/movies/");
  });
});

describe("the space form", () => {
  test("round-trips settings", () => {
    const s = {
      storageRoots: ["/srv/tv", "/mnt/b"],
      downloadDir: "/srv/tv/new",
      folderRules: [{ label: "shows", folder: "shows" }],
      downloadRate: 2048,
      uploadRate: 0,
      maxConnections: 100,
      maxActive: 0,
      maxActiveDownloads: 3,
      maxActiveSeeds: 0,
    };
    expect(settingsOf(formOf("tv", "TV", "", s))).toEqual(s);
  });

  test("refuses folder rules without a label", () => {
    expect(settingsOf({ ...emptyForm(), folderRules: "= /x" })).toBeTypeOf("string");
    expect(settingsOf({ ...emptyForm(), downloadKib: "-3" })).toMatchObject({ downloadRate: 0 });
  });

  test("slugs come from names", () => {
    expect(slugFrom("My TV shows!")).toBe("my-tv-shows");
    expect(slugFrom("  ")).toBe("");
  });
});

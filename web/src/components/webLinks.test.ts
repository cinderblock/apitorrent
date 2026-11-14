import { describe, expect, test } from "bun:test";

import type { StorageRoot } from "../api/generated/StorageRoot";
import { parseServerPath, webBase, webUrl } from "./webLinks";

const root = (path: string, ...locators: string[]): StorageRoot => ({ id: path, path, locators });

describe("webUrl", () => {
  const roots = [root("/srv/downloads", "\\\\nas\\t", "https://files.example.com/path/to/downloads/", "http://lan/dl/")];

  test("files and folders below a root served over the web", () => {
    expect(webUrl(roots, "/srv/downloads", "Show/ep 1#.mkv")).toBe("https://files.example.com/path/to/downloads/Show/ep%201%23.mkv");
    expect(webUrl(roots, "/srv/downloads/tv", "a?.mkv")).toBe("https://files.example.com/path/to/downloads/tv/a%3F.mkv");
    expect(webUrl(roots, "/srv/downloads/tv", "", true)).toBe("https://files.example.com/path/to/downloads/tv/");
    expect(webUrl(roots, "/srv/downloads", "", true)).toBe("https://files.example.com/path/to/downloads/");
    expect(webUrl(roots, "/srv/downloads", "Show", true)).toBe("https://files.example.com/path/to/downloads/Show/");
  });

  test("no link outside a web root, or for unsafe paths", () => {
    expect(webUrl(roots, "/srv/other", "a")).toBeNull();
    expect(webUrl(roots, "/srv/downloadsX", "a")).toBeNull();
    expect(webUrl(roots, "/srv/downloads", "../etc/passwd")).toBeNull();
    expect(webUrl(roots, "/srv/downloads/../x", "a")).toBeNull();
    expect(webUrl(roots, "relative", "a")).toBeNull();
    expect(webUrl([root("/srv/downloads", "smb://nas/t")], "/srv/downloads", "a")).toBeNull();
    expect(webUrl([], "/srv/downloads", "a")).toBeNull();
  });

  test("Windows paths compare case-insensitively and split on either separator", () => {
    const w = [root("D:\\Torrents", "https://f.example/")];
    expect(webUrl(w, "d:\\torrents\\TV", "Show/a\\b.mkv")).toBe("https://f.example/TV/Show/a/b.mkv");
    expect(webUrl(w, "/torrents", "a")).toBeNull();
  });

  test("the innermost root wins", () => {
    const nested = [root("/srv", "https://a.example/"), root("/srv/dl", "https://b.example/x/")];
    expect(webUrl(nested, "/srv/dl/tv", "f")).toBe("https://b.example/x/tv/f");
    expect(webUrl(nested, "/srv/other", "f")).toBe("https://a.example/other/f");
  });
});

test("webBase and parseServerPath", () => {
  expect(webBase("https://f.example/x")).toBe("https://f.example/x/");
  expect(webBase("HTTP://f.example/")).toBe("HTTP://f.example/");
  expect(webBase("nfs://nas/x")).toBeNull();
  expect(parseServerPath("\\\\nas")).toBeNull();
  expect(parseServerPath("\\\\nas\\t\\x")).toEqual({ windows: true, parts: ["nas", "t", "x"] });
});

import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";

import { applyTheme, parseTheme, THEME_KEY, type ThemePreference } from "./theme";

const read = (path: string) => readFileSync(new URL(path, import.meta.url), "utf8");

/** Just enough of a DOM element for the theme code: attributes. */
class FakeElement {
  attrs = new Map<string, string>();
  constructor(attrs: Record<string, string> = {}) {
    for (const [k, v] of Object.entries(attrs)) this.attrs.set(k, v);
  }
  getAttribute(name: string): string | null {
    return this.attrs.get(name) ?? null;
  }
  setAttribute(name: string, value: string): void {
    this.attrs.set(name, value);
  }
  removeAttribute(name: string): void {
    this.attrs.delete(name);
  }
}

/** A document holding index.html's <meta> tags, answering the selectors the theme code uses. */
function fakeDocument() {
  const metas = [...read("../index.html").matchAll(/<meta\s([^>]*?)\s*\/?>/g)].map(
    (m) => new FakeElement(Object.fromEntries([...(m[1] ?? "").matchAll(/([\w-]+)="([^"]*)"/g)].map((a) => [a[1], a[2]]))),
  );
  const named = (name: string) => metas.filter((m) => m.getAttribute("name") === name);
  const select = (selector: string): FakeElement[] => {
    switch (selector) {
      case 'meta[name="color-scheme"]':
        return named("color-scheme");
      case 'meta[name="theme-color"][data-scheme]':
        return named("theme-color").filter((m) => m.getAttribute("data-scheme") !== null);
      default:
        throw new Error(`unexpected selector ${selector}`);
    }
  };
  return {
    documentElement: new FakeElement(),
    querySelector: (s: string) => select(s)[0] ?? null,
    querySelectorAll: (s: string) => select(s),
  };
}

type FakeDocument = ReturnType<typeof fakeDocument>;

/** What the theme code controls, for comparing. */
function snapshot(doc: FakeDocument) {
  return {
    dataTheme: doc.documentElement.getAttribute("data-theme"),
    colorScheme: doc.querySelector('meta[name="color-scheme"]')?.getAttribute("content"),
    bars: doc.querySelectorAll('meta[name="theme-color"][data-scheme]').map((m) => [
      m.getAttribute("data-scheme"),
      m.getAttribute("media"),
    ]),
  };
}

/** Run public/theme-boot.js against `doc` with `stored` in localStorage. */
function boot(doc: FakeDocument, stored: string | null) {
  const localStorage = { getItem: (k: string) => (k === THEME_KEY ? stored : null) };
  new Function("document", "localStorage", read("../public/theme-boot.js"))(doc, localStorage);
}

describe("theme", () => {
  test("parseTheme accepts only the explicit themes, else follows the system", () => {
    expect(parseTheme("light")).toBe("light");
    expect(parseTheme("dark")).toBe("dark");
    expect(parseTheme("system")).toBe("system");
    expect(parseTheme("purple")).toBe("system");
    expect(parseTheme(null)).toBe("system");
  });

  test("index.html has the metas both scripts drive", () => {
    const s = snapshot(fakeDocument());
    expect(s.colorScheme).toBe("light dark");
    expect(s.bars).toEqual([
      ["light", "(prefers-color-scheme: light)"],
      ["dark", "(prefers-color-scheme: dark)"],
    ]);
  });

  test("applyTheme pins a chosen theme and restores the system one", () => {
    const doc = fakeDocument();
    const initial = snapshot(doc);
    applyTheme("dark", doc as unknown as Document);
    expect(snapshot(doc)).toEqual({
      dataTheme: "dark",
      colorScheme: "dark",
      bars: [
        ["light", "not all"],
        ["dark", "all"],
      ],
    });
    applyTheme("system", doc as unknown as Document);
    expect(snapshot(doc)).toEqual(initial);
  });

  // theme-boot.js runs before the bundle, so it can't share code with theme.ts: hold the two
  // to the same result instead.
  for (const stored of ["light", "dark", null, "nonsense"]) {
    test(`theme-boot.js matches applyTheme for ${stored}`, () => {
      const booted = fakeDocument();
      boot(booted, stored);
      const applied = fakeDocument();
      applyTheme(parseTheme(stored) as ThemePreference, applied as unknown as Document);
      expect(snapshot(booted)).toEqual(snapshot(applied));
    });
  }

  test("theme-boot.js survives blocked storage", () => {
    const doc = fakeDocument();
    const localStorage = {
      getItem: () => {
        throw new Error("SecurityError");
      },
    };
    new Function("document", "localStorage", read("../public/theme-boot.js"))(doc, localStorage);
    expect(snapshot(doc).dataTheme).toBeNull();
  });

  test("the shell links theme.css and the boot script in <head>, ahead of the app", () => {
    const html = read("../index.html");
    const head = html.slice(0, html.indexOf("</head>"));
    expect(head).toContain('<script src="/theme-boot.js"></script>');
    expect(head).toContain('<link rel="stylesheet" href="/src/theme.css" />');
    expect(head).not.toContain("<style");
  });
});

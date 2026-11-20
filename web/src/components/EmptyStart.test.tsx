import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";

import { EmptyStart, loadingCount, loadingText } from "./EmptyStart";

const noop = () => {};
const render = (mayWrite: boolean, mayImport: boolean, narrow = false) =>
  renderToStaticMarkup(<EmptyStart mayWrite={mayWrite} mayImport={mayImport} narrow={narrow} onAdd={noop} onImport={noop} />);

describe("an empty torrent list", () => {
  test("a space admin is offered Add and the Deluge import", () => {
    const html = render(true, true);
    expect(html).toContain("Add a torrent…");
    expect(html).toContain("Import from Deluge…");
    expect(html).toContain("Drop a .torrent file");
  });

  test("a member who can't import gets Add only", () => {
    const html = render(true, false);
    expect(html).toContain("Add a torrent…");
    expect(html).not.toContain("Deluge");
  });

  test("a viewer gets no buttons", () => {
    expect(render(false, true)).not.toContain("<button");
  });

  test("a phone isn't told to drop files", () => {
    expect(render(true, true, true)).not.toContain("Drop");
  });
});

describe("loading in parts", () => {
  test("counts what has come", () => {
    expect(loadingText({ received: 1100, total: 9800 })).toBe(`Loading ${(1100).toLocaleString()} of ${(9800).toLocaleString()} torrents…`);
    expect(loadingCount({ received: 1100, total: 9800 })).toBe("Loading 11%");
  });
});

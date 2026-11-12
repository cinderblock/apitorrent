import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";
import { Markdown, safeHref } from "./markdown";

const html = (text: string) => renderToStaticMarkup(<Markdown text={text} />);

describe("release notes Markdown", () => {
  test("GitHub's generated notes", () => {
    const out = html(
      "## What's Changed\r\n* Spaces by @someone in https://github.com/o/r/pull/1\r\n* **Faster** checks\r\n\r\n**Full Changelog**: https://github.com/o/r/compare/v0.1.0...v0.2.0",
    );
    expect(out).toContain("<h4>What&#x27;s Changed</h4>");
    expect(out).toContain("<ul><li>Spaces by @someone in <a href=\"https://github.com/o/r/pull/1\"");
    expect(out).toContain("<li><strong>Faster</strong> checks</li>");
    expect(out).toContain('<p><strong>Full Changelog</strong>: <a href="https://github.com/o/r/compare/v0.1.0...v0.2.0"');
  });

  test("markup in the text stays text", () => {
    const out = html('<img src=x onerror="alert(1)"> <script>alert(1)</script>');
    expect(out).not.toContain("<img");
    expect(out).not.toContain("<script");
    expect(out).toContain("&lt;script&gt;");
  });

  test("only http(s) links are followed", () => {
    expect(html("[click](javascript:alert(1))")).not.toContain("<a");
    expect(html("[x](data:text/html,hi)")).not.toContain("<a");
    expect(safeHref("javascript:alert(1)")).toBeNull();
    const out = html("[docs](https://example.com/a)");
    expect(out).toContain('<a href="https://example.com/a" target="_blank" rel="noopener noreferrer">docs</a>');
  });

  test("code, numbered lists, fences and trailing punctuation", () => {
    const out = html("1. run `apitorrentd update`\n2. done\n\n```\na <b>\n```\nSee https://example.com.");
    expect(out).toContain("<ol><li>run <code>apitorrentd update</code></li><li>done</li></ol>");
    expect(out).toContain("<pre><code>a &lt;b&gt;</code></pre>");
    expect(out).toContain('<a href="https://example.com/" target="_blank" rel="noopener noreferrer">https://example.com</a>.');
  });
});

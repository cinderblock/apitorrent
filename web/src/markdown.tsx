import type { ReactNode } from "react";

/**
 * A small Markdown renderer for release notes. It builds React elements, never HTML
 * strings, so nothing in the text can inject markup or script. Supported: headings,
 * paragraphs, bullet and numbered lists, fenced code, `code`, **bold**, *italic*,
 * [links](https://…) and bare https URLs. Links go only to http(s) and open in a new tab.
 * Anything else shows as plain text.
 */
export function Markdown({ text, className }: { text: string; className?: string }) {
  return <div className={className}>{blocks(text)}</div>;
}

function blocks(text: string): ReactNode[] {
  const lines = text.replace(/\r\n?/g, "\n").split("\n");
  const at = (n: number) => lines[n] ?? "";
  const more = (n: number) => n < lines.length;
  const bullet = /^\s*[-*+]\s+/;
  const numbered = /^\s*\d+[.)]\s+/;
  const out: ReactNode[] = [];
  let i = 0;
  while (more(i)) {
    const line = at(i);
    if (line.trim() === "") {
      i++;
      continue;
    }
    const fence = /^\s*(```|~~~)/.exec(line)?.[1];
    if (fence) {
      const body: string[] = [];
      i++;
      while (more(i) && !at(i).trimStart().startsWith(fence)) body.push(at(i++));
      i++; // the closing fence, if any
      out.push(
        <pre key={out.length}>
          <code>{body.join("\n")}</code>
        </pre>,
      );
      continue;
    }
    const heading = /^(#{1,6})\s+(.*?)\s*#*\s*$/.exec(line);
    if (heading) {
      // Release notes sit inside a dialog: headings start at h3.
      const level = Math.min((heading[1] ?? "#").length + 2, 6);
      const Tag = `h${level}` as "h3";
      out.push(<Tag key={out.length}>{inline(heading[2] ?? "")}</Tag>);
      i++;
      continue;
    }
    if (bullet.test(line) || numbered.test(line)) {
      const marker = bullet.test(line) ? bullet : numbered;
      const items: string[] = [];
      while (more(i) && marker.test(at(i))) {
        let item = at(i++).replace(marker, "");
        // Indented continuation lines belong to the item.
        while (more(i) && /^\s{2,}\S/.test(at(i)) && !marker.test(at(i))) {
          item += ` ${at(i++).trim()}`;
        }
        items.push(item);
      }
      const children = items.map((item, n) => <li key={n}>{inline(item)}</li>);
      out.push(marker === bullet ? <ul key={out.length}>{children}</ul> : <ol key={out.length}>{children}</ol>);
      continue;
    }
    const para: string[] = [];
    while (
      more(i) &&
      at(i).trim() !== "" &&
      !/^\s*(```|~~~|#{1,6}\s)/.test(at(i)) &&
      !bullet.test(at(i)) &&
      !numbered.test(at(i))
    ) {
      para.push(at(i++).trim());
    }
    out.push(<p key={out.length}>{inline(para.join(" "))}</p>);
  }
  return out;
}

/** Only http(s) links are followed; anything else (javascript:, data:, …) stays text. */
export function safeHref(url: string): string | null {
  try {
    const u = new URL(url);
    return u.protocol === "https:" || u.protocol === "http:" ? u.href : null;
  } catch {
    return null;
  }
}

const INLINE = /(`[^`]+`)|(\*\*[^*]+\*\*|__[^_]+__)|(\*[^*\s][^*]*\*|_[^_\s][^_]*_)|(\[[^\]]+\]\([^)\s]+\))|(https?:\/\/[^\s<>()]+[^\s<>().,;:!?'"])/g;

function inline(text: string): ReactNode[] {
  const out: ReactNode[] = [];
  let last = 0;
  for (const m of text.matchAll(INLINE)) {
    const at = m.index ?? 0;
    if (at > last) out.push(text.slice(last, at));
    const s = m[0];
    const key = out.length;
    if (m[1]) out.push(<code key={key}>{s.slice(1, -1)}</code>);
    else if (m[2]) out.push(<strong key={key}>{inline(s.slice(2, -2))}</strong>);
    else if (m[3]) out.push(<em key={key}>{inline(s.slice(1, -1))}</em>);
    else if (m[4]) {
      const split = s.indexOf("](");
      const label = s.slice(1, split);
      const href = safeHref(s.slice(split + 2, -1));
      out.push(href ? link(key, href, inline(label)) : label);
    } else {
      const href = safeHref(s);
      out.push(href ? link(key, href, s) : s);
    }
    last = at + s.length;
  }
  if (last < text.length) out.push(text.slice(last));
  return out;
}

function link(key: number, href: string, children: ReactNode) {
  return (
    <a key={key} href={href} target="_blank" rel="noopener noreferrer">
      {children}
    </a>
  );
}

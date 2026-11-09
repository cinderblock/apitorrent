/**
 * A small, forgiving XML parser: enough for RSS and Atom, without a DOM. It keeps
 * namespace prefixes in names (`torrent:magnetURI`), decodes entities and CDATA, skips
 * comments, processing instructions and DOCTYPEs, and recovers from mismatched tags the
 * way feeds in the wild need.
 */

export interface XmlElement {
  name: string;
  attrs: Record<string, string>;
  children: XmlElement[];
  /** The element's own text (CDATA included), not its descendants'. */
  text: string;
}

const NAMED: Record<string, string> = { amp: "&", lt: "<", gt: ">", quot: '"', apos: "'", nbsp: " " };

export function decodeEntities(s: string): string {
  return s.replace(/&(#x[0-9a-fA-F]+|#[0-9]+|[A-Za-z]+);/g, (m, e: string) => {
    if (e[0] === "#") {
      const n = e[1] === "x" || e[1] === "X" ? parseInt(e.slice(2), 16) : parseInt(e.slice(1), 10);
      return Number.isFinite(n) && n > 0 && n <= 0x10ffff ? String.fromCodePoint(n) : m;
    }
    return NAMED[e] ?? m;
  });
}

const ATTR = /([^\s=/>]+)\s*(?:=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+)))?/g;

function parseAttrs(s: string): Record<string, string> {
  const attrs: Record<string, string> = {};
  for (const m of s.matchAll(ATTR)) {
    attrs[m[1]!] = decodeEntities(m[2] ?? m[3] ?? m[4] ?? "");
  }
  return attrs;
}

/** Parse a document; returns its root element, or null if there is none. */
export function parseXml(input: string): XmlElement | null {
  const root: XmlElement = { name: "#document", attrs: {}, children: [], text: "" };
  const stack: XmlElement[] = [root];
  let i = 0;
  const top = () => stack[stack.length - 1]!;
  while (i < input.length) {
    const lt = input.indexOf("<", i);
    if (lt < 0) {
      top().text += decodeEntities(input.slice(i));
      break;
    }
    if (lt > i) top().text += decodeEntities(input.slice(i, lt));
    if (input.startsWith("<!--", lt)) {
      const end = input.indexOf("-->", lt + 4);
      i = end < 0 ? input.length : end + 3;
    } else if (input.startsWith("<![CDATA[", lt)) {
      const end = input.indexOf("]]>", lt + 9);
      top().text += input.slice(lt + 9, end < 0 ? input.length : end);
      i = end < 0 ? input.length : end + 3;
    } else if (input.startsWith("<?", lt)) {
      const end = input.indexOf("?>", lt + 2);
      i = end < 0 ? input.length : end + 2;
    } else if (input.startsWith("<!", lt)) {
      // DOCTYPE, possibly with an internal subset in brackets.
      let depth = 0;
      let j = lt + 2;
      for (; j < input.length; j++) {
        const c = input[j];
        if (c === "[") depth++;
        else if (c === "]") depth--;
        else if (c === ">" && depth <= 0) break;
      }
      i = j + 1;
    } else if (input[lt + 1] === "/") {
      const end = input.indexOf(">", lt);
      const name = input.slice(lt + 2, end < 0 ? input.length : end).trim();
      // Close the nearest open element of this name; ignore a stray closing tag.
      const at = stack.map((e) => e.name).lastIndexOf(name);
      if (at > 0) stack.length = at;
      i = end < 0 ? input.length : end + 1;
    } else {
      // A start tag. Find its end, minding quotes in attribute values.
      let j = lt + 1;
      let quote: string | null = null;
      for (; j < input.length; j++) {
        const c = input[j];
        if (quote) {
          if (c === quote) quote = null;
        } else if (c === '"' || c === "'") quote = c;
        else if (c === ">") break;
      }
      const raw = input.slice(lt + 1, j);
      const selfClosing = raw.endsWith("/");
      const body = selfClosing ? raw.slice(0, -1) : raw;
      const nameEnd = body.search(/[\s/]|$/);
      const el: XmlElement = {
        name: body.slice(0, nameEnd),
        attrs: parseAttrs(body.slice(nameEnd)),
        children: [],
        text: "",
      };
      top().children.push(el);
      if (!selfClosing) stack.push(el);
      i = j + 1;
    }
  }
  return root.children[0] ?? null;
}

export function child(el: XmlElement | undefined, name: string): XmlElement | undefined {
  return el?.children.find((c) => c.name === name);
}

export function childrenNamed(el: XmlElement | undefined, name: string): XmlElement[] {
  return el?.children.filter((c) => c.name === name) ?? [];
}

/** The trimmed text of the first child with one of these names. */
export function textOf(el: XmlElement | undefined, ...names: string[]): string | undefined {
  for (const n of names) {
    const t = child(el, n)?.text.trim();
    if (t) return t;
  }
  return undefined;
}

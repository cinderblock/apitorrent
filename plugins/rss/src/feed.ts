/**
 * Read RSS 2.0, RSS 1.0 (RDF), Atom and JSON Feed into one item shape, finding each
 * item's torrent: a magnet link (in the link, an enclosure, `torrent:magnetURI`, or built
 * from an info-hash element) or a `.torrent` URL (an enclosure, or the link).
 */

import { child, childrenNamed, parseXml, textOf, type XmlElement } from "./xml";

export interface FeedItem {
  /** Stable identity for dedupe: the guid or id, else the link, else title and date. */
  guid: string;
  title: string;
  link?: string;
  /** As the feed gave it. */
  published?: string;
  magnet?: string;
  torrentUrl?: string;
  size?: number;
}

export interface Feed {
  title?: string;
  items: FeedItem[];
}

const TORRENT_TYPE = "application/x-bittorrent";
const HASH = /^([0-9a-fA-F]{40}|[A-Za-z2-7]{32})$/;

function magnetFor(hash: string, title: string): string {
  return `magnet:?xt=urn:btih:${hash.toLowerCase()}&dn=${encodeURIComponent(title)}`;
}

function isTorrentUrl(url: string, type?: string): boolean {
  if (type?.toLowerCase() === TORRENT_TYPE) return true;
  const path = URL.parse(url)?.pathname ?? url;
  return path.toLowerCase().endsWith(".torrent");
}

function finish(partial: Omit<FeedItem, "guid"> & { guid?: string; hash?: string }): FeedItem {
  const { hash, ...item } = partial;
  if (!item.magnet && hash && HASH.test(hash)) item.magnet = magnetFor(hash, item.title);
  if (!item.magnet && item.link?.startsWith("magnet:")) item.magnet = item.link;
  if (!item.torrentUrl && item.link && !item.link.startsWith("magnet:") && isTorrentUrl(item.link)) {
    item.torrentUrl = item.link;
  }
  const guid = item.guid || item.link || item.magnet || item.torrentUrl ||
    `${item.title}|${item.published ?? ""}`;
  return { ...item, guid };
}

function rssItem(el: XmlElement): FeedItem {
  const title = textOf(el, "title") ?? "(untitled)";
  const link = textOf(el, "link");
  let magnet = textOf(el, "torrent:magnetURI");
  let torrentUrl: string | undefined;
  let size: number | undefined;
  for (const enc of childrenNamed(el, "enclosure")) {
    const url = enc.attrs.url?.trim();
    if (!url) continue;
    if (url.startsWith("magnet:")) magnet ??= url;
    else if (isTorrentUrl(url, enc.attrs.type)) {
      torrentUrl ??= url;
      const n = Number(enc.attrs.length);
      if (Number.isFinite(n) && n > 0) size = n;
    }
  }
  const sizeText = textOf(el, "torrent:contentLength", "nyaa:size", "size");
  if (size === undefined && sizeText && /^\d+$/.test(sizeText)) size = Number(sizeText);
  return finish({
    guid: textOf(el, "guid"),
    title,
    link,
    published: textOf(el, "pubDate", "dc:date"),
    magnet,
    torrentUrl,
    size,
    hash: textOf(el, "torrent:infoHash", "nyaa:infoHash", "infohash"),
  });
}

function atomEntry(el: XmlElement): FeedItem {
  const title = textOf(el, "title") ?? "(untitled)";
  let link: string | undefined;
  let magnet: string | undefined;
  let torrentUrl: string | undefined;
  let size: number | undefined;
  for (const l of childrenNamed(el, "link")) {
    const href = l.attrs.href?.trim();
    if (!href) continue;
    const rel = l.attrs.rel ?? "alternate";
    if (href.startsWith("magnet:")) magnet ??= href;
    else if (rel === "enclosure" && isTorrentUrl(href, l.attrs.type)) {
      torrentUrl ??= href;
      const n = Number(l.attrs.length);
      if (Number.isFinite(n) && n > 0) size = n;
    } else if (rel === "alternate") link ??= href;
  }
  return finish({
    guid: textOf(el, "id"),
    title,
    link,
    published: textOf(el, "updated", "published"),
    magnet,
    torrentUrl,
    size,
    hash: textOf(el, "torrent:infoHash"),
  });
}

interface JsonFeedItem {
  id?: unknown;
  title?: unknown;
  url?: unknown;
  date_published?: unknown;
  attachments?: { url?: unknown; mime_type?: unknown; size_in_bytes?: unknown }[];
}

function jsonItem(it: JsonFeedItem): FeedItem {
  const str = (v: unknown) => (typeof v === "string" && v.trim() ? v.trim() : undefined);
  let magnet: string | undefined;
  let torrentUrl: string | undefined;
  let size: number | undefined;
  for (const a of Array.isArray(it.attachments) ? it.attachments : []) {
    const url = str(a.url);
    if (!url) continue;
    if (url.startsWith("magnet:")) magnet ??= url;
    else if (isTorrentUrl(url, str(a.mime_type))) {
      torrentUrl ??= url;
      if (typeof a.size_in_bytes === "number") size = a.size_in_bytes;
    }
  }
  return finish({
    guid: str(it.id) ?? (typeof it.id === "number" ? String(it.id) : undefined),
    title: str(it.title) ?? "(untitled)",
    link: str(it.url),
    published: str(it.date_published),
    magnet,
    torrentUrl,
    size,
  });
}

export function parseFeed(text: string): Feed {
  const trimmed = text.trimStart();
  if (trimmed.startsWith("{")) {
    const doc = JSON.parse(trimmed) as { title?: unknown; items?: unknown };
    if (!Array.isArray(doc.items)) throw new Error("a JSON feed needs an items list");
    return {
      title: typeof doc.title === "string" ? doc.title : undefined,
      items: (doc.items as JsonFeedItem[]).map(jsonItem),
    };
  }
  const root = parseXml(text);
  if (!root) throw new Error("not an RSS, Atom or JSON feed");
  switch (root.name) {
    case "rss": {
      const channel = child(root, "channel");
      return { title: textOf(channel, "title"), items: childrenNamed(channel, "item").map(rssItem) };
    }
    case "rdf:RDF":
      return { title: textOf(child(root, "channel"), "title"), items: childrenNamed(root, "item").map(rssItem) };
    case "feed":
      return { title: textOf(root, "title"), items: childrenNamed(root, "entry").map(atomEntry) };
    default:
      throw new Error(`not an RSS, Atom or JSON feed (root element <${root.name}>)`);
  }
}

/** Decode a feed's bytes: the XML declaration's encoding, else the HTTP charset, else UTF-8. */
export function decodeFeed(bytes: Uint8Array, contentType: string | null): string {
  const head = new TextDecoder("latin1").decode(bytes.subarray(0, 256));
  const declared = /<\?xml[^>]*encoding=["']([^"']+)["']/i.exec(head)?.[1];
  const charset = /charset=["']?([^;"'\s]+)/i.exec(contentType ?? "")?.[1];
  for (const label of [declared, charset, "utf-8"]) {
    if (!label) continue;
    try {
      return new TextDecoder(label).decode(bytes);
    } catch {
      // Unknown label: try the next.
    }
  }
  return new TextDecoder().decode(bytes);
}

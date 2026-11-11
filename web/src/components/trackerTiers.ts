// Editing an entry's tracker list: tiers as text boxes (one announce URL per line), which
// the user can add, remove and reorder before saving.

export type TierDraft = { key: number; text: string };

let nextKey = 0;

export function newTier(text = ""): TierDraft {
  nextKey += 1;
  return { key: nextKey, text };
}

export function toDrafts(tiers: string[][]): TierDraft[] {
  return tiers.map((urls) => newTier(urls.join("\n")));
}

/** The tiers to save: lines trimmed, blank lines and empty tiers dropped. */
export function fromDrafts(drafts: TierDraft[]): string[][] {
  return drafts
    .map((d) =>
      d.text
        .split(/\r?\n/)
        .map((l) => l.trim())
        .filter((l) => l !== ""),
    )
    .filter((urls) => urls.length > 0);
}

/** Move the tier at `index` up (`-1`) or down (`+1`); out of range leaves it. */
export function moveTier(drafts: TierDraft[], index: number, by: -1 | 1): TierDraft[] {
  const to = index + by;
  if (index < 0 || index >= drafts.length || to < 0 || to >= drafts.length) return drafts;
  const out = [...drafts];
  out.splice(to, 0, ...out.splice(index, 1));
  return out;
}

/** URLs listed more than once (the server refuses those), in first-seen order. */
export function duplicateUrls(tiers: string[][]): string[] {
  const seen = new Set<string>();
  const twice: string[] = [];
  for (const url of tiers.flat()) {
    if (seen.has(url) && !twice.includes(url)) twice.push(url);
    seen.add(url);
  }
  return twice;
}

/** Lines that aren't http, https or udp URLs (the server refuses those). */
export function badUrls(tiers: string[][]): string[] {
  return tiers.flat().filter((u) => {
    try {
      const url = new URL(u);
      return !["http:", "https:", "udp:"].includes(url.protocol) || url.hostname === "";
    } catch {
      return true;
    }
  });
}

export function sameTiers(a: string[][], b: string[][]): boolean {
  return a.length === b.length && a.every((t, i) => t.length === b[i]?.length && t.every((u, j) => u === b[i]?.[j]));
}

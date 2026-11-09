/** Which table columns show, and in what order. Remembered per browser. */

export interface ColumnPrefs {
  /** Every column id, in display order (hidden ones keep their place). */
  order: string[];
  /** Ids turned off. */
  hidden: string[];
}

export interface ColumnInfo {
  id: string;
  /** Shown unless the user turns it off. */
  defaultVisible: boolean;
  /** Can't be turned off (the name column). */
  required?: boolean;
}

export function defaultPrefs(all: readonly ColumnInfo[]): ColumnPrefs {
  return { order: all.map((c) => c.id), hidden: all.filter((c) => !c.defaultVisible && !c.required).map((c) => c.id) };
}

/**
 * Saved prefs made to fit the columns this build has: unknown and duplicate ids drop out,
 * and a column added since the prefs were saved goes after the column that precedes it in
 * the default order, with its default visibility. So an upgrade never hides a new column
 * by accident, or reshuffles the old ones.
 */
export function normalize(all: readonly ColumnInfo[], prefs: ColumnPrefs | null): ColumnPrefs {
  if (!prefs) return defaultPrefs(all);
  const byId = new Map(all.map((c) => [c.id, c]));
  const order = prefs.order.filter((id, i) => byId.has(id) && prefs.order.indexOf(id) === i);
  const hidden = new Set(prefs.hidden.filter((id) => byId.has(id) && !byId.get(id)!.required));
  all.forEach((c, i) => {
    if (order.includes(c.id)) return;
    const after = i === 0 ? -1 : order.indexOf(all[i - 1]!.id);
    order.splice(after + 1, 0, c.id);
    if (!c.defaultVisible && !c.required) hidden.add(c.id);
  });
  return { order, hidden: order.filter((id) => hidden.has(id)) };
}

/** The ids to show, in order. */
export function visibleColumns(all: readonly ColumnInfo[], prefs: ColumnPrefs | null): string[] {
  const p = normalize(all, prefs);
  return p.order.filter((id) => !p.hidden.includes(id));
}

/** Show or hide one column. Required columns stay. */
export function toggleColumn(all: readonly ColumnInfo[], prefs: ColumnPrefs | null, id: string): ColumnPrefs {
  const p = normalize(all, prefs);
  if (all.find((c) => c.id === id)?.required) return p;
  const hidden = p.hidden.includes(id) ? p.hidden.filter((x) => x !== id) : [...p.hidden, id];
  return { order: p.order, hidden: p.order.filter((x) => hidden.includes(x)) };
}

/** Move a column past its visible neighbour: left (-1) or right (+1). */
export function moveColumn(all: readonly ColumnInfo[], prefs: ColumnPrefs | null, id: string, by: -1 | 1): ColumnPrefs {
  const p = normalize(all, prefs);
  const visible = p.order.filter((x) => !p.hidden.includes(x));
  const i = visible.indexOf(id);
  const neighbour = visible[i + by];
  if (i < 0 || neighbour === undefined) return p;
  const order = [...p.order];
  const a = order.indexOf(id);
  const b = order.indexOf(neighbour);
  [order[a], order[b]] = [order[b]!, order[a]!];
  return { order, hidden: p.hidden };
}

const KEY = "apitorrent.columns.v1";

/** Saved prefs, or null. Storage can be missing or blocked (private windows). */
export function loadColumnPrefs(): ColumnPrefs | null {
  try {
    const raw = localStorage.getItem(KEY);
    if (!raw) return null;
    const v = JSON.parse(raw) as Partial<ColumnPrefs> | null;
    const strings = (x: unknown): x is string[] => Array.isArray(x) && x.every((s) => typeof s === "string");
    if (v && strings(v.order) && strings(v.hidden)) return { order: v.order, hidden: v.hidden };
  } catch {
    // Unreadable or blocked: use the defaults.
  }
  return null;
}

/** Remember prefs; null forgets them. */
export function saveColumnPrefs(prefs: ColumnPrefs | null): void {
  try {
    if (prefs) localStorage.setItem(KEY, JSON.stringify(prefs));
    else localStorage.removeItem(KEY);
  } catch {
    // Not remembered; the choice still applies until the page reloads.
  }
}

// File order and moving completed files: the words for each setting, the checks the daemon
// makes before saving one (`files::validate_move`), and small list edits. No React here.

import type { FileOrder } from "../api/generated/FileOrder";
import type { MoveCompleted } from "../api/generated/MoveCompleted";
import type { MoveRule } from "../api/generated/MoveRule";
import type { MoveWhen } from "../api/generated/MoveWhen";

export type FileOrderBy = FileOrder["by"];

/** The orders anyone can pick. "manual" is a torrent's own list, set elsewhere. */
export const FILE_ORDERS: ReadonlyArray<[Exclude<FileOrderBy, "manual">, string]> = [
  ["off", "No preference"],
  ["name", "By name"],
  ["smallestFirst", "Smallest first"],
  ["largestFirst", "Largest first"],
  ["torrent", "As listed in the torrent"],
];

const ORDER_WORDS: Record<FileOrderBy, string> = {
  off: "No preference",
  name: "By name",
  smallestFirst: "Smallest first",
  largestFirst: "Largest first",
  torrent: "As listed in the torrent",
  manual: "Its own list",
};

export function describeFileOrder(o: FileOrder): string {
  return ORDER_WORDS[o.by];
}

/** The order for a picked `by` (a manual order keeps its list when it is picked again). */
export function fileOrderOf(by: FileOrderBy, current?: FileOrder | null): FileOrder {
  if (by === "manual") return current?.by === "manual" ? current : { by: "manual", files: [] };
  return { by };
}

export const NO_MOVE: MoveCompleted = { enabled: false, to: "", when: "eachFile", rules: [] };

export const MOVE_WHEN: ReadonlyArray<[MoveWhen, string]> = [
  ["eachFile", "As each file finishes"],
  ["wholeTorrent", "When the whole torrent is done"],
];

/** A path the daemon accepts as full: `/srv/x`, `D:\x`, `D:/x` or `\\nas\x`. The daemon
    checks it for its own OS; this only catches the obvious mistakes first. */
export function isFullPath(p: string): boolean {
  return /^(\/|[A-Za-z]:[\\/]|\\\\)/.test(p.trim());
}

/** What the daemon would refuse in a move setting, said plainly. */
export function moveProblems(m: MoveCompleted): string[] {
  const out: string[] = [];
  const to = m.to.trim();
  if (to !== "" && !isFullPath(to)) out.push(`The destination must be a full path: ${to}`);
  m.rules.forEach((r, i) => {
    const pattern = r.pattern.trim();
    if (pattern === "") out.push(`Rule ${i + 1} needs a pattern.`);
    const rto = r.to.trim();
    if (rto !== "" && !isFullPath(rto)) out.push(`Rule ${i + 1}'s destination must be a full path: ${rto}`);
  });
  return out;
}

/** The setting as it is sent: paths and patterns trimmed. */
export function trimMove(m: MoveCompleted): MoveCompleted {
  return { ...m, to: m.to.trim(), rules: m.rules.map((r) => ({ pattern: r.pattern.trim(), to: r.to.trim() })) };
}

/** Whether it moves anything at all (the daemon's `MoveCompleted::active`). */
export function movesAnything(m: MoveCompleted): boolean {
  return m.enabled && (m.to.trim() !== "" || m.rules.some((r) => r.to.trim() !== ""));
}

function plural(n: number, one: string, many: string): string {
  return `${n} ${n === 1 ? one : many}`;
}

/** "Off", "To /srv/public as each file finishes, 2 rules", "Only by its rules (1), …". */
export function describeMove(m: MoveCompleted): string {
  if (!m.enabled) return "Off";
  if (!movesAnything(m)) return "On, but no destination is set";
  const when = m.when === "eachFile" ? "as each file finishes" : "when the whole torrent is done";
  const rules = m.rules.length > 0 ? `, ${plural(m.rules.length, "rule", "rules")}` : "";
  const to = m.to.trim();
  return to !== "" ? `To ${to} ${when}${rules}` : `Only by its rules, ${when}${rules}`;
}

/** Move the rule at `index` up (`-1`) or down (`+1`); out of range leaves the list. */
export function moveRule(rules: MoveRule[], index: number, by: -1 | 1): MoveRule[] {
  const to = index + by;
  if (index < 0 || index >= rules.length || to < 0 || to >= rules.length) return rules;
  const out = [...rules];
  out.splice(to, 0, ...out.splice(index, 1));
  return out;
}

/** A moved file's folder and name, for the links that open it where it now is. */
export function splitMovedPath(p: string): { dir: string; name: string } {
  const cut = Math.max(p.lastIndexOf("/"), p.lastIndexOf("\\"));
  if (cut < 0) return { dir: "", name: p };
  // Keep a root's separator: "/a" is "/" and "a", "D:\a" is "D:\" and "a".
  const dir = p.slice(0, cut) === "" || /^[A-Za-z]:$/.test(p.slice(0, cut)) ? p.slice(0, cut + 1) : p.slice(0, cut);
  return { dir, name: p.slice(cut + 1) };
}

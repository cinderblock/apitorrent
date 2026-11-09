// Pure helpers for the Rules page: a new rule's defaults, list fields edited as lines, and
// plain-language descriptions of rules, verdicts and item states.

import type { DryRunVerdict } from "../api/generated/DryRunVerdict";
import type { MatchMode } from "../api/generated/MatchMode";
import type { RuleItemState } from "../api/generated/RuleItemState";
import type { RuleSpec } from "../api/generated/RuleSpec";
import type { SupersededAction } from "../api/generated/SupersededAction";
import type { TextMatch } from "../api/generated/TextMatch";
import type { VersionScheme } from "../api/generated/VersionScheme";
import { describeSeedLimits } from "../components/SeedLimitsFields";

export function newRule(): RuleSpec {
  return {
    name: "",
    enabled: true,
    match: { sources: [], title: null, excludeTitle: null, metadata: [] },
    order: { scheme: "semver", field: "version" },
    keepLatest: 1,
    label: null,
    savePath: null,
    files: { include: [], exclude: [] },
    superseded: { action: "keepSeeding" },
    paused: false,
    alias: null,
  };
}

/** One entry per non-empty line, trimmed. */
export function lines(text: string): string[] {
  return text
    .split(/\r?\n/)
    .map((l) => l.trim())
    .filter((l) => l.length > 0);
}

/** A text match from a pattern box: none when the pattern is empty. */
export function textMatch(mode: MatchMode, pattern: string): TextMatch | null {
  return pattern.trim() ? { mode, pattern: pattern.trim() } : null;
}

/** Empty strings become null, as the server expects for "not set". */
export function orNull(text: string): string | null {
  const t = text.trim();
  return t ? t : null;
}

export const MATCH_MODES: { mode: MatchMode; text: string }[] = [
  { mode: "contains", text: "contains" },
  { mode: "equals", text: "is" },
  { mode: "glob", text: "matches the pattern" },
  { mode: "regex", text: "matches the regular expression" },
];

export const SCHEMES: { scheme: VersionScheme; text: string }[] = [
  { scheme: "semver", text: "Version numbers (24.04.1, 2.1.0-rc.1)" },
  { scheme: "date", text: "Dates (2026-11-08)" },
  { scheme: "natural", text: "Text, numbers compared as numbers" },
];

export const VERDICT_TEXT: Record<DryRunVerdict, string> = {
  take: "Would take",
  keep: "Keeps",
  supersede: "Would supersede",
  older: "Older: left alone",
  gone: "Removed: not taken again",
  unusable: "Can't use",
};

export const ITEM_STATE_TEXT: Record<RuleItemState, string> = {
  active: "Kept",
  superseded: "Superseded",
  gone: "Removed",
  failed: "Couldn't add",
};

export function describeSuperseded(a: SupersededAction): string {
  switch (a.action) {
    case "keepSeeding":
      return "keep seeding";
    case "stop":
      return "pause";
    case "seedLimits":
      return describeSeedLimits(a.limits).replace(/^No limits: seeds for ever$/, "keep seeding");
  }
}

/** One line about what a rule does: "Keeps the latest 2 · label ubuntu · older ones: pause". */
export function describeRule(r: RuleSpec): string {
  const parts = [r.keepLatest === 0 ? "Keeps every match" : `Keeps the latest ${r.keepLatest}`];
  if (r.match.sources.length) parts.push(`from ${r.match.sources.join(", ")}`);
  if (r.label) parts.push(`label ${r.label}`);
  if (r.keepLatest > 0) parts.push(`older ones: ${describeSuperseded(r.superseded)}`);
  if (r.alias) parts.push(`latest at ${r.alias.path}`);
  return parts.join(" · ");
}

/** What's wrong with a rule before sending it, if anything the server would refuse. */
export function problems(r: RuleSpec): string[] {
  const out: string[] = [];
  if (!r.name.trim()) out.push("Give the rule a name.");
  if (!Number.isInteger(r.keepLatest) || r.keepLatest < 0 || r.keepLatest > 1000) {
    out.push("Keep between 0 (all) and 1000 releases.");
  }
  if (r.alias && (!r.alias.path.trim() || /(^|[\\/])\.\.([\\/]|$)/.test(r.alias.path) || /^([\\/]|[A-Za-z]:)/.test(r.alias.path))) {
    out.push("The alias path must be relative to the download directory, with no “..”.");
  }
  for (const m of r.match.metadata) {
    if (!m.field.trim()) out.push("Each metadata condition needs a field name.");
  }
  return out;
}

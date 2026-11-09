import type { SeedLimitAction } from "../api/generated/SeedLimitAction";
import type { SeedLimits } from "../api/generated/SeedLimits";

export const NO_SEED_LIMITS: SeedLimits = { ratio: 0, seedMinutes: 0, action: "pause" };

const ACTION_TEXT: Record<SeedLimitAction, string> = {
  pause: "pause",
  remove: "remove it (files stay)",
  removeWithData: "remove it and delete its files",
};

function hours(minutes: number): string {
  const h = minutes / 60;
  return Number.isInteger(h) ? `${h}` : h.toFixed(1);
}

/** "Ratio 2 or 48 hours of seeding, then pause", or "No limits". */
export function describeSeedLimits(l: SeedLimits): string {
  const parts: string[] = [];
  if (l.ratio > 0) parts.push(`ratio ${l.ratio}`);
  if (l.seedMinutes > 0) parts.push(`${hours(l.seedMinutes)} hours of seeding`);
  if (parts.length === 0) return "No limits: seeds for ever";
  const text = parts.join(" or ");
  return `${text[0]!.toUpperCase()}${text.slice(1)}, then ${ACTION_TEXT[l.action]}`;
}

/** "3 h 12 min" */
export function formatSeedingTime(secs: number): string {
  const m = Math.floor(secs / 60);
  if (m < 60) return `${m} min`;
  const h = Math.floor(m / 60);
  return h < 48 ? `${h} h ${m % 60} min` : `${Math.floor(h / 24)} days ${h % 24} h`;
}

/** Ratio, seeding time and action, for the settings, a group or one torrent. */
export function SeedLimitsFields({ value, onChange, idPrefix }: { value: SeedLimits; onChange: (v: SeedLimits) => void; idPrefix: string }) {
  return (
    <div className="seed-limits">
      <label className="field">
        <span>
          Ratio <span className="muted">(0 is none)</span>
        </span>
        <input
          id={`${idPrefix}-ratio`}
          type="number"
          min={0}
          step={0.1}
          inputMode="decimal"
          value={value.ratio === 0 ? "" : value.ratio}
          placeholder="0"
          onChange={(e) => onChange({ ...value, ratio: Math.max(0, Number(e.target.value) || 0) })}
        />
      </label>
      <label className="field">
        <span>
          Seeding hours <span className="muted">(0 is none)</span>
        </span>
        <input
          id={`${idPrefix}-hours`}
          type="number"
          min={0}
          step={0.5}
          inputMode="decimal"
          value={value.seedMinutes === 0 ? "" : hours(value.seedMinutes)}
          placeholder="0"
          onChange={(e) => onChange({ ...value, seedMinutes: Math.max(0, Math.round((Number(e.target.value) || 0) * 60)) })}
        />
      </label>
      <label className="field">
        <span>Then</span>
        <select id={`${idPrefix}-action`} value={value.action} onChange={(e) => onChange({ ...value, action: e.target.value as SeedLimitAction })}>
          <option value="pause">Pause</option>
          <option value="remove">Remove it, keep the files</option>
          <option value="removeWithData">Remove it and delete the files</option>
        </select>
      </label>
    </div>
  );
}

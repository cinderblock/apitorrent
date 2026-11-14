// The Settings page's building blocks: a card of rows, one row per setting (label and a
// line of description beside the control), number and rate inputs that keep what you type,
// switches, and radio cards for choices that need a sentence each.

import { Children, useEffect, useId, useState, type ReactNode } from "react";

import { formatRate } from "../format";
import { RATE_UNIT_BYTES, parseAmount, pickRateUnit, rateFrom, rateIn, type RateUnit } from "./model";

/** A titled group of settings rows. */
export function SettingCard({ title, description, children, footer }: { title?: string; description?: ReactNode; children?: ReactNode; footer?: ReactNode }) {
  return (
    <section className="s-card">
      {(title || description) && (
        <header className="s-card-head">
          {title && <h3>{title}</h3>}
          {description && <p className="muted">{description}</p>}
        </header>
      )}
      {Children.toArray(children).length > 0 && <div className="s-card-body">{children}</div>}
      {footer && <footer className="s-card-foot">{footer}</footer>}
    </section>
  );
}

/** One setting: its label and description, and the control. */
export function SettingRow({
  label,
  description,
  htmlFor,
  children,
  wide = false,
}: {
  label: ReactNode;
  description?: ReactNode;
  /** The control's id, so the label focuses it. */
  htmlFor?: string;
  children: ReactNode;
  /** The control takes the row's full width under the label (lists, long inputs). */
  wide?: boolean;
}) {
  return (
    <div className={`s-row${wide ? " s-row-wide" : ""}`}>
      <div className="s-row-text">
        {htmlFor ? (
          <label className="s-row-label" htmlFor={htmlFor}>
            {label}
          </label>
        ) : (
          <div className="s-row-label">{label}</div>
        )}
        {description && <div className="s-row-desc">{description}</div>}
      </div>
      <div className="s-row-control">{children}</div>
    </div>
  );
}

function formatAmount(n: number): string {
  return n === 0 ? "" : String(n);
}

/** A non-negative number typed as text (no spinner, no scroll-wheel changes); empty is 0. */
export function AmountInput({
  id,
  value,
  onChange,
  unit,
  placeholder = "0",
  max,
  integer = true,
  label,
}: {
  id?: string;
  value: number;
  onChange: (v: number) => void;
  /** Shown after the number, inside the box. */
  unit?: string;
  placeholder?: string;
  max?: number;
  integer?: boolean;
  /** For screen readers when no row label points here. */
  label?: string;
}) {
  const [text, setText] = useState(() => formatAmount(value));
  const parse = (t: string) => {
    const n = parseAmount(t);
    const whole = integer ? Math.floor(n) : n;
    return max === undefined ? whole : Math.min(max, whole);
  };
  // A change from outside (Discard, a preset) replaces the text; typing doesn't.
  useEffect(() => {
    if (parse(text) !== value) setText(formatAmount(value));
  }, [value]);
  const bad = text.trim() !== "" && !/^\d*([.,]\d*)?$/.test(text.trim());
  return (
    <span className={`s-amount${bad ? " invalid" : ""}`}>
      <input
        id={id}
        type="text"
        inputMode={integer ? "numeric" : "decimal"}
        autoComplete="off"
        aria-label={label}
        aria-invalid={bad || undefined}
        value={text}
        placeholder={placeholder}
        onChange={(e) => {
          setText(e.target.value);
          onChange(parse(e.target.value));
        }}
        onBlur={() => {
          if (!bad) setText(formatAmount(value));
        }}
      />
      {unit && <span className="s-unit">{unit}</span>}
    </span>
  );
}

/** A rate in bytes per second, typed in KiB/s or MiB/s; empty is 0 (unlimited, or off). */
export function RateInput({
  id,
  value,
  onChange,
  placeholder = "Unlimited",
  label,
}: {
  id?: string;
  value: number;
  onChange: (bytesPerSecond: number) => void;
  placeholder?: string;
  label?: string;
}) {
  const [unit, setUnit] = useState<RateUnit>(() => pickRateUnit(value));
  const [text, setText] = useState(() => formatAmount(rateIn(value, unit)));
  useEffect(() => {
    if (rateFrom(parseAmount(text), unit) !== value) {
      const u = pickRateUnit(value);
      setUnit(u);
      setText(formatAmount(rateIn(value, u)));
    }
  }, [value]);
  const bad = text.trim() !== "" && !/^\d*([.,]\d*)?$/.test(text.trim());
  const unitId = useId();
  return (
    <span className={`s-amount s-rate${bad ? " invalid" : ""}`}>
      <input
        id={id}
        type="text"
        inputMode="decimal"
        autoComplete="off"
        aria-label={label}
        aria-invalid={bad || undefined}
        value={text}
        placeholder={placeholder}
        onChange={(e) => {
          setText(e.target.value);
          onChange(rateFrom(parseAmount(e.target.value), unit));
        }}
      />
      <select
        id={unitId}
        aria-label="Unit"
        value={unit}
        onChange={(e) => {
          // The number stays; it now means the new unit.
          const u = e.target.value as RateUnit;
          setUnit(u);
          onChange(rateFrom(parseAmount(text), u));
        }}
      >
        {(Object.keys(RATE_UNIT_BYTES) as RateUnit[]).map((u) => (
          <option key={u} value={u}>
            {u}/s
          </option>
        ))}
      </select>
    </span>
  );
}

/** "Unlimited", "Off", or the rate in words, for a row's description. */
export function rateWords(bytesPerSecond: number, zero: string): string {
  return bytesPerSecond > 0 ? formatRate(bytesPerSecond) : zero;
}

/** An on/off setting: the whole row is the label; the switch sits where controls go. */
export function ToggleRow({
  label,
  description,
  checked,
  onChange,
  disabled = false,
  children,
}: {
  label: ReactNode;
  description?: ReactNode;
  checked: boolean;
  onChange: (v: boolean) => void;
  disabled?: boolean;
  /** Settings that only apply while it's on, shown under it. */
  children?: ReactNode;
}) {
  const id = useId();
  return (
    <>
      <div className={`s-row s-toggle-row${disabled ? " disabled" : ""}`}>
        <div className="s-row-text">
          <label className="s-row-label" htmlFor={id}>
            {label}
          </label>
          {description && <div className="s-row-desc">{description}</div>}
        </div>
        <div className="s-row-control">
          <input id={id} type="checkbox" role="switch" className="s-switch" checked={checked} disabled={disabled} onChange={(e) => onChange(e.target.checked)} />
        </div>
      </div>
      {checked && children && <div className="s-nested">{children}</div>}
    </>
  );
}

export type Choice<T extends string> = { value: T; label: string; description?: ReactNode; tone?: "warn" };

/** One of a few choices, each with its consequence spelled out. */
export function ChoiceCards<T extends string>({
  value,
  options,
  onChange,
  label,
  columns = 1,
}: {
  value: T;
  options: readonly Choice<T>[];
  onChange: (v: T) => void;
  /** The group's accessible name. */
  label: string;
  columns?: 1 | 2 | 3;
}) {
  const name = useId();
  return (
    <div className={`s-choices s-choices-${columns}`} role="radiogroup" aria-label={label}>
      {options.map((o) => (
        <label key={o.value} className={`s-choice${o.value === value ? " picked" : ""}${o.tone === "warn" ? " warn" : ""}`}>
          <input type="radio" name={name} value={o.value} checked={o.value === value} onChange={() => onChange(o.value)} />
          <span className="s-choice-text">
            <span className="s-choice-label">{o.label}</span>
            {o.description && <span className="s-choice-desc">{o.description}</span>}
          </span>
        </label>
      ))}
    </div>
  );
}

/** A short note in a section: what applies when, or a caution. */
export function Note({ tone = "info", children }: { tone?: "info" | "warn"; children: ReactNode }) {
  return <div className={`s-note s-note-${tone}`}>{children}</div>;
}

/** An empty list, said plainly. */
export function Empty({ children }: { children: ReactNode }) {
  return <p className="s-empty">{children}</p>;
}

// Where completed files move: a destination, when, and rules by pattern. The same fields for
// the settings' default, a group's and a torrent's own; whether it is on is the caller's.

import type { ReactNode } from "react";

import type { MoveCompleted } from "../api/generated/MoveCompleted";
import type { MoveRule } from "../api/generated/MoveRule";
import type { MoveWhen } from "../api/generated/MoveWhen";
import { MOVE_WHEN, moveProblems, moveRule } from "./fileSettings";
import { Tip } from "./Tip";

export function MoveCompletedFields({
  value,
  onChange,
  idPrefix,
  note,
}: {
  value: MoveCompleted;
  onChange: (v: MoveCompleted) => void;
  idPrefix: string;
  /** A line under the destination: where it may be, say. */
  note?: ReactNode;
}) {
  const setRules = (rules: MoveRule[]) => onChange({ ...value, rules });
  const setRule = (i: number, patch: Partial<MoveRule>) => setRules(value.rules.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  const problems = moveProblems(value);
  return (
    <div className="move-fields">
      <div className="field-row">
        <label className="field grow" htmlFor={`${idPrefix}-to`}>
          <span>Move to</span>
          <input
            id={`${idPrefix}-to`}
            className="mono"
            value={value.to}
            placeholder="Leave in place"
            spellCheck={false}
            autoCapitalize="off"
            autoCorrect="off"
            onChange={(e) => onChange({ ...value, to: e.target.value })}
          />
        </label>
        <label className="field" htmlFor={`${idPrefix}-when`}>
          <span>When</span>
          <select id={`${idPrefix}-when`} value={value.when} onChange={(e) => onChange({ ...value, when: e.target.value as MoveWhen })}>
            {MOVE_WHEN.map(([w, words]) => (
              <option key={w} value={w}>
                {words}
              </option>
            ))}
          </select>
        </label>
      </div>
      <p className="hint muted">
        A full path on the server. Files keep their folders from the torrent under it, and keep seeding from there. Empty: only the rules move files.
        {note && <> {note}</>}
      </p>
      <div className="move-rules">
        <div className="move-rules-head">
          <span>Rules</span>
          <Tip label="How move rules match">
            Each file goes where the first matching rule says. A pattern without a <code>/</code> matches the file name in any folder (<code>*.mkv</code>);
            with one, it matches the path inside the torrent (<code>Extras/**</code>). <code>*</code> stays within a name, <code>**</code> crosses folders,{" "}
            <code>?</code> is one character, and case doesn&apos;t matter. A rule with no destination keeps its files where they are.
          </Tip>
        </div>
        {value.rules.length > 0 && (
          <ol className="move-rule-list">
            {value.rules.map((r, i) => (
              <li key={i} className="move-rule">
                <input
                  className="mono"
                  aria-label={`Rule ${i + 1} pattern`}
                  value={r.pattern}
                  placeholder="*.mkv"
                  spellCheck={false}
                  autoCapitalize="off"
                  autoCorrect="off"
                  onChange={(e) => setRule(i, { pattern: e.target.value })}
                />
                <span className="muted" aria-hidden="true">
                  →
                </span>
                <input
                  className="mono"
                  aria-label={`Rule ${i + 1} destination`}
                  value={r.to}
                  placeholder="Leave in place"
                  spellCheck={false}
                  autoCapitalize="off"
                  autoCorrect="off"
                  onChange={(e) => setRule(i, { to: e.target.value })}
                />
                <span className="move-rule-actions">
                  <button type="button" aria-label={`Move rule ${i + 1} up`} disabled={i === 0} onClick={() => setRules(moveRule(value.rules, i, -1))}>
                    ↑
                  </button>
                  <button
                    type="button"
                    aria-label={`Move rule ${i + 1} down`}
                    disabled={i === value.rules.length - 1}
                    onClick={() => setRules(moveRule(value.rules, i, 1))}
                  >
                    ↓
                  </button>
                  <button type="button" aria-label={`Remove rule ${i + 1}`} onClick={() => setRules(value.rules.filter((_, j) => j !== i))}>
                    Remove
                  </button>
                </span>
              </li>
            ))}
          </ol>
        )}
        <div>
          <button type="button" onClick={() => setRules([...value.rules, { pattern: "", to: "" }])}>
            + Add rule
          </button>
        </div>
      </div>
      {problems.length > 0 && (
        <ul className="move-problems">
          {problems.map((p) => (
            <li key={p} className="error-text">
              {p}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

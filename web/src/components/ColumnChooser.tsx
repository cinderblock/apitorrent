import { useEffect, useRef } from "react";

import { moveColumn, normalize, toggleColumn, type ColumnPrefs } from "../state/columns";

interface ChooserColumn {
  id: string;
  label: string;
  defaultVisible: boolean;
  required?: boolean;
}

/**
 * A panel under the table header: turn columns on and off, and move them. Closes on
 * Escape or a click outside it.
 */
export function ColumnChooser({
  columns,
  prefs,
  onChange,
  onClose,
}: {
  columns: readonly ChooserColumn[];
  prefs: ColumnPrefs | null;
  onChange: (prefs: ColumnPrefs | null) => void;
  onClose: () => void;
}) {
  const panel = useRef<HTMLDivElement>(null);
  const p = normalize(columns, prefs);
  const label = new Map(columns.map((c) => [c.id, c]));
  const visible = p.order.filter((id) => !p.hidden.includes(id));

  useEffect(() => {
    panel.current?.querySelector<HTMLInputElement>("input")?.focus();
    function outside(e: PointerEvent) {
      const target = e.target as Element | null;
      if (target && !panel.current?.contains(target) && !target.closest(".th-columns")) onClose();
    }
    function escape(e: globalThis.KeyboardEvent) {
      if (e.key === "Escape") onClose();
    }
    document.addEventListener("pointerdown", outside);
    document.addEventListener("keydown", escape);
    return () => {
      document.removeEventListener("pointerdown", outside);
      document.removeEventListener("keydown", escape);
    };
  }, [onClose]);

  return (
    <div className="column-chooser" role="dialog" aria-label="Columns" ref={panel}>
      <p className="hint muted">Shown columns, left to right. Right-click the header to open this too.</p>
      <ul>
        {p.order.map((id) => {
          const c = label.get(id)!;
          const shown = !p.hidden.includes(id);
          const at = visible.indexOf(id);
          return (
            <li key={id} className={shown ? "" : "muted"}>
              <label className="check">
                <input type="checkbox" checked={shown} disabled={c.required} onChange={() => onChange(toggleColumn(columns, prefs, id))} />
                {c.label === "#" ? "# (queue position)" : c.label}
                {c.required && <span className="muted"> (always shown)</span>}
              </label>
              <span className="column-moves">
                <button type="button" aria-label={`Move ${c.label} left`} disabled={!shown || at === 0} onClick={() => onChange(moveColumn(columns, prefs, id, -1))}>
                  ←
                </button>
                <button
                  type="button"
                  aria-label={`Move ${c.label} right`}
                  disabled={!shown || at === visible.length - 1}
                  onClick={() => onChange(moveColumn(columns, prefs, id, 1))}
                >
                  →
                </button>
              </span>
            </li>
          );
        })}
      </ul>
      <div className="column-chooser-actions">
        <button type="button" disabled={prefs === null} onClick={() => onChange(null)}>
          Reset to defaults
        </button>
        <button type="button" onClick={onClose}>
          Done
        </button>
      </div>
    </div>
  );
}

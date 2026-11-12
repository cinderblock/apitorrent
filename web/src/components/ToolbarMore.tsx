import { useEffect, useRef, useState } from "react";
import { ThemePicker } from "./ThemePicker";

export interface MoreItem {
  label: string;
  onSelect: () => void;
}

/**
 * The phone toolbar's "More" menu: the less-used toolbar buttons and the theme picker,
 * so the toolbar keeps to one row above the search box. Closes on a choice, Escape or a
 * click outside.
 */
export function ToolbarMore({ items }: { items: MoreItem[] }) {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const button = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (!open) return;
    const outside = (e: PointerEvent) => {
      if (root.current && !root.current.contains(e.target as Node)) setOpen(false);
    };
    const key = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        setOpen(false);
        button.current?.focus();
      }
    };
    document.addEventListener("pointerdown", outside);
    document.addEventListener("keydown", key);
    return () => {
      document.removeEventListener("pointerdown", outside);
      document.removeEventListener("keydown", key);
    };
  }, [open]);

  return (
    <div className="toolbar-more narrow-only" ref={root}>
      <button
        ref={button}
        type="button"
        aria-label="More"
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
      >
        ⋯
      </button>
      {open && (
        <div className="toolbar-more-menu" role="menu" aria-label="More">
          {items.map((item) => (
            <button
              key={item.label}
              type="button"
              role="menuitem"
              onClick={() => {
                setOpen(false);
                item.onSelect();
              }}
            >
              {item.label}
            </button>
          ))}
          <label className="toolbar-more-theme">
            <span>Theme</span>
            <ThemePicker />
          </label>
        </div>
      )}
    </div>
  );
}

import { useEffect, useRef, useState, type ReactNode } from "react";
import { ThemePicker } from "./ThemePicker";

export interface MoreItem {
  label: string;
  onSelect: () => void;
}

interface MenuProps {
  /** The button's text. */
  label: ReactNode;
  /** The button's and the menu's accessible name. */
  name: string;
  items: MoreItem[];
  className?: string;
  /** Open above the button (a bar at the bottom of the screen). */
  up?: boolean;
  /** More content under the items. */
  children?: ReactNode;
}

/** A button that opens a short menu. Closes on a choice, Escape or a click outside. */
export function MenuButton({ label, name, items, className = "", up = false, children }: MenuProps) {
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
        // The menu closes, not whatever the Escape would otherwise do (clear the selection).
        e.stopPropagation();
        setOpen(false);
        button.current?.focus();
      }
    };
    document.addEventListener("pointerdown", outside);
    document.addEventListener("keydown", key, true);
    return () => {
      document.removeEventListener("pointerdown", outside);
      document.removeEventListener("keydown", key, true);
    };
  }, [open]);

  return (
    <div className={`toolbar-more ${className}`} ref={root}>
      <button ref={button} type="button" aria-label={name} aria-haspopup="menu" aria-expanded={open} onClick={() => setOpen((o) => !o)}>
        {label}
      </button>
      {open && (
        <div className={`toolbar-more-menu ${up ? "menu-up" : ""}`} role="menu" aria-label={name}>
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
          {children}
        </div>
      )}
    </div>
  );
}

/**
 * The phone toolbar's "More" menu: the less-used toolbar buttons and the theme picker,
 * so the toolbar keeps to one row above the search box.
 */
export function ToolbarMore({ items }: { items: MoreItem[] }) {
  return (
    <MenuButton label="⋯" name="More" items={items} className="narrow-only">
      <label className="toolbar-more-theme">
        <span>Theme</span>
        <ThemePicker />
      </label>
    </MenuButton>
  );
}

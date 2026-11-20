import { useCallback, useEffect, useId, useLayoutEffect, useRef, useState, type ReactNode } from "react";

/** How long the pointer rests on the ⓘ before the tip opens, and how long it lingers after. */
const OPEN_AFTER_MS = 120;
const CLOSE_AFTER_MS = 150;
/** Gap between the ⓘ and the bubble, and the margin kept from the window's edges. */
const GAP = 6;
const MARGIN = 8;

/**
 * An ⓘ that explains the thing beside it. It opens on hover (after a moment), on keyboard
 * focus, and on a tap or click, which pins it open until a tap elsewhere or Escape. The
 * bubble is positioned against the window, so it isn't clipped by a dialog or a scroller.
 */
export function Tip({ label, children }: { label: string; children: ReactNode }) {
  const id = useId();
  const button = useRef<HTMLButtonElement>(null);
  const bubble = useRef<HTMLSpanElement>(null);
  const [hovered, setHovered] = useState(false);
  const [focused, setFocused] = useState(false);
  const [pinned, setPinned] = useState(false);
  const [place, setPlace] = useState<{ left: number; top: number } | null>(null);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const open = hovered || focused || pinned;

  const hoverSoon = useCallback((on: boolean) => {
    if (timer.current) clearTimeout(timer.current);
    timer.current = setTimeout(() => setHovered(on), on ? OPEN_AFTER_MS : CLOSE_AFTER_MS);
  }, []);
  useEffect(
    () => () => {
      if (timer.current) clearTimeout(timer.current);
    },
    [],
  );

  // Below the ⓘ, or above it when there's no room; never past the window's edges.
  useLayoutEffect(() => {
    if (!open) {
      setPlace(null);
      return;
    }
    const b = button.current?.getBoundingClientRect();
    const t = bubble.current?.getBoundingClientRect();
    if (!b || !t) return;
    const left = Math.min(Math.max(MARGIN, b.left + b.width / 2 - t.width / 2), window.innerWidth - t.width - MARGIN);
    const below = b.bottom + GAP;
    const top = below + t.height + MARGIN > window.innerHeight && b.top - GAP - t.height >= MARGIN ? b.top - GAP - t.height : below;
    setPlace({ left: Math.max(MARGIN, left), top });
  }, [open]);

  // A pinned tip closes on a tap elsewhere, or on Escape (which then goes no further, so a
  // dialog or page around it stays open).
  useEffect(() => {
    if (!open) return;
    const away = (e: PointerEvent) => {
      const target = e.target as Node;
      if (button.current?.contains(target) || bubble.current?.contains(target)) return;
      setPinned(false);
      setHovered(false);
    };
    const key = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.preventDefault();
      e.stopPropagation();
      setPinned(false);
      setHovered(false);
      setFocused(false);
    };
    const close = () => {
      setPinned(false);
      setHovered(false);
    };
    document.addEventListener("pointerdown", away, true);
    document.addEventListener("keydown", key, true);
    window.addEventListener("scroll", close, true);
    window.addEventListener("resize", close);
    return () => {
      document.removeEventListener("pointerdown", away, true);
      document.removeEventListener("keydown", key, true);
      window.removeEventListener("scroll", close, true);
      window.removeEventListener("resize", close);
    };
  }, [open]);

  return (
    <span className="tip">
      <button
        ref={button}
        type="button"
        className="tip-button"
        aria-label={label}
        aria-expanded={open}
        aria-describedby={open ? id : undefined}
        onPointerEnter={(e) => e.pointerType === "mouse" && hoverSoon(true)}
        onPointerLeave={(e) => e.pointerType === "mouse" && hoverSoon(false)}
        onFocus={(e) => setFocused(e.target.matches(":focus-visible"))}
        onBlur={() => setFocused(false)}
        onClick={(e) => {
          // Often inside a label: don't let the click go on to focus its input.
          e.preventDefault();
          setPinned(!pinned);
          if (pinned) setHovered(false);
        }}
      >
        ⓘ
      </button>
      {open && (
        <span
          ref={bubble}
          id={id}
          role="tooltip"
          className="tip-bubble"
          style={place ? { left: place.left, top: place.top } : { visibility: "hidden" }}
          onPointerEnter={(e) => e.pointerType === "mouse" && hoverSoon(true)}
          onPointerLeave={(e) => e.pointerType === "mouse" && hoverSoon(false)}
          onClick={(e) => e.preventDefault()}
        >
          {children}
        </span>
      )}
    </span>
  );
}

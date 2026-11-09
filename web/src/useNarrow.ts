import { useSyncExternalStore } from "react";

/** Below this width the UI switches to its phone layout. Keep in step with styles.css. */
export const NARROW_QUERY = "(max-width: 720px)";

function subscribe(onChange: () => void): () => void {
  const mq = window.matchMedia(NARROW_QUERY);
  mq.addEventListener("change", onChange);
  return () => mq.removeEventListener("change", onChange);
}

/** Whether the phone layout applies. */
export function useNarrow(): boolean {
  return useSyncExternalStore(
    subscribe,
    () => window.matchMedia(NARROW_QUERY).matches,
    () => false,
  );
}

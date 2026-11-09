// Small helpers for the desktop UI, free of React and Tauri so they can be tested.

/** A message for any thrown value. */
export function describe(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** Comma- or whitespace-separated entries, trimmed, without empties. */
export function splitList(text: string): string[] {
  return text
    .split(/[\s,]+/)
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
}

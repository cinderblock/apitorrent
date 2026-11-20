// Whether the Deluge import dialog is open, and on which job: any page can open it (the
// empty torrent list, Settings → Deluge, the jobs tray), and one place renders it.

import { useSyncExternalStore } from "react";

/** A job the dialog follows instead of starting at the first step. */
export interface JobRef {
  slug: string | null;
  id: number;
}

/** `null`: closed. */
type DialogState = { job: JobRef | null } | null;

let state: DialogState = null;
const listeners = new Set<() => void>();

function emit() {
  for (const l of listeners) l();
}

export function showDelugeImport(job: JobRef | null = null): void {
  state = { job };
  emit();
}

export function closeDelugeImport(): void {
  state = null;
  emit();
}

export function useDelugeImportDialog(): DialogState {
  return useSyncExternalStore(
    (l) => {
      listeners.add(l);
      return () => listeners.delete(l);
    },
    () => state,
  );
}

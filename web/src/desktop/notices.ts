// Short messages from the desktop app to the user (a .torrent file that couldn't be read,
// a folder that couldn't be opened). Shown by <DesktopNotices>.

import { useSyncExternalStore } from "react";

export interface Notice {
  id: number;
  text: string;
}

let notices: Notice[] = [];
let nextId = 1;
const listeners = new Set<() => void>();

function emit(): void {
  for (const l of listeners) l();
}

export function addNotice(text: string): void {
  notices = [...notices, { id: nextId++, text }].slice(-5);
  emit();
}

export function dismissNotice(id: number): void {
  notices = notices.filter((n) => n.id !== id);
  emit();
}

export function currentNotices(): Notice[] {
  return notices;
}

export function useNotices(): Notice[] {
  return useSyncExternalStore(
    (l) => {
      listeners.add(l);
      return () => listeners.delete(l);
    },
    () => notices,
  );
}

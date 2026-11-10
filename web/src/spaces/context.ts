// The page's space, loaded once: what the switcher shows and what the caller may do here.

import { useSyncExternalStore } from "react";

import type { SpaceContext } from "../api/generated/SpaceContext";
import type { SpaceRole } from "../api/generated/SpaceRole";
import { spacesApi } from "./api";

let current: SpaceContext | null = null;
let loading: Promise<void> | null = null;
const listeners = new Set<() => void>();

function notify() {
  for (const l of listeners) l();
}

/** Load (or reload) the context. Failures leave it empty: the UI then acts as before spaces. */
export function loadSpaceContext(): Promise<void> {
  loading ??= spacesApi
    .context()
    .then((c) => {
      current = c;
    })
    .catch(() => {
      current = null;
    })
    .finally(() => {
      loading = null;
      notify();
    });
  return loading;
}

function subscribe(l: () => void): () => void {
  listeners.add(l);
  if (current === null && loading === null) void loadSpaceContext();
  return () => listeners.delete(l);
}

export function useSpaceContext(): SpaceContext | null {
  return useSyncExternalStore(subscribe, () => current);
}

const RANK: Record<SpaceRole, number> = { viewer: 0, user: 1, admin: 2 };

/** Whether a role is `at least` another. */
export function atLeast(role: SpaceRole, least: SpaceRole): boolean {
  return RANK[role] >= RANK[least];
}

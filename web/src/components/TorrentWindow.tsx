import { useEffect, useMemo, useSyncExternalStore } from "react";

import type { AuthStatus } from "../api/generated/AuthStatus";
import { SyncClient } from "../api/sync";
import { atLeast, useSpaceContext } from "../spaces/context";
import { DetailPane } from "./DetailPane";

/**
 * One torrent's details, filling a window of its own (`/torrent/<id>`). It keeps its own
 * live connection, so it carries on when the window with the list closes.
 */
export function TorrentWindow({ id, status: auth }: { id: string; status: AuthStatus }) {
  const sync = useMemo(() => SyncClient.forCurrentPage(), []);
  useEffect(() => {
    sync.start();
    return () => sync.stop();
  }, [sync]);
  const { state, status } = useSyncExternalStore(sync.subscribe, sync.getSnapshot);
  const space = useSpaceContext();
  const mayWrite = space ? atLeast(space.current.role, "user") : auth.authDisabled || auth.user?.role !== "viewer";
  const summary = state.torrents.get(id);
  const loaded = state.revision !== null;

  useEffect(() => {
    document.title = summary ? `${summary.name} · apitorrent` : "apitorrent";
  }, [summary?.name]);

  return (
    <div className="torrent-window">
      {summary ? (
        <>
          <header className="torrent-window-head">
            <h1>{summary.name}</h1>
            {status !== "live" && (
              <span className={`conn conn-${status}`} role="status">
                {status === "connecting" ? "Connecting…" : "Reconnecting…"}
              </span>
            )}
          </header>
          <DetailPane summary={summary} readOnly={!mayWrite} />
        </>
      ) : (
        <p className="torrent-window-gone muted">{loaded ? "This torrent isn't here any more: it was removed, or moved to another space." : "Loading…"}</p>
      )}
    </div>
  );
}

import { useEffect, useState, type ReactNode } from "react";

import { api } from "../api/client";
import type { SessionStats } from "../api/generated/SessionStats";
import { formatBytes } from "../format";

/**
 * Session-wide numbers along the bottom, refreshed every few seconds. `rates`: the total
 * transfer rates, shown here instead of in the toolbar when the window is mid-width.
 */
export function StatusBar({ rates }: { rates?: ReactNode }) {
  const [stats, setStats] = useState<SessionStats | null>(null);
  useEffect(() => {
    let alive = true;
    const load = () => {
      api.stats().then(
        (s) => alive && setStats(s),
        () => alive && setStats(null),
      );
    };
    load();
    const timer = setInterval(load, 3000);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  }, []);
  const ratesHere = rates && <span className="statusbar-rates">{rates}</span>;
  if (!stats) return <footer className="statusbar">{ratesHere}</footer>;
  return (
    <footer className="statusbar">
      {ratesHere}
      <span>{stats.connections} connections</span>
      <span>
        Piece memory {formatBytes(stats.memoryUsed) || "0 B"} of {formatBytes(stats.memoryLimit)}
      </span>
      <span>{stats.dhtNodes === null ? "DHT off" : `DHT ${stats.dhtNodes} nodes`}</span>
      {stats.banned > 0 && <span>{stats.banned} banned</span>}
      <span>Peer port {stats.peerPort}</span>
    </footer>
  );
}

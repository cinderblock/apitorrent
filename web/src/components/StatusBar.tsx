import { useEffect, useState } from "react";

import { api } from "../api/client";
import type { SessionStats } from "../api/generated/SessionStats";
import { formatBytes } from "../format";

/** Session-wide numbers along the bottom, refreshed every few seconds. */
export function StatusBar() {
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
  if (!stats) return <footer className="statusbar" />;
  return (
    <footer className="statusbar">
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

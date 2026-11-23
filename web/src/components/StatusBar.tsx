import { useEffect, useState, type ReactNode } from "react";

import { api } from "../api/client";
import type { SessionStats } from "../api/generated/SessionStats";
import { formatBytes } from "../format";
import { reachLines, reachSummary } from "./reachability";
import { Tip } from "./Tip";

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
  const reach = reachSummary(stats.reachability);
  const reachDetail = reachLines(stats.reachability, Date.now());
  return (
    <footer className="statusbar">
      {ratesHere}
      <span>{stats.connections} connections</span>
      <span>
        Piece memory {formatBytes(stats.memoryUsed) || "0 B"} of {formatBytes(stats.memoryLimit)}
      </span>
      <span>{stats.dhtNodes === null ? "DHT off" : `DHT ${stats.dhtNodes} nodes`}</span>
      {stats.banned > 0 && <span>{stats.banned} banned</span>}
      {stats.libraryBytes > 0 && <span>Library saved {formatBytes(stats.libraryBytes)}</span>}
      <span>
        Peer port {stats.peerPort}
        {reach && <span className={`reach reach-${reach.tone}`}> {reach.text}</span>}
        {stats.portForward &&
          (stats.portForward.method
            ? `, forwarded by ${stats.portForward.method === "upnp" ? "UPnP" : "NAT-PMP"}`
            : ", not forwarded")}
        {reachDetail.length > 0 && (
          <Tip label="About the peer port">
            {reachDetail.map((line) => (
              <span key={line} className="tip-line">
                {line}
              </span>
            ))}
            <span className="tip-line">
              An incoming connection proves a port open. Until one comes, apitorrent can't tell
              a closed port from a quiet swarm.
            </span>
          </Tip>
        )}
      </span>
    </footer>
  );
}

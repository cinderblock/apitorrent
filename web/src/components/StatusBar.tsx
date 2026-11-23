import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";

import { api } from "../api/client";
import type { PortCheckService } from "../api/generated/PortCheckService";
import type { SessionStats } from "../api/generated/SessionStats";
import { formatBytes } from "../format";
import { checkSummary, checkerHost, PORT_CHECKERS, reachLines, reachSummary } from "./reachability";
import { Tip } from "./Tip";
import { MenuButton } from "./ToolbarMore";

/**
 * Session-wide numbers along the bottom, refreshed every few seconds. `rates`: the total
 * transfer rates, shown here instead of in the toolbar when the window is mid-width.
 */
export function StatusBar({ rates }: { rates?: ReactNode }) {
  const [stats, setStats] = useState<SessionStats | null>(null);
  const alive = useRef(true);
  const load = useCallback(() => {
    api.stats().then(
      (s) => alive.current && setStats(s),
      () => alive.current && setStats(null),
    );
  }, []);
  useEffect(() => {
    alive.current = true;
    load();
    const timer = setInterval(load, 3000);
    return () => {
      alive.current = false;
      clearInterval(timer);
    };
  }, [load]);
  const [checking, setChecking] = useState<PortCheckService | null>(null);
  const [checkNote, setCheckNote] = useState<string | null>(null);
  // The answer stays a minute; after that the ⓘ has it, with its time.
  useEffect(() => {
    if (checkNote === null) return;
    const timer = setTimeout(() => setCheckNote(null), 60_000);
    return () => clearTimeout(timer);
  }, [checkNote]);
  const checkPort = (service: PortCheckService) => {
    setChecking(service);
    setCheckNote(null);
    api.checkPort(service).then(
      (r) => alive.current && setCheckNote(checkSummary(service, r)),
      (e: unknown) => alive.current && setCheckNote(`${checkerHost(service)}: ${e instanceof Error ? e.message : String(e)}`),
    ).finally(() => {
      if (!alive.current) return;
      setChecking(null);
      load();
    });
  };
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
              a closed port from a quiet swarm, unless you check the port: the service you pick
              connects back to this port at the daemon's public address.
            </span>
          </Tip>
        )}
      </span>
      {reach && (
        <MenuButton
          className="statusbar-check"
          label={checking ? `Checking with ${checkerHost(checking)}…` : "Check port with…"}
          name="Check the peer port with a public service"
          up
          disabled={checking !== null}
          items={PORT_CHECKERS.map((c) => ({ label: c.host, onSelect: () => checkPort(c.service) }))}
        />
      )}
      {checkNote && <span className="statusbar-check-note">{checkNote}</span>}
    </footer>
  );
}

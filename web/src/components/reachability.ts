import type { FamilyReachability } from "../api/generated/FamilyReachability";
import type { PortReachability } from "../api/generated/PortReachability";
import type { Reachability } from "../api/generated/Reachability";

export type ReachTone = "ok" | "warn" | "muted";

/**
 * One phrase for the peer port as a whole: open when TCP has seen an incoming connection
 * on any family, probably closed when one has waited long enough without, else not yet
 * known. Null when it listens on no family (nothing to say).
 */
export function reachSummary(r: Reachability): { text: string; tone: ReachTone } | null {
  const tcp = [r.ipv4.tcp, r.ipv6.tcp].filter((p): p is PortReachability => p !== null);
  if (tcp.length === 0) return null;
  if (tcp.some((p) => p.state === "open")) return { text: "open", tone: "ok" };
  if (tcp.some((p) => p.state === "probablyClosed")) return { text: "probably closed", tone: "warn" };
  return { text: "not confirmed yet", tone: "muted" };
}

/** "just now", "4 min ago", "2 h ago", "3 days ago". */
export function ago(ms: number, now: number): string {
  const minutes = Math.floor(Math.max(0, now - ms) / 60_000);
  if (minutes < 1) return "just now";
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 48) return `${hours} h ago`;
  return `${Math.floor(hours / 24)} days ago`;
}

function portLine(name: string, p: PortReachability, now: number): string {
  const head = `${name} ${p.port}: `;
  switch (p.state) {
    case "open": {
      const when = p.lastIncoming === null ? "" : `, last incoming connection ${ago(p.lastIncoming, now)}`;
      return `${head}open${when}`;
    }
    case "probablyClosed":
      return `${head}probably closed: nothing came in on its own for half an hour after peers learned our address`;
    case "waiting":
      return p.waitingSince === null
        ? `${head}no incoming connection yet; waiting for a tracker or the DHT to announce us`
        : `${head}no incoming connection yet`;
  }
}

/**
 * The lines behind the summary, for one family: our public address, a warning when a NAT
 * sits in front of the router, and each port.
 */
export function familyLines(label: "IPv4" | "IPv6", f: FamilyReachability, now: number): string[] {
  const lines: string[] = [];
  if (f.externalIp) lines.push(`Public ${label} address: ${f.externalIp}`);
  if (f.doubleNat && f.fromRouter) {
    lines.push(
      f.externalIp
        ? `The router's public address is ${f.fromRouter}, not ${f.externalIp}: another NAT (often the internet provider's) is in front of it, so a port forward on the router can't open the port.`
        : `The router's public address, ${f.fromRouter}, isn't a public one: another NAT (often the internet provider's) is in front of it, so a port forward on the router can't open the port.`,
    );
  }
  for (const [name, p] of [
    ["TCP", f.tcp],
    ["uTP", f.utp],
    ["DHT", f.dht],
  ] as const) {
    if (p) lines.push(`${label} ${portLine(name, p, now)}`);
  }
  return lines;
}

/** Every line, IPv4 first. */
export function reachLines(r: Reachability, now: number): string[] {
  return [...familyLines("IPv4", r.ipv4, now), ...familyLines("IPv6", r.ipv6, now)];
}

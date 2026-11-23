// Settings sections that edit the instance's settings draft: saved together with the page's
// Save bar. A few hold admin-only parts that act at once (storage roots, library folders).

import { useEffect, useState } from "react";

import type { FileOrder } from "../api/generated/FileOrder";
import type { GroupSettings } from "../api/generated/GroupSettings";
import type { MoveCompleted } from "../api/generated/MoveCompleted";
import type { ProxySettings } from "../api/generated/ProxySettings";
import type { ProxyType } from "../api/generated/ProxyType";
import type { SeedLimitAction } from "../api/generated/SeedLimitAction";
import type { SeedLimits } from "../api/generated/SeedLimits";
import type { Settings } from "../api/generated/Settings";
import { FILE_ORDERS, describeFileOrder, describeMove, fileOrderOf, type FileOrderBy } from "../components/fileSettings";
import { DedupeStatus, LibraryFolders } from "../components/LibrarySection";
import { MoveCompletedFields } from "../components/MoveCompletedFields";
import { describeSeedLimits } from "../components/SeedLimitsFields";
import { StorageRoots } from "../components/StorageRootsSection";
import { AmountInput, ChoiceCards, Note, RateInput, SettingCard, SettingRow, ToggleRow, rateWords, type Choice } from "./controls";
import { parseLabels } from "./model";

export type DraftProps = {
  draft: Settings;
  update: (patch: Partial<Settings>) => void;
  /** The settings as the server has them (for status that depends on what's saved). */
  saved: Settings;
  serverAdmin: boolean;
};

// Transfers

export function TransfersSection({ draft, update }: DraftProps) {
  const q = draft.queue;
  const setQueue = (patch: Partial<Settings["queue"]>) => update({ queue: { ...q, ...patch } });
  return (
    <>
      <SettingCard title="Speed limits" description="For all torrents together. Leave empty for no limit.">
        <SettingRow label="Download" htmlFor="s-down">
          <RateInput id="s-down" value={draft.downloadRate} onChange={(downloadRate) => update({ downloadRate })} />
        </SettingRow>
        <SettingRow label="Upload" htmlFor="s-up">
          <RateInput id="s-up" value={draft.uploadRate} onChange={(uploadRate) => update({ uploadRate })} />
        </SettingRow>
      </SettingCard>

      <SettingCard title="Queue" description="Torrents beyond these numbers wait their turn, in queue order. Leave empty for no limit.">
        <SettingRow label="Downloading at once" htmlFor="s-q-down">
          <AmountInput id="s-q-down" value={q.maxActiveDownloads} placeholder="No limit" onChange={(maxActiveDownloads) => setQueue({ maxActiveDownloads })} />
        </SettingRow>
        <SettingRow label="Seeding at once" htmlFor="s-q-seed">
          <AmountInput id="s-q-seed" value={q.maxActiveSeeds} placeholder="No limit" onChange={(maxActiveSeeds) => setQueue({ maxActiveSeeds })} />
        </SettingRow>
        <SettingRow label="Running in total" description="Downloading and seeding together" htmlFor="s-q-all">
          <AmountInput id="s-q-all" value={q.maxActive} placeholder="No limit" onChange={(maxActive) => setQueue({ maxActive })} />
        </SettingRow>
        <ToggleRow
          label="Let slow torrents give up their slot"
          description="A slow torrent keeps running, but the next one in the queue starts. It still counts toward the total."
          checked={q.ignoreSlow}
          onChange={(ignoreSlow) => setQueue({ ignoreSlow })}
        >
          <SettingRow label="Slow while downloading below" htmlFor="s-q-slowdown">
            <RateInput id="s-q-slowdown" value={q.slowDownloadRate} placeholder="0" onChange={(slowDownloadRate) => setQueue({ slowDownloadRate })} />
          </SettingRow>
          <SettingRow label="Slow while seeding below" description="Upload rate" htmlFor="s-q-slowup">
            <RateInput id="s-q-slowup" value={q.slowUploadRate} placeholder="0" onChange={(slowUploadRate) => setQueue({ slowUploadRate })} />
          </SettingRow>
          <SettingRow label="For at least" description="A torrent that just started gets this long to find peers" htmlFor="s-q-after">
            <AmountInput id="s-q-after" value={q.slowAfterSecs} unit="seconds" onChange={(slowAfterSecs) => setQueue({ slowAfterSecs })} />
          </SettingRow>
        </ToggleRow>
      </SettingCard>
    </>
  );
}

// Seeding

const SEED_ACTIONS: readonly Choice<SeedLimitAction>[] = [
  { value: "pause", label: "Pause it", description: "Resume it by hand to seed on; its limits then leave it alone." },
  { value: "remove", label: "Remove it, keep the files" },
  { value: "removeWithData", label: "Remove it and delete its files", tone: "warn" },
];

function hours(minutes: number): number {
  return Math.round((minutes / 60) * 100) / 100;
}

/** Ratio, seeding time and what happens then: the instance's, or a group's own. */
function SeedLimitRows({ value, onChange, idPrefix }: { value: SeedLimits; onChange: (v: SeedLimits) => void; idPrefix: string }) {
  const none = value.ratio === 0 && value.seedMinutes === 0;
  return (
    <>
      <SettingRow label="Stop at ratio" description="Uploaded divided by size" htmlFor={`${idPrefix}-ratio`}>
        <AmountInput id={`${idPrefix}-ratio`} integer={false} value={value.ratio} placeholder="No limit" onChange={(ratio) => onChange({ ...value, ratio })} />
      </SettingRow>
      <SettingRow label="Or after seeding for" htmlFor={`${idPrefix}-hours`}>
        <AmountInput
          id={`${idPrefix}-hours`}
          integer={false}
          unit="hours"
          value={hours(value.seedMinutes)}
          placeholder="No limit"
          onChange={(h) => onChange({ ...value, seedMinutes: Math.round(h * 60) })}
        />
      </SettingRow>
      <SettingRow label="Then" description={none ? "Applies once a ratio or a time is set" : undefined} wide>
        <ChoiceCards label="When the limit is reached" columns={3} value={value.action} options={SEED_ACTIONS} onChange={(action) => onChange({ ...value, action })} />
      </SettingRow>
    </>
  );
}

export function SeedingSection({ draft, update }: DraftProps) {
  return (
    <SettingCard
      title="Default seeding limits"
      description={
        <>
          Whichever comes first. A group, or a torrent in its details, can set its own. Now: <strong>{describeSeedLimits(draft.seedLimits)}</strong>.
        </>
      }
    >
      <SeedLimitRows idPrefix="s-seed" value={draft.seedLimits} onChange={(seedLimits) => update({ seedLimits })} />
    </SettingCard>
  );
}

// Groups

const NEW_GROUP: GroupSettings = { name: "", labels: [], downloadRate: 0, uploadRate: 0, maxConnections: 0, seedLimits: null, fileOrder: null, moveCompleted: null };

/** Labels typed comma-separated; the text is kept as typed, so a comma doesn't vanish. */
function LabelsInput({ id, value, onChange }: { id: string; value: string[]; onChange: (v: string[]) => void }) {
  const [text, setText] = useState(value.join(", "));
  useEffect(() => {
    if (parseLabels(text).join("\n") !== value.join("\n")) setText(value.join(", "));
  }, [value]);
  return (
    <input
      id={id}
      className="s-text"
      value={text}
      placeholder="For example: tv, anime"
      spellCheck={false}
      onChange={(e) => {
        setText(e.target.value);
        onChange(parseLabels(e.target.value));
      }}
    />
  );
}

function groupSummary(g: GroupSettings): string {
  const parts: string[] = [];
  if (g.downloadRate > 0) parts.push(`↓ ${rateWords(g.downloadRate, "")}`);
  if (g.uploadRate > 0) parts.push(`↑ ${rateWords(g.uploadRate, "")}`);
  if (g.maxConnections > 0) parts.push(`${g.maxConnections} connections`);
  if (g.seedLimits) parts.push(describeSeedLimits(g.seedLimits));
  if (g.fileOrder) parts.push(`order: ${describeFileOrder(g.fileOrder).toLowerCase()}`);
  if (g.moveCompleted) parts.push(`move: ${describeMove(g.moveCompleted).toLowerCase()}`);
  return parts.length > 0 ? parts.join(" · ") : "No limits of its own yet";
}

function GroupCard({
  group: g,
  index: i,
  open,
  onToggle,
  onChange,
  onRemove,
  defaults,
}: {
  group: GroupSettings;
  index: number;
  open: boolean;
  onToggle: () => void;
  onChange: (patch: Partial<GroupSettings>) => void;
  onRemove: () => void;
  /** The instance's settings these override. */
  defaults: Pick<Settings, "seedLimits" | "fileOrder" | "moveCompleted">;
}) {
  const p = `s-group-${i}`;
  return (
    <section className={`s-card s-group${open ? " open" : ""}`}>
      <header className="s-group-head">
        <button type="button" className="s-group-toggle" aria-expanded={open} aria-controls={open ? `${p}-body` : undefined} onClick={onToggle}>
          <span className="s-chevron" aria-hidden="true">
            ›
          </span>
          <span className="s-group-name">{g.name.trim() || <em className="error-text">Unnamed group</em>}</span>
          <span className="s-group-labels">
            {g.labels.length === 0 ? (
              <span className="muted">no labels</span>
            ) : (
              g.labels.map((l) => (
                <span key={l} className="chip">
                  {l}
                </span>
              ))
            )}
          </span>
          {!open && <span className="s-group-summary muted">{groupSummary(g)}</span>}
        </button>
      </header>
      {open && (
        <div className="s-card-body" id={`${p}-body`}>
          <SettingRow label="Name" htmlFor={`${p}-name`}>
            <input id={`${p}-name`} className="s-text" value={g.name} placeholder="Name this group" onChange={(e) => onChange({ name: e.target.value })} />
          </SettingRow>
          <SettingRow label="Labels" description="Torrents with any of these labels are in the group. Separate them with commas." htmlFor={`${p}-labels`}>
            <LabelsInput id={`${p}-labels`} value={g.labels} onChange={(labels) => onChange({ labels })} />
          </SettingRow>
          <SettingRow label="Download" description="Shared by the group's torrents" htmlFor={`${p}-down`}>
            <RateInput id={`${p}-down`} value={g.downloadRate} onChange={(downloadRate) => onChange({ downloadRate })} />
          </SettingRow>
          <SettingRow label="Upload" htmlFor={`${p}-up`}>
            <RateInput id={`${p}-up`} value={g.uploadRate} onChange={(uploadRate) => onChange({ uploadRate })} />
          </SettingRow>
          <SettingRow label="Connections" htmlFor={`${p}-conn`}>
            <AmountInput id={`${p}-conn`} value={g.maxConnections} placeholder="No limit" onChange={(maxConnections) => onChange({ maxConnections })} />
          </SettingRow>
          <ToggleRow
            label="Its own seeding limits"
            description={g.seedLimits ? undefined : `Off: follows the default (${describeSeedLimits(defaults.seedLimits).toLowerCase()})`}
            checked={g.seedLimits !== null}
            onChange={(on) => onChange({ seedLimits: on ? { ...defaults.seedLimits } : null })}
          >
            {g.seedLimits && <SeedLimitRows idPrefix={`${p}-seed`} value={g.seedLimits} onChange={(seedLimits) => onChange({ seedLimits })} />}
          </ToggleRow>
          <ToggleRow
            label="Its own download order"
            description={g.fileOrder ? undefined : `Off: follows the default (${describeFileOrder(defaults.fileOrder).toLowerCase()})`}
            checked={g.fileOrder !== null}
            onChange={(on) => onChange({ fileOrder: on ? { ...defaults.fileOrder } : null })}
          >
            {g.fileOrder && (
              <SettingRow label="Order" htmlFor={`${p}-order`}>
                <FileOrderSelect id={`${p}-order`} value={g.fileOrder} onChange={(fileOrder) => onChange({ fileOrder })} />
              </SettingRow>
            )}
          </ToggleRow>
          <ToggleRow
            label="Its own moving of completed files"
            description={g.moveCompleted ? undefined : `Off: follows the default (${describeMove(defaults.moveCompleted).toLowerCase()})`}
            checked={g.moveCompleted !== null}
            onChange={(on) => onChange({ moveCompleted: on ? structuredClone(defaults.moveCompleted) : null })}
          >
            {g.moveCompleted && <MoveRows idPrefix={`${p}-move`} value={g.moveCompleted} onChange={(moveCompleted) => onChange({ moveCompleted })} />}
          </ToggleRow>
          <div className="s-card-foot">
            <button type="button" className="danger-outline" onClick={onRemove}>
              Remove group
            </button>
          </div>
        </div>
      )}
    </section>
  );
}

export function GroupsSection({ draft, update }: DraftProps) {
  // Which groups are open, by position. New and unnamed groups open.
  const [open, setOpen] = useState<Set<number>>(() => new Set(draft.groups.flatMap((g, i) => (g.name.trim() === "" ? [i] : []))));
  const setGroup = (i: number, patch: Partial<GroupSettings>) => update({ groups: draft.groups.map((g, j) => (j === i ? { ...g, ...patch } : g)) });
  const shift = (s: Set<number>, removed: number) => new Set([...s].filter((i) => i !== removed).map((i) => (i > removed ? i - 1 : i)));
  return (
    <>
      <Note>
        A torrent is in a group when it has one of the group&apos;s labels; a label belongs to one group at most. The group&apos;s limits apply to its torrents
        together, on top of the server-wide ones.
      </Note>
      {draft.groups.length === 0 && (
        <SettingCard>
          <p className="s-empty">No groups yet. Make one to give, say, everything labelled “tv” its own upload limit.</p>
        </SettingCard>
      )}
      {draft.groups.map((g, i) => (
        <GroupCard
          key={i}
          group={g}
          index={i}
          open={open.has(i)}
          defaults={draft}
          onToggle={() => setOpen((s) => (s.has(i) ? new Set([...s].filter((x) => x !== i)) : new Set([...s, i])))}
          onChange={(patch) => setGroup(i, patch)}
          onRemove={() => {
            update({ groups: draft.groups.filter((_, j) => j !== i) });
            setOpen((s) => shift(s, i));
          }}
        />
      ))}
      <div>
        <button
          type="button"
          onClick={() => {
            setOpen((s) => new Set([...s, draft.groups.length]));
            update({ groups: [...draft.groups, { ...NEW_GROUP }] });
          }}
        >
          + Add group
        </button>
      </div>
    </>
  );
}

// Network

const ENCRYPTION: readonly Choice<Settings["encryption"]>[] = [
  { value: "enabled", label: "Encrypt when the peer can", description: "Recommended. Plain connections still work with peers that can't encrypt." },
  {
    value: "required",
    label: "Encrypted peers only",
    description: "Hides all of it, but shuts out peers that can't encrypt, so some torrents find fewer peers.",
  },
  { value: "preferPlaintext", label: "Obfuscate the handshake only", description: "When the peer allows it. Less work per byte; the data itself isn't encrypted." },
  { value: "disabled", label: "Never encrypt", description: "Some networks throttle or block plain BitTorrent traffic.", tone: "warn" },
];

export function NetworkSection({ draft, update }: DraftProps) {
  return (
    <>
      <SettingCard title="Ports" description="Picked at random on the first start and kept, so they don't clash with another client's usual 6881.">
        <SettingRow label="Peer port" description="TCP and uTP" htmlFor="s-peer-port">
          <AmountInput id="s-peer-port" value={draft.peerPort} max={65535} placeholder="Automatic" onChange={(peerPort) => update({ peerPort })} />
        </SettingRow>
        <SettingRow label="DHT port" description="UDP" htmlFor="s-dht-port">
          <AmountInput id="s-dht-port" value={draft.dhtPort} max={65535} placeholder="Automatic" onChange={(dhtPort) => update({ dhtPort })} />
        </SettingRow>
        <Note>
          Port changes apply when the daemon next restarts. Empty picks a new port then. A port given on the daemon&apos;s command line wins over these.
        </Note>
      </SettingCard>

      <SettingCard title="Finding peers" description="Beyond the trackers. Private torrents never use the DHT or local discovery: their peers come from the tracker.">
        <ToggleRow
          label="DHT"
          description="Peers for public torrents without a tracker, and magnet links"
          checked={draft.discovery.dht}
          onChange={(dht) => update({ discovery: { ...draft.discovery, dht } })}
        />
        <ToggleRow
          label="Local peer discovery"
          description="Peers on the same network, found by multicast"
          checked={draft.discovery.lsd}
          onChange={(lsd) => update({ discovery: { ...draft.discovery, lsd } })}
        />
        <ToggleRow
          label="Forward ports with UPnP"
          description="Asks the router to open the peer and DHT ports, so peers outside can connect in"
          checked={draft.discovery.upnp}
          onChange={(upnp) => update({ discovery: { ...draft.discovery, upnp } })}
        />
        <ToggleRow
          label="Forward ports with NAT-PMP"
          description="The same, for routers that speak NAT-PMP; tried before UPnP"
          checked={draft.discovery.natpmp}
          onChange={(natpmp) => update({ discovery: { ...draft.discovery, natpmp } })}
        />
        <Note>These apply when the daemon next restarts. The daemon&apos;s --no-dht and --no-lsd turn those off whatever is set here.</Note>
        {draft.proxy.force && draft.proxy.type !== "none" && <Note tone="warn">Force proxy (under Proxy, below) keeps all four off.</Note>}
      </SettingCard>

      <SettingCard title="Connections" description="Across all torrents. Leave empty for no limit.">
        <SettingRow label="Open connections" htmlFor="s-conn">
          <AmountInput id="s-conn" value={draft.maxConnections} placeholder="No limit" onChange={(maxConnections) => update({ maxConnections })} />
        </SettingRow>
        <SettingRow label="Connecting at once" description="Attempts still waiting for an answer" htmlFor="s-halfopen">
          <AmountInput id="s-halfopen" value={draft.maxHalfOpen} placeholder="No limit" onChange={(maxHalfOpen) => update({ maxHalfOpen })} />
        </SettingRow>
        <SettingRow label="New connections" description="Some routers struggle with many at once" htmlFor="s-connrate">
          <AmountInput id="s-connrate" value={draft.connectRate} unit="per second" placeholder="No limit" onChange={(connectRate) => update({ connectRate })} />
        </SettingRow>
      </SettingCard>

      <SettingCard
        title="Encryption"
        description="Hides BitTorrent traffic from network inspection and ISP throttling. It doesn't prove who a peer is; your data is safe either way, since every piece is checked against the torrent's hashes. New connections use a change."
      >
        <ChoiceCards label="Encryption" columns={2} value={draft.encryption} options={ENCRYPTION} onChange={(encryption) => update({ encryption })} />
      </SettingCard>

      <ProxyCard value={draft.proxy} onChange={(proxy) => update({ proxy })} />
    </>
  );
}

const PROXY_TYPES: readonly Choice<ProxyType>[] = [
  { value: "none", label: "No proxy", description: "Every connection goes straight to the peer or tracker." },
  { value: "socks5", label: "SOCKS5", description: "With or without a username and password. Carries UDP trackers too." },
  { value: "http", label: "HTTP", description: "A proxy that allows CONNECT, with or without a username and password. No UDP trackers." },
  { value: "socks4", label: "SOCKS4", description: "IPv4 peers only, and no UDP trackers. A username is sent as the user ID; there is no password." },
];

function ProxyCard({ value: p, onChange }: { value: ProxySettings; onChange: (v: ProxySettings) => void }) {
  const set = (patch: Partial<ProxySettings>) => onChange({ ...p, ...patch });
  const on = p.type !== "none";
  return (
    <SettingCard
      title="Proxy"
      description="Send peer, web seed and tracker connections through a SOCKS or HTTP proxy. A change applies to new connections at once."
    >
      <ChoiceCards label="Proxy type" columns={2} value={p.type} options={PROXY_TYPES} onChange={(type) => set({ type })} />
      {on && (
        <>
          <SettingRow label="Host" description="The proxy's name or address" htmlFor="s-proxy-host">
            <input id="s-proxy-host" className="s-text" value={p.host} placeholder="proxy.example.com" spellCheck={false} autoComplete="off" onChange={(e) => set({ host: e.target.value })} />
          </SettingRow>
          <SettingRow label="Port" htmlFor="s-proxy-port">
            <AmountInput id="s-proxy-port" value={p.port} max={65535} placeholder="1080" onChange={(port) => set({ port })} />
          </SettingRow>
          <SettingRow label="Username" description="Empty for a proxy without one" htmlFor="s-proxy-user">
            <input id="s-proxy-user" className="s-text" value={p.username} spellCheck={false} autoComplete="off" onChange={(e) => set({ username: e.target.value })} />
          </SettingRow>
          {p.type !== "socks4" && <ProxyPassword value={p} onChange={set} />}
          <ToggleRow
            label="Resolve host names through the proxy"
            description="Tracker names are looked up by the proxy, so they never reach this server's DNS. HTTP proxies always do this for HTTP trackers."
            checked={p.proxyHostnames}
            onChange={(proxyHostnames) => set({ proxyHostnames })}
          />
          <ToggleRow
            label="Peer connections"
            description={p.force ? "Always, with force proxy." : "Peers and web seeds go through the proxy, over TCP only (uTP can't)."}
            checked={p.proxyPeers || p.force}
            disabled={p.force}
            onChange={(proxyPeers) => set({ proxyPeers })}
          />
          <ToggleRow
            label="Tracker connections"
            description={p.force ? "Always, with force proxy." : "Announces and scrapes go through the proxy."}
            checked={p.proxyTrackers || p.force}
            disabled={p.force}
            onChange={(proxyTrackers) => set({ proxyTrackers })}
          />
          <ToggleRow
            label="Force proxy"
            description="Everything that can go through the proxy does, and the rest is off: no incoming connections, DHT, local peer discovery, uTP or port forwarding."
            checked={p.force}
            onChange={(force) => set({ force })}
          />
          {p.force && (
            <Note tone="warn">
              With no incoming connections, peers who can&apos;t accept connections either can&apos;t reach you, so some torrents find fewer peers. Turning force proxy off
              brings incoming connections, DHT, local peer discovery, uTP and port forwarding back when the daemon next restarts.
            </Note>
          )}
          {p.type !== "socks5" && p.proxyTrackers && <Note>UDP trackers are skipped while trackers go through {p.type === "http" ? "an HTTP" : "a SOCKS4"} proxy.</Note>}
        </>
      )}
    </SettingCard>
  );
}

/** The proxy password: write-only. The server says only whether one is stored; leaving the box
    empty keeps it, and Remove clears it. */
function ProxyPassword({ value: p, onChange }: { value: ProxySettings; onChange: (patch: Partial<ProxySettings>) => void }) {
  const removing = p.password === "";
  const state = removing ? "will be removed" : p.password !== undefined ? "will be replaced" : p.passwordSet ? "saved" : "none";
  return (
    <SettingRow label="Password" description={`Stored, never shown (${state})`} htmlFor="s-proxy-pass">
      <span className="s-inline">
        <input
          id="s-proxy-pass"
          className="s-text"
          type="password"
          autoComplete="new-password"
          value={removing ? "" : (p.password ?? "")}
          placeholder={p.passwordSet && !removing ? "Leave empty to keep it" : ""}
          onChange={(e) => onChange({ password: e.target.value === "" ? undefined : e.target.value })}
        />
        {p.passwordSet && !removing && (
          <button type="button" onClick={() => onChange({ password: "" })}>
            Remove
          </button>
        )}
        {removing && (
          <button type="button" onClick={() => onChange({ password: undefined })}>
            Keep
          </button>
        )}
      </span>
    </SettingRow>
  );
}

// Files

const NAMING: readonly Choice<Settings["naming"]>[] = [
  {
    value: "portable",
    label: "Portable",
    description: "Names that are safe on every OS, so the files can be shared over SMB or moved to an exFAT drive unchanged.",
  },
  {
    value: "native",
    label: "Native",
    description: (
      <>
        Change only what this filesystem forbids, keeping names like <code>a:b</code> where it allows them.
      </>
    ),
  },
];

/** The orders anyone can pick, plus a torrent's own list when that is what's set. */
export function FileOrderSelect({ id, value, onChange }: { id?: string; value: FileOrder; onChange: (v: FileOrder) => void }) {
  return (
    <select id={id} value={value.by} onChange={(e) => onChange(fileOrderOf(e.target.value as FileOrderBy, value))}>
      {FILE_ORDERS.map(([by, words]) => (
        <option key={by} value={by}>
          {words}
        </option>
      ))}
      {value.by === "manual" && <option value="manual">{describeFileOrder(value)}</option>}
    </select>
  );
}

/** Whether completed files move, and where: the instance's default, or a group's own. */
function MoveRows({ value, onChange, idPrefix }: { value: MoveCompleted; onChange: (v: MoveCompleted) => void; idPrefix: string }) {
  return (
    <ToggleRow label="Move completed files" description={value.enabled ? undefined : "Off: files stay in the save folder"} checked={value.enabled} onChange={(enabled) => onChange({ ...value, enabled })}>
      <div className="s-row s-row-wide">
        <MoveCompletedFields idPrefix={idPrefix} value={value} onChange={onChange} />
      </div>
    </ToggleRow>
  );
}

export function FilesSection({ draft, update, serverAdmin }: DraftProps) {
  return (
    <>
      <SettingCard title="File names" description="For new torrents. Torrents already added keep their names.">
        <ChoiceCards label="File names" columns={2} value={draft.naming} options={NAMING} onChange={(naming) => update({ naming })} />
      </SettingCard>
      <SettingCard
        title="Download order"
        description="Which of a torrent's files to download first. A preference, not a rule: peers that don't have the earlier files still send later ones, and a file's priority comes first. A group, or a torrent in its details, can set its own."
      >
        <SettingRow label="Order" htmlFor="s-file-order">
          <FileOrderSelect id="s-file-order" value={draft.fileOrder} onChange={(fileOrder) => update({ fileOrder })} />
        </SettingRow>
      </SettingCard>
      <SettingCard
        title="Moving completed files"
        description="Move files out of the save folder when they finish, into a shared folder say. Moved files keep seeding from their new place. A group, or a torrent in its details, can set its own."
      >
        <MoveRows idPrefix="s-move" value={draft.moveCompleted} onChange={(moveCompleted) => update({ moveCompleted })} />
      </SettingCard>
      {serverAdmin && (
        <SettingCard
          title="Storage roots"
          description={
            <>
              Where other computers reach each download folder over the network: <code>\\nas\torrents</code>, <code>smb://</code> or <code>nfs://</code> (the
              desktop app suggests its folder mappings from these), or a web server listing its files, such as{" "}
              <code>https://files.example.com/downloads/</code> (files get download links; the first web address is used). Every user can see these, so
              don&apos;t put passwords here. Each folder saves on its own.
            </>
          }
        >
          <StorageRoots />
        </SettingCard>
      )}
    </>
  );
}

// Library

const DEDUPE: readonly Choice<Settings["library"]["dedupe"]>[] = [
  { value: "on", label: "On", description: "Replace the extra copies with links to one." },
  { value: "report", label: "Report only", description: "Show how much space it would free." },
  { value: "off", label: "Off" },
];

export function LibrarySection({ draft, update, saved, serverAdmin }: DraftProps) {
  const lib = draft.library;
  const set = (patch: Partial<Settings["library"]>) => update({ library: { ...lib, ...patch } });
  return (
    <>
      <SettingCard title="Reuse files" description="Before downloading, look for the same files among other torrents and library folders.">
        <ToggleRow
          label="Reuse files other torrents already have"
          description="Hard-linked when they're on the same drive, so they take no extra space."
          checked={lib.enabled}
          onChange={(enabled) => set({ enabled })}
        >
          <ToggleRow label="Copy them from another drive" description="Otherwise those are downloaded." checked={lib.allowCopy} onChange={(allowCopy) => set({ allowCopy })} />
          <ToggleRow
            label="Reuse files that match only in places"
            description="A repack, say: a copy is made and only the parts that differ are downloaded."
            checked={lib.partialReuse}
            onChange={(partialReuse) => set({ partialReuse })}
          />
        </ToggleRow>
        <ToggleRow
          label="Retire torrents a pack has replaced"
          description="Once their tracker drops them. Their files stay."
          checked={lib.retireSuperseded}
          onChange={(retireSuperseded) => set({ retireSuperseded })}
        />
      </SettingCard>

      <SettingCard title="Share between running torrents">
        <SettingRow
          label="Share up to"
          description="Torrents downloading the same file share each piece as it arrives, checked against their own hashes, so it is downloaded once. Empty turns it off."
          htmlFor="s-share"
        >
          <RateInput id="s-share" value={lib.shareRate} placeholder="Off" onChange={(shareRate) => set({ shareRate })} />
        </SettingRow>
      </SettingCard>

      <SettingCard
        title="Merge identical files"
        description="Identical copies on one drive that torrents downloaded separately become hard links to one file. Each torrent keeps its own path; a file is only replaced after it is read again and found identical."
        footer={serverAdmin ? <DedupeStatus savedOff={saved.library.dedupe === "off"} /> : undefined}
      >
        <ChoiceCards label="Merge identical files" columns={3} value={lib.dedupe} options={DEDUPE} onChange={(dedupe) => set({ dedupe })} />
        <SettingRow
          label="Read in idle time at up to"
          description="For dedupe, and to fingerprint v1 torrents' files so a later v2 torrent of the same file is found without reading it again. Empty turns it off."
          htmlFor="s-fingerprint"
        >
          <RateInput id="s-fingerprint" value={lib.fingerprintRate} placeholder="Off" onChange={(fingerprintRate) => set({ fingerprintRate })} />
        </SettingRow>
      </SettingCard>

      <SettingCard
        title="Library folders"
        description="Folders of files outside torrents, such as a media folder. New torrents reuse matching files from them. apitorrent only reads them: it never changes, moves or deletes their files."
      >
        <SettingRow label="Look through them again every" description="Empty: only when you press Rescan" htmlFor="s-rescan">
          <AmountInput id="s-rescan" value={lib.folderRescanHours} unit="hours" placeholder="Never" onChange={(folderRescanHours) => set({ folderRescanHours })} />
        </SettingRow>
        {serverAdmin && (
          <div className="s-row s-row-wide">
            <LibraryFolders />
          </div>
        )}
      </SettingCard>
    </>
  );
}

// Settings sections that edit the instance's settings draft: saved together with the page's
// Save bar. A few hold admin-only parts that act at once (storage roots, library folders).

import { useEffect, useState } from "react";

import type { GroupSettings } from "../api/generated/GroupSettings";
import type { SeedLimitAction } from "../api/generated/SeedLimitAction";
import type { SeedLimits } from "../api/generated/SeedLimits";
import type { Settings } from "../api/generated/Settings";
import { DedupeStatus, LibraryFolders } from "../components/LibrarySection";
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

const NEW_GROUP: GroupSettings = { name: "", labels: [], downloadRate: 0, uploadRate: 0, maxConnections: 0, seedLimits: null };

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
  defaults: SeedLimits;
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
            description={g.seedLimits ? undefined : `Off: follows the default (${describeSeedLimits(defaults).toLowerCase()})`}
            checked={g.seedLimits !== null}
            onChange={(on) => onChange({ seedLimits: on ? { ...defaults } : null })}
          >
            {g.seedLimits && <SeedLimitRows idPrefix={`${p}-seed`} value={g.seedLimits} onChange={(seedLimits) => onChange({ seedLimits })} />}
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
          defaults={draft.seedLimits}
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
    </>
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

export function FilesSection({ draft, update, serverAdmin }: DraftProps) {
  return (
    <>
      <SettingCard title="File names" description="For new torrents. Torrents already added keep their names.">
        <ChoiceCards label="File names" columns={2} value={draft.naming} options={NAMING} onChange={(naming) => update({ naming })} />
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

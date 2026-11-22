import { useEffect, useState } from "react";

import { api } from "../api/client";
import type { FileOrder } from "../api/generated/FileOrder";
import type { MoveCompleted } from "../api/generated/MoveCompleted";
import type { TorrentDetail } from "../api/generated/TorrentDetail";
import type { TorrentLimits } from "../api/generated/TorrentLimits";
import type { TorrentFile } from "../api/generated/TorrentFile";
import type { TorrentSummary } from "../api/generated/TorrentSummary";
import type { UnwantedPolicy } from "../api/generated/UnwantedPolicy";
import { describeError } from "../auth/flow";
import { PathActions } from "../desktop/PathActions";
import { describeReuse } from "../library";
import { formatBytes, formatDateTime, formatPercent, formatRate, formatRatio, formatUtc, fromKiB, STATE_LABEL, toKiB } from "../format";
import { describeSeedLimits, formatSeedingTime, SeedLimitsFields } from "./SeedLimitsFields";
import { FILE_ORDERS, NO_MOVE, describeFileOrder, describeMove, fileOrderOf, moveProblems, splitMovedPath, trimMove, type FileOrderBy } from "./fileSettings";
import { MoveCompletedFields } from "./MoveCompletedFields";
import { Tip } from "./Tip";
import { WebLink } from "./WebLink";
import { badUrls, duplicateUrls, fromDrafts, moveTier, newTier, sameTiers, toDrafts, type TierDraft } from "./trackerTiers";

type Tab = "general" | "files" | "peers" | "trackers";

const TAB_NAMES: Record<Tab, string> = { general: "General", files: "Files", peers: "Peers", trackers: "Trackers" };

const PRIORITIES: Array<[number, string]> = [
  [0, "Skip"],
  [1, "Low"],
  [4, "Normal"],
  [7, "High"],
];

/** The listed priority nearest a file's own. */
function nearestPriority(priority: number): number {
  return PRIORITIES.reduce((best, [p]) => (Math.abs(p - priority) < Math.abs(best - priority) ? p : best), 4);
}

/** Fetch the detail for `id`, refreshing while it is shown. */
function useDetail(id: string, revision: unknown) {
  const [detail, setDetail] = useState<TorrentDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Bumped after an action whose effect (a file moving, say) doesn't change the summary.
  const [nudge, setNudge] = useState(0);
  useEffect(() => {
    let cancelled = false;
    api.detail(id).then(
      (d) => {
        if (!cancelled) {
          setDetail(d);
          setError(null);
        }
      },
      (e: unknown) => {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      },
    );
    return () => {
      cancelled = true;
    };
    // Re-fetch whenever the torrent's summary changes; the server pushes those live.
  }, [id, revision, nudge]);
  /** Re-fetch now and again shortly, since file moves finish in the background. */
  const refetch = () => {
    setNudge((n) => n + 1);
    setTimeout(() => setNudge((n) => n + 1), 600);
  };
  return { detail, error, refetch };
}

function Row({ label, value }: { label: string; value: React.ReactNode }) {
  return (
    <>
      <dt>{label}</dt>
      <dd>{value}</dd>
    </>
  );
}

/**
 * Above every tab while Deluge still has the torrent: it was imported, but stays paused here
 * so the two clients don't both run it. Starting it here pauses it in Deluge first.
 */
function InDelugeBanner({ summary, readOnly }: { summary: TorrentSummary; readOnly: boolean }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  async function moveOver() {
    setBusy(true);
    setError(null);
    try {
      // Resuming an imported torrent pauses it in Deluge first, and refuses if it can't.
      await api.resume(summary.id);
    } catch (e) {
      setError(describeError(e));
    } finally {
      setBusy(false);
    }
  }
  return (
    <div className="banner-deluge" role="status">
      <p>
        <strong>Still in Deluge.</strong> Imported from <code>{summary.inDeluge}</code>; Deluge keeps running it, so it stays paused here.
        Starting it here pauses it in Deluge first.
      </p>
      {!readOnly && (
        <button type="button" disabled={busy} onClick={() => void moveOver()}>
          {busy ? "Pausing in Deluge…" : "Pause in Deluge, start here"}
        </button>
      )}
      {error && <p className="error-text">{error}</p>}
    </div>
  );
}

/** A local time with UTC beside it in smaller text. */
function When({ ms }: { ms: number }) {
  return (
    <>
      {formatDateTime(ms)} <small className="muted">{formatUtc(ms)}</small>
    </>
  );
}

function copy(text: string) {
  void navigator.clipboard?.writeText(text);
}

/** What the location column says about a file that isn't simply in the save directory. */
function location(f: TorrentFile): React.ReactNode {
  if (f.movedTo !== null) {
    return <span className="chip">Moved</span>;
  }
  if (f.placement === "held") {
    return <span className="chip chip-held">Held</span>;
  }
  if (f.placement === "parts") {
    return <span className="chip">Not in folder</span>;
  }
  return null;
}

/** Where a completed file was moved; it seeds from there. */
function MovedTo({ path }: { path: string }) {
  const { dir, name } = splitMovedPath(path);
  return (
    <div className="file-note muted">
      Moved to <code>{path}</code>, seeding from there
      <PathActions savePath={dir} file={name} />
      <WebLink savePath={dir} path={name}>
        Download
      </WebLink>
    </div>
  );
}

/** Names the torrent that took over this one's files. */
function SupersededBy({ id }: { id: string }) {
  const [name, setName] = useState<string | null>(null);
  useEffect(() => {
    api.detail(id).then(
      (d) => setName(d.summary.name),
      () => setName(null),
    );
  }, [id]);
  return (
    <span>
      All of its files are in <strong>{name ?? id}</strong>. It keeps seeding until its tracker drops it.
    </span>
  );
}

function LabelEditor({ id, label, readOnly }: { id: string; label: string | null; readOnly: boolean }) {
  const [value, setValue] = useState(label ?? "");
  useEffect(() => setValue(label ?? ""), [label]);
  const dirty = value.trim() !== (label ?? "");
  if (readOnly) return label ?? <span className="muted">none</span>;
  return (
    <form
      className="inline-edit"
      onSubmit={(e) => {
        e.preventDefault();
        void api.setLabel(id, value.trim() || null);
      }}
    >
      <input aria-label="Label" value={value} placeholder="none" onChange={(e) => setValue(e.target.value)} />
      {dirty && <button type="submit">Save</button>}
    </form>
  );
}

function describeLimits(l: TorrentLimits): string {
  const parts = [];
  if (l.downloadRate > 0) parts.push(`↓ ${formatRate(l.downloadRate)}`);
  if (l.uploadRate > 0) parts.push(`↑ ${formatRate(l.uploadRate)}`);
  if (l.maxConnections != null) parts.push(`${l.maxConnections} peers`);
  return parts.length > 0 ? parts.join(", ") : "None of its own";
}

function LimitsEditor({ id, limits, refetch, readOnly }: { id: string; limits: TorrentLimits; refetch: () => void; readOnly: boolean }) {
  const [editing, setEditing] = useState(false);
  const [down, setDown] = useState(toKiB(limits.downloadRate));
  const [up, setUp] = useState(toKiB(limits.uploadRate));
  const [peers, setPeers] = useState(limits.maxConnections == null ? "" : String(limits.maxConnections));
  if (!editing) {
    return (
      <span className="inline-edit">
        {describeLimits(limits)}
        {!readOnly && (
          <button type="button" className="link" onClick={() => setEditing(true)}>
            Change
          </button>
        )}
      </span>
    );
  }
  return (
    <form
      className="inline-edit"
      onSubmit={(e) => {
        e.preventDefault();
        const maxConnections = peers.trim() === "" ? null : Math.max(1, Math.floor(Number(peers) || 1));
        void api.setLimits(id, { downloadRate: fromKiB(down), uploadRate: fromKiB(up), maxConnections }).then(refetch);
        setEditing(false);
      }}
    >
      <label>
        ↓ <input type="number" min={0} aria-label="Download limit in KiB/s" value={down || ""} placeholder="KiB/s" onChange={(e) => setDown(Number(e.target.value) || 0)} />
      </label>
      <label>
        ↑ <input type="number" min={0} aria-label="Upload limit in KiB/s" value={up || ""} placeholder="KiB/s" onChange={(e) => setUp(Number(e.target.value) || 0)} />
      </label>
      <label>
        Peers <input type="number" min={1} aria-label="Most peers" value={peers} placeholder="default" onChange={(e) => setPeers(e.target.value)} />
      </label>
      <button type="submit">Save</button>
      <button type="button" onClick={() => setEditing(false)}>
        Cancel
      </button>
      <span className="muted">Rates in KiB/s; empty means no limit of its own.</span>
    </form>
  );
}

function SeedLimitsEditor({ id, detail, refetch, readOnly }: { id: string; detail: TorrentDetail; refetch: () => void; readOnly: boolean }) {
  const [editing, setEditing] = useState(false);
  const [own, setOwn] = useState(detail.seedLimits !== null);
  const [limits, setLimits] = useState(detail.seedLimits ?? detail.effectiveSeedLimits);
  if (!editing) {
    return (
      <span className="inline-edit">
        <span>
          {describeSeedLimits(detail.effectiveSeedLimits)}
          <span className="muted"> ({detail.seedLimits ? "its own" : "from its group or the settings"})</span>
          {detail.seedLimitReached && <span className="muted">. Reached; it seeds on until these change</span>}
        </span>
        {!readOnly && (
          <button
            type="button"
            className="link"
            onClick={() => {
              setOwn(detail.seedLimits !== null);
              setLimits(detail.seedLimits ?? detail.effectiveSeedLimits);
              setEditing(true);
            }}
          >
            Change
          </button>
        )}
      </span>
    );
  }
  return (
    <form
      className="seed-limits-form"
      onSubmit={(e) => {
        e.preventDefault();
        void api.setSeedLimits(id, own ? limits : null).then(refetch);
        setEditing(false);
      }}
    >
      <label className="check">
        <input type="checkbox" checked={own} onChange={(e) => setOwn(e.target.checked)} />
        Its own limits (otherwise its group&apos;s, or the settings&apos;)
      </label>
      {own && <SeedLimitsFields idPrefix={`torrent-seed`} value={limits} onChange={setLimits} />}
      <span className="inline-edit">
        <button type="submit">Save</button>
        <button type="button" onClick={() => setEditing(false)}>
          Cancel
        </button>
      </span>
    </form>
  );
}

/** "(its own)" or "(from its group or the settings)", after a setting a torrent may override. */
function whose(own: boolean) {
  return <span className="muted"> ({own ? "its own" : "from its group or the settings"})</span>;
}

/** The torrent's file order: its own, or its group's or the settings'. Saved on change. */
function FileOrderEditor({ id, detail, refetch, readOnly }: { id: string; detail: TorrentDetail; refetch: () => void; readOnly: boolean }) {
  const [error, setError] = useState<string | null>(null);
  const own = detail.fileOrder;
  const tip = (
    <Tip label="About download order">
      Which files are downloaded first. A preference, not a rule: peers that don&apos;t have the earlier files still send later ones, and a file&apos;s
      priority comes first. To watch a file while it downloads, tick <strong>Download in order</strong> on it under Files.
    </Tip>
  );
  if (readOnly) {
    return (
      <span>
        {describeFileOrder(detail.effectiveFileOrder)}
        {whose(own !== null)}
        {tip}
      </span>
    );
  }
  const set = async (order: FileOrder | null) => {
    setError(null);
    try {
      await api.setFileOrder(id, order);
      refetch();
    } catch (e) {
      setError(describeError(e));
    }
  };
  return (
    <span className="inline-edit">
      <select
        aria-label="Download order"
        value={own?.by ?? "inherit"}
        onChange={(e) => void set(e.target.value === "inherit" ? null : fileOrderOf(e.target.value as FileOrderBy, own))}
      >
        <option value="inherit">
          Like its group or the settings{own === null ? ` (${describeFileOrder(detail.effectiveFileOrder).toLowerCase()})` : ""}
        </option>
        {FILE_ORDERS.map(([by, words]) => (
          <option key={by} value={by}>
            {words}
          </option>
        ))}
        {own?.by === "manual" && <option value="manual">{describeFileOrder(own)}</option>}
      </select>
      {tip}
      {error && <span className="error-text">{error}</span>}
    </span>
  );
}

type MoveMode = "inherit" | "off" | "own";

/** Moving the torrent's completed files: like its group or the settings, never, or its own way. */
function MoveCompletedEditor({ id, detail, refetch, readOnly }: { id: string; detail: TorrentDetail; refetch: () => void; readOnly: boolean }) {
  const [editing, setEditing] = useState(false);
  const [mode, setMode] = useState<MoveMode>("inherit");
  const [value, setValue] = useState<MoveCompleted>(NO_MOVE);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  if (!editing) {
    return (
      <span className="inline-edit">
        <span>
          {describeMove(detail.effectiveMoveCompleted)}
          {whose(detail.moveCompleted !== null)}
        </span>
        {!readOnly && (
          <button
            type="button"
            className="link"
            onClick={() => {
              const own = detail.moveCompleted;
              setMode(own === null ? "inherit" : own.enabled ? "own" : "off");
              setValue({ ...(own ?? detail.effectiveMoveCompleted), enabled: true });
              setError(null);
              setEditing(true);
            }}
          >
            Change
          </button>
        )}
      </span>
    );
  }
  const problems = mode === "own" ? moveProblems(value) : [];
  const save = async () => {
    // "Leave in place" keeps what was typed for next time, unless the daemon would refuse it.
    const settings =
      mode === "inherit" ? null
      : mode === "off" ? (moveProblems(value).length === 0 ? { ...trimMove(value), enabled: false } : NO_MOVE)
      : trimMove(value);
    setBusy(true);
    setError(null);
    try {
      await api.setMoveCompleted(id, settings);
      setEditing(false);
      refetch();
    } catch (e) {
      setError(describeError(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <form
      className="move-editor"
      onSubmit={(e) => {
        e.preventDefault();
        void save();
      }}
    >
      <select aria-label="Moving completed files" value={mode} onChange={(e) => setMode(e.target.value as MoveMode)}>
        <option value="inherit">Like its group or the settings</option>
        <option value="off">Leave its files where they are</option>
        <option value="own">Its own setting</option>
      </select>
      {mode === "own" && (
        <MoveCompletedFields
          idPrefix="torrent-move"
          value={value}
          onChange={setValue}
          note="For one torrent, destinations must be inside one of the space's storage roots."
        />
      )}
      <span className="inline-edit">
        <button type="submit" disabled={busy || problems.length > 0}>
          Save
        </button>
        <button type="button" onClick={() => setEditing(false)}>
          Cancel
        </button>
      </span>
      {error && <p className="error-text">{error}</p>}
    </form>
  );
}

function PeersTab({ detail }: { detail: TorrentDetail }) {
  if (detail.peers.length === 0) {
    return <p className="muted">No peers connected.</p>;
  }
  return (
    <>
      <table className="files">
        <thead>
          <tr>
            <th>Address</th>
            <th>Client</th>
            <th>Progress</th>
            <th className="end">Down</th>
            <th className="end">Up</th>
            <th>Flags</th>
          </tr>
        </thead>
        <tbody>
          {detail.peers.map((p) => (
            <tr key={p.address}>
              <td className="cell-path">
                <code>{p.address}</code>
              </td>
              <td>{p.client || <span className="muted">unknown</span>}</td>
              <td>{formatPercent(p.progress)}</td>
              <td className="end">{formatRate(p.downloadRate)}</td>
              <td className="end">{formatRate(p.uploadRate)}</td>
              <td>
                <code>{p.flags}</code>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      <p className="hint muted">
        Flags: O we connected to it, C we choke it, I we want its pieces, c it chokes us, i it wants ours, P on parole (suspected of bad data, given whole
        pieces only), U over uTP, E encrypted.
      </p>
    </>
  );
}

/** This space's tracker list for the torrent: tiers of announce URLs, edited as a whole. */
function TrackersEditor({ id, tiers, onDone }: { id: string; tiers: string[][]; onDone: (saved: boolean) => void }) {
  const [drafts, setDrafts] = useState<TierDraft[]>(() => (tiers.length > 0 ? toDrafts(tiers) : [newTier()]));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const result = fromDrafts(drafts);
  const twice = duplicateUrls(result);
  const bad = badUrls(result);
  const problem =
    twice.length > 0 ? `Listed twice: ${twice.join(", ")}` : bad.length > 0 ? `Not an http, https or udp tracker URL: ${bad.join(", ")}` : null;
  const set = (key: number, text: string) => setDrafts((ds) => ds.map((d) => (d.key === key ? { ...d, text } : d)));

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.setTrackers(id, result);
      onDone(true);
    } catch (e) {
      setError(describeError(e));
      setBusy(false);
    }
  };

  return (
    <form
      className="tracker-editor"
      aria-label="Edit trackers"
      onSubmit={(e) => {
        e.preventDefault();
        void save();
      }}
    >
      <p className="muted">
        One announce URL per line. Each tier is tried in turn, and within a tier one tracker at a time until one answers. Only this space&apos;s copy of the
        torrent changes; a tracker another space&apos;s copy also lists is announced to once, by the copy added first.
      </p>
      <ol className="tracker-tiers">
        {drafts.map((d, i) => (
          <li key={d.key} className="tracker-tier">
            <div className="tracker-tier-head">
              <strong>Tier {i + 1}</strong>
              <span className="tracker-tier-actions">
                <button type="button" aria-label={`Move tier ${i + 1} up`} disabled={i === 0} onClick={() => setDrafts((ds) => moveTier(ds, i, -1))}>
                  ↑
                </button>
                <button
                  type="button"
                  aria-label={`Move tier ${i + 1} down`}
                  disabled={i === drafts.length - 1}
                  onClick={() => setDrafts((ds) => moveTier(ds, i, 1))}
                >
                  ↓
                </button>
                <button type="button" aria-label={`Remove tier ${i + 1}`} onClick={() => setDrafts((ds) => ds.filter((x) => x.key !== d.key))}>
                  Remove
                </button>
              </span>
            </div>
            <textarea
              aria-label={`Tier ${i + 1} trackers`}
              rows={Math.max(2, d.text.split("\n").length)}
              spellCheck={false}
              autoCapitalize="off"
              autoCorrect="off"
              placeholder="udp://tracker.example:6969/announce"
              value={d.text}
              onChange={(e) => set(d.key, e.target.value)}
            />
          </li>
        ))}
      </ol>
      <div className="tracker-editor-buttons">
        <button type="button" onClick={() => setDrafts((ds) => [...ds, newTier()])}>
          Add tier
        </button>
        <span className="spacer" />
        <button type="button" onClick={() => onDone(false)} disabled={busy}>
          Cancel
        </button>
        <button type="submit" className="primary" disabled={busy || problem !== null || sameTiers(result, tiers)}>
          Save
        </button>
      </div>
      {result.length === 0 && <p className="muted">With no trackers, peers come only from the DHT, peer exchange and the local network.</p>}
      {problem && <p className="error-text">{problem}</p>}
      {error && <p className="error-text">{error}</p>}
    </form>
  );
}

function TrackersTab({ detail, id, mayEdit, refetch }: { detail: TorrentDetail; id: string; mayEdit: boolean; refetch: () => void }) {
  const [editing, setEditing] = useState(false);
  if (editing) {
    return (
      <TrackersEditor
        id={id}
        tiers={detail.trackers}
        onDone={(saved) => {
          setEditing(false);
          if (saved) refetch();
        }}
      />
    );
  }
  const edit = mayEdit && (
    <p>
      <button type="button" onClick={() => setEditing(true)}>
        Edit trackers
      </button>
    </p>
  );
  if (detail.trackerStatus.length === 0) {
    return (
      <>
        <p className="muted">No trackers. Peers come from other sources.</p>
        {edit}
      </>
    );
  }
  return (
    <>
    {edit}
    <table className="files">
      <thead>
        <tr>
          <th>Tier</th>
          <th>Tracker</th>
          <th>Status</th>
        </tr>
      </thead>
      <tbody>
        {detail.trackerStatus.map((t) => (
          <tr key={`${t.tier}-${t.url}`}>
            <td>{t.tier + 1}</td>
            <td className="cell-path">
              <code>{t.url}</code>
            </td>
            <td>
              {t.error ? (
                <span className="error-text">{t.error}</span>
              ) : t.peers != null ? (
                <span className="ok-text">
                  {t.peers} peers{t.seeders != null && `, ${t.seeders} seeds, ${t.leechers ?? 0} leechers`}
                </span>
              ) : (
                <span className="muted">Not announced yet</span>
              )}
              {t.warning && <div className="muted">{t.warning}</div>}
            </td>
          </tr>
        ))}
      </tbody>
    </table>
    </>
  );
}

function FilesTab({ summary, detail, refetch, readOnly }: { summary: TorrentSummary; detail: TorrentDetail; refetch: () => void; readOnly: boolean }) {
  const held = detail.files.filter((f) => f.placement === "held");
  const heldBytes = held.reduce((n, f) => n + f.length, 0);
  const anyNotInFolder = detail.files.some((f) => f.placement !== "save");
  const [error, setError] = useState<string | null>(null);
  const setPolicy = (policy: UnwantedPolicy) => {
    void api.setUnwantedPolicy(summary.id, policy).then(refetch);
  };
  const setSequential = async (f: TorrentFile, sequential: boolean) => {
    setError(null);
    try {
      // From the start: a player reading the file on disk asks for its own bytes.
      await api.setSequential(summary.id, [{ index: f.index, sequential, from: 0 }]);
      refetch();
    } catch (e) {
      setError(describeError(e));
    }
  };
  return (
    <>
      <div className="files-bar">
        {readOnly ? (
          <span className="inline-field">
            <span>Deselected files with data:</span> {detail.unwantedPolicy === "hold" ? "moved to the held area" : "deleted"}
          </span>
        ) : (
          <label className="inline-field">
            <span>Deselected files with data:</span>
            <select aria-label="What happens to deselected files" value={detail.unwantedPolicy} onChange={(e) => setPolicy(e.target.value as UnwantedPolicy)}>
              <option value="hold">Move to the held area</option>
              <option value="delete">Delete</option>
            </select>
          </label>
        )}
        {!readOnly && held.length > 0 && (
          <button type="button" onClick={() => void api.releaseHeld(summary.id).then(refetch)}>
            Delete held files ({held.length}, {formatBytes(heldBytes)})
          </button>
        )}
      </div>
      {anyNotInFolder && (
        <p className="hint muted">
          Deselected files are never in the save folder. <strong>Held</strong> files are kept by apitorrent and come back instantly when selected again.{" "}
          <strong>Not in folder</strong> files keep only the bytes they share with wanted files.
        </p>
      )}
      {error && <p className="error-text">{error}</p>}
      <table className="files">
        <thead>
          <tr>
            <th>
              File
              {!readOnly && (
                <Tip label="About downloading in order">
                  <strong>Download in order</strong> fetches a file front to back, so you can watch or read it while it downloads. Its first and last parts
                  come first, since players need them. The rest of the torrent keeps downloading around it.
                </Tip>
              )}
            </th>
            <th className="end">Size</th>
            <th>Progress</th>
            <th>Location</th>
            <th>Priority</th>
          </tr>
        </thead>
        <tbody>
          {detail.files.map((f) => (
            <tr key={f.index}>
              <td className="cell-path">
                {f.path}
                {f.placement === "save" && f.movedTo === null && <PathActions savePath={summary.savePath} file={f.path} />}
                {/* Only whole files: a partial one would download with holes. */}
                {f.placement === "save" && f.progress >= 1 && f.movedTo === null && (
                  <WebLink savePath={summary.savePath} path={f.path}>
                    Download
                  </WebLink>
                )}
                {f.reused && <div className="file-reuse muted">{describeReuse(f.reused, f.length)}</div>}
                {f.movedTo !== null && <MovedTo path={f.movedTo} />}
                {f.moveError && <div className="file-note error-text">Couldn&apos;t move it: {f.moveError}</div>}
                {readOnly && f.sequential && <div className="file-note muted">Downloading in order</div>}
                {/* Only while it matters: a finished file has nothing left to fetch in order. */}
                {!readOnly && f.priority > 0 && (f.progress < 1 || f.sequential) && (
                  <div>
                    <label className="file-sequential">
                      <input type="checkbox" checked={f.sequential} onChange={(e) => void setSequential(f, e.target.checked)} />
                      Download in order
                    </label>
                  </div>
                )}
              </td>
              <td className="end">{formatBytes(f.length)}</td>
              <td>{formatPercent(f.progress)}</td>
              <td>{location(f)}</td>
              <td>
                {readOnly ? (
                  PRIORITIES.find(([p]) => p === nearestPriority(f.priority))?.[1]
                ) : (
                  <select
                    aria-label={`Priority of ${f.path}`}
                    value={nearestPriority(f.priority)}
                    onChange={(e) => {
                      void api.setFilePriorities(summary.id, [{ index: f.index, priority: Number(e.target.value) }]).then(refetch);
                    }}
                  >
                    {PRIORITIES.map(([p, name]) => (
                      <option key={p} value={p}>{name}</option>
                    ))}
                  </select>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </>
  );
}

/**
 * `sheet`: on a phone, where the details cover the screen and `onClose` goes back. Otherwise
 * `onClose` and `onPopOut` add close and open-in-a-window buttons to the tab bar. `readOnly`:
 * the caller is a viewer here, so values show as text without their editors.
 */
export function DetailPane({
  summary,
  sheet = false,
  onClose,
  onPopOut,
  readOnly = false,
}: {
  summary: TorrentSummary;
  sheet?: boolean;
  onClose?: () => void;
  onPopOut?: () => void;
  readOnly?: boolean;
}) {
  const [tab, setTab] = useState<Tab>("general");
  const { detail, error, refetch } = useDetail(summary.id, summary);

  return (
    <section className={`detail ${sheet ? "detail-sheet" : ""}`} aria-label={`Details of ${summary.name}`}>
      {sheet && onClose && (
        <div className="sheet-head">
          <button type="button" onClick={onClose}>
            ← Back
          </button>
          <span className="sheet-title">{summary.name}</span>
        </div>
      )}
      <div className="tabs" role="tablist">
        {(["general", "files", "peers", "trackers"] as const).map((t) => (
          <button key={t} type="button" role="tab" aria-selected={tab === t} className={`tab ${tab === t ? "active" : ""}`} onClick={() => setTab(t)}>
            {TAB_NAMES[t]}
          </button>
        ))}
        {!sheet && (onPopOut || onClose) && (
          <span className="tabs-actions">
            {onPopOut && (
              <button type="button" className="tabs-action" onClick={onPopOut}>
                ⧉ Window
              </button>
            )}
            {onClose && (
              <button type="button" className="tabs-action" aria-label="Close details" onClick={onClose}>
                ✕
              </button>
            )}
          </span>
        )}
      </div>
      <div className="tab-body" role="tabpanel">
        {error && <p className="error-text">{error}</p>}
        {summary.inDeluge && <InDelugeBanner key={summary.id} summary={summary} readOnly={readOnly} />}
        {tab === "general" && (
          <dl className="facts">
            <Row label="Name" value={summary.name} />
            <Row
              label="Status"
              value={
                summary.error ? <span className="error-text">Error: {summary.error}</span>
                : summary.inDeluge ? `${STATE_LABEL[summary.state]} here, in Deluge`
                : STATE_LABEL[summary.state]
              }
            />
            <Row label="Progress" value={`${formatPercent(summary.progress)} (${summary.piecesHave} of ${summary.piecesTotal} pieces)`} />
            <Row label="Size" value={formatBytes(summary.size)} />
            <Row label="Peers" value={`${summary.peers} connected, ${summary.seeds} of them seeds`} />
            <Row label="Downloaded" value={formatBytes(summary.downloaded)} />
            <Row label="Uploaded" value={`${formatBytes(summary.uploaded)} (ratio ${formatRatio(summary.uploaded, summary.downloaded)})`} />
            <Row
              label="Save path"
              value={
                <>
                  <code>{summary.savePath}</code>
                  <WebLink savePath={summary.savePath} dir>
                    Browse
                  </WebLink>
                  <PathActions savePath={summary.savePath} file={summary.name} explain />
                </>
              }
            />
            <Row label="Label" value={<LabelEditor id={summary.id} label={summary.label} readOnly={readOnly} />} />
            <Row label="Group" value={summary.group ?? <span className="muted">None (set up groups in Settings → Groups)</span>} />
            <Row label="Queue" value={`Position ${summary.queuePosition + 1}`} />
            {summary.supersededBy && <Row label="Superseded" value={<SupersededBy id={summary.supersededBy} />} />}
            {detail && detail.libraryBytes > 0 && (
              <Row label="From the library" value={`${formatBytes(detail.libraryBytes)} reused from other torrents, not downloaded (see Files)`} />
            )}
            {detail && <Row label="File names" value={detail.naming === "native" ? "Native" : "Portable"} />}
            {detail && <Row label="Limits" value={<LimitsEditor id={summary.id} limits={detail.limits} refetch={refetch} readOnly={readOnly} />} />}
            {detail && <Row label="Seeding limits" value={<SeedLimitsEditor key={summary.id} id={summary.id} detail={detail} refetch={refetch} readOnly={readOnly} />} />}
            {detail && detail.seedingSecs > 0 && <Row label="Seeding time" value={formatSeedingTime(detail.seedingSecs)} />}
            {detail && <Row label="Download order" value={<FileOrderEditor id={summary.id} detail={detail} refetch={refetch} readOnly={readOnly} />} />}
            {detail && (
              <Row
                label="Completed files"
                value={<MoveCompletedEditor key={summary.id} id={summary.id} detail={detail} refetch={refetch} readOnly={readOnly} />}
              />
            )}
            <Row
              label="Added"
              value={
                <>
                  <When ms={summary.addedAt} />
                  {summary.leftoverFrom && <span className="muted"> (when Deluge saved its .torrent file)</span>}
                </>
              }
            />
            {summary.leftoverFrom && (
              <Row
                label="Imported from"
                value={
                  <>
                    <span className="chip chip-leftover">Deluge leftover</span> A .torrent file left in <code>{summary.leftoverFrom}</code>. Deluge didn&apos;t
                    list it and never ran it.
                  </>
                }
              />
            )}
            {summary.infoHashV1 && <Row label="Info-hash v1" value={<code>{summary.infoHashV1}</code>} />}
            {summary.infoHashV2 && <Row label="Info-hash v2" value={<code>{summary.infoHashV2}</code>} />}
            {summary.private && <Row label="Private" value="Yes: trackers only" />}
            {detail?.comment && <Row label="Comment" value={detail.comment} />}
            {detail?.createdBy && <Row label="Created by" value={detail.createdBy} />}
            {detail?.creationDate != null && <Row label="Created" value={<When ms={detail.creationDate * 1000} />} />}
            {detail && (
              <Row
                label="Magnet"
                value={
                  <button type="button" className="link" onClick={() => copy(detail.magnet)}>
                    Copy magnet link
                  </button>
                }
              />
            )}
          </dl>
        )}
        {tab === "files" && detail && summary.state === "metadata" && (
          <p className="muted">The file list arrives with the metadata, which is being fetched from peers{summary.peers > 0 ? ` (${summary.peers} connected)` : ""}.</p>
        )}
        {tab === "files" && detail && summary.state !== "metadata" && <FilesTab summary={summary} detail={detail} refetch={refetch} readOnly={readOnly} />}
        {tab === "peers" && detail && <PeersTab detail={detail} />}
        {tab === "trackers" && detail && (
          <TrackersTab key={summary.id} detail={detail} id={summary.id} mayEdit={!readOnly && summary.state !== "metadata"} refetch={refetch} />
        )}
      </div>
    </section>
  );
}

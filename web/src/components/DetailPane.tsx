import { useEffect, useState } from "react";

import { api } from "../api/client";
import type { TorrentDetail } from "../api/generated/TorrentDetail";
import type { TorrentLimits } from "../api/generated/TorrentLimits";
import type { TorrentFile } from "../api/generated/TorrentFile";
import type { TorrentSummary } from "../api/generated/TorrentSummary";
import type { UnwantedPolicy } from "../api/generated/UnwantedPolicy";
import { formatBytes, formatDateTime, formatPercent, formatRate, formatRatio, formatUtc, fromKiB, toKiB } from "../format";

type Tab = "general" | "files" | "trackers";

const PRIORITIES: Array<[number, string]> = [
  [0, "Skip"],
  [1, "Low"],
  [4, "Normal"],
  [7, "High"],
];

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
  if (f.placement === "held") {
    return <span className="chip chip-held">Held</span>;
  }
  if (f.placement === "parts") {
    return <span className="chip">Not in folder</span>;
  }
  return null;
}

function LabelEditor({ id, label }: { id: string; label: string | null }) {
  const [value, setValue] = useState(label ?? "");
  useEffect(() => setValue(label ?? ""), [label]);
  const dirty = value.trim() !== (label ?? "");
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

function LimitsEditor({ id, limits, refetch }: { id: string; limits: TorrentLimits; refetch: () => void }) {
  const [editing, setEditing] = useState(false);
  const [down, setDown] = useState(toKiB(limits.downloadRate));
  const [up, setUp] = useState(toKiB(limits.uploadRate));
  const [peers, setPeers] = useState(limits.maxConnections == null ? "" : String(limits.maxConnections));
  if (!editing) {
    return (
      <span className="inline-edit">
        {describeLimits(limits)}
        <button type="button" className="link" onClick={() => setEditing(true)}>
          Change
        </button>
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

function FilesTab({ summary, detail, refetch }: { summary: TorrentSummary; detail: TorrentDetail; refetch: () => void }) {
  const held = detail.files.filter((f) => f.placement === "held");
  const heldBytes = held.reduce((n, f) => n + f.length, 0);
  const anyNotInFolder = detail.files.some((f) => f.placement !== "save");
  const setPolicy = (policy: UnwantedPolicy) => {
    void api.setUnwantedPolicy(summary.id, policy).then(refetch);
  };
  return (
    <>
      <div className="files-bar">
        <label className="inline-field">
          <span>Deselected files with data:</span>
          <select aria-label="What happens to deselected files" value={detail.unwantedPolicy} onChange={(e) => setPolicy(e.target.value as UnwantedPolicy)}>
            <option value="hold">Move to the held area</option>
            <option value="delete">Delete</option>
          </select>
        </label>
        {held.length > 0 && (
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
      <table className="files">
        <thead>
          <tr>
            <th>File</th>
            <th className="end">Size</th>
            <th>Progress</th>
            <th>Location</th>
            <th>Priority</th>
          </tr>
        </thead>
        <tbody>
          {detail.files.map((f) => (
            <tr key={f.index}>
              <td className="cell-path">{f.path}</td>
              <td className="end">{formatBytes(f.length)}</td>
              <td>{formatPercent(f.progress)}</td>
              <td>{location(f)}</td>
              <td>
                <select
                  aria-label={`Priority of ${f.path}`}
                  value={PRIORITIES.reduce((best, [p]) => (Math.abs(p - f.priority) < Math.abs(best - f.priority) ? p : best), 4)}
                  onChange={(e) => {
                    void api.setFilePriorities(summary.id, [{ index: f.index, priority: Number(e.target.value) }]).then(refetch);
                  }}
                >
                  {PRIORITIES.map(([p, name]) => (
                    <option key={p} value={p}>{name}</option>
                  ))}
                </select>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </>
  );
}

export function DetailPane({ summary }: { summary: TorrentSummary }) {
  const [tab, setTab] = useState<Tab>("general");
  const { detail, error, refetch } = useDetail(summary.id, summary);

  return (
    <section className="detail" aria-label={`Details of ${summary.name}`}>
      <div className="tabs" role="tablist">
        {(["general", "files", "trackers"] as const).map((t) => (
          <button key={t} type="button" role="tab" aria-selected={tab === t} className={`tab ${tab === t ? "active" : ""}`} onClick={() => setTab(t)}>
            {t === "general" ? "General" : t === "files" ? "Files" : "Trackers"}
          </button>
        ))}
      </div>
      <div className="tab-body" role="tabpanel">
        {error && <p className="error-text">{error}</p>}
        {tab === "general" && (
          <dl className="facts">
            <Row label="Name" value={summary.name} />
            <Row label="Status" value={summary.error ? <span className="error-text">Error: {summary.error}</span> : summary.state} />
            <Row label="Progress" value={`${formatPercent(summary.progress)} (${summary.piecesHave} of ${summary.piecesTotal} pieces)`} />
            <Row label="Size" value={formatBytes(summary.size)} />
            <Row label="Peers" value={`${summary.peers} connected, ${summary.seeds} of them seeds`} />
            <Row label="Downloaded" value={formatBytes(summary.downloaded)} />
            <Row label="Uploaded" value={`${formatBytes(summary.uploaded)} (ratio ${formatRatio(summary.uploaded, summary.downloaded)})`} />
            <Row label="Save path" value={<code>{summary.savePath}</code>} />
            <Row label="Label" value={<LabelEditor id={summary.id} label={summary.label} />} />
            <Row label="Group" value={summary.group ?? <span className="muted">None (set up groups in Settings)</span>} />
            <Row label="Queue" value={`Position ${summary.queuePosition + 1}`} />
            {detail && <Row label="Limits" value={<LimitsEditor id={summary.id} limits={detail.limits} refetch={refetch} />} />}
            <Row label="Added" value={<When ms={summary.addedAt} />} />
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
        {tab === "files" && detail && <FilesTab summary={summary} detail={detail} refetch={refetch} />}
        {tab === "trackers" && detail && (
          detail.trackers.length === 0 ? (
            <p className="muted">No trackers. Peers come from other sources.</p>
          ) : (
            <ol className="tiers">
              {detail.trackers.map((tier, i) => (
                <li key={i}>
                  <span className="muted">Tier {i + 1}</span>
                  <ul>
                    {tier.map((url) => (
                      <li key={url}><code>{url}</code></li>
                    ))}
                  </ul>
                </li>
              ))}
            </ol>
          )
        )}
      </div>
    </section>
  );
}

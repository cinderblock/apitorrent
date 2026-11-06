import { useEffect, useState } from "react";

import { api } from "../api/client";
import type { TorrentDetail } from "../api/generated/TorrentDetail";
import type { TorrentSummary } from "../api/generated/TorrentSummary";
import { formatBytes, formatDateTime, formatPercent, formatRatio, formatUtc } from "../format";

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
  }, [id, revision]);
  return { detail, error };
}

function Row({ label, value, title }: { label: string; value: React.ReactNode; title?: string }) {
  return (
    <>
      <dt>{label}</dt>
      <dd title={title}>{value}</dd>
    </>
  );
}

function copy(text: string) {
  void navigator.clipboard?.writeText(text);
}

export function DetailPane({ summary }: { summary: TorrentSummary }) {
  const [tab, setTab] = useState<Tab>("general");
  const { detail, error } = useDetail(summary.id, summary);

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
            <Row label="Status" value={summary.error ? `Error: ${summary.error}` : summary.state} />
            <Row label="Progress" value={`${formatPercent(summary.progress)} (${summary.piecesHave} of ${summary.piecesTotal} pieces)`} />
            <Row label="Size" value={formatBytes(summary.size)} />
            <Row label="Downloaded" value={formatBytes(summary.downloaded)} />
            <Row label="Uploaded" value={`${formatBytes(summary.uploaded)} (ratio ${formatRatio(summary.uploaded, summary.downloaded)})`} />
            <Row label="Save path" value={<code>{summary.savePath}</code>} />
            <Row label="Added" value={formatDateTime(summary.addedAt)} title={formatUtc(summary.addedAt)} />
            {summary.infoHashV1 && <Row label="Info-hash v1" value={<code>{summary.infoHashV1}</code>} />}
            {summary.infoHashV2 && <Row label="Info-hash v2" value={<code>{summary.infoHashV2}</code>} />}
            {summary.private && <Row label="Private" value="Yes: trackers only" />}
            {detail?.comment && <Row label="Comment" value={detail.comment} />}
            {detail?.createdBy && <Row label="Created by" value={detail.createdBy} />}
            {detail?.creationDate != null && (
              <Row label="Created" value={formatDateTime(detail.creationDate * 1000)} title={formatUtc(detail.creationDate * 1000)} />
            )}
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
        {tab === "files" && detail && (
          <table className="files">
            <thead>
              <tr>
                <th>File</th>
                <th className="end">Size</th>
                <th>Progress</th>
                <th>Priority</th>
              </tr>
            </thead>
            <tbody>
              {detail.files.map((f) => (
                <tr key={f.index}>
                  <td className="cell-name" title={f.path}>{f.path}</td>
                  <td className="end">{formatBytes(f.length)}</td>
                  <td>{formatPercent(f.progress)}</td>
                  <td>
                    <select
                      aria-label={`Priority of ${f.path}`}
                      value={PRIORITIES.reduce((best, [p]) => (Math.abs(p - f.priority) < Math.abs(best - f.priority) ? p : best), 4)}
                      onChange={(e) => {
                        void api.setFilePriorities(summary.id, [{ index: f.index, priority: Number(e.target.value) }]);
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
        )}
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

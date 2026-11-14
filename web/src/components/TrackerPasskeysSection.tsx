// Settings → Tracker passkeys: the per-user tokens private trackers put in announce URLs,
// found in this space's torrents and changed on all of them at once (after rolling one at
// the tracker). The server never sends a passkey back; rows show its ends only.

import { useCallback, useEffect, useState } from "react";

import { ApiRequestError, api } from "../api/client";
import type { ReplacePasskeyReport } from "../api/generated/ReplacePasskeyReport";
import type { TrackerPasskey } from "../api/generated/TrackerPasskey";
import { Empty, SettingCard } from "../settings/controls";

function message(e: unknown): string {
  return e instanceof ApiRequestError ? e.message : String(e);
}

function PasskeyRow({ passkey, onReplaced }: { passkey: TrackerPasskey; onReplaced: (r: ReplacePasskeyReport) => void }) {
  const [editing, setEditing] = useState(false);
  const [value, setValue] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const fresh = value.trim();

  async function replace() {
    setBusy(true);
    setError(null);
    try {
      const r = await api.replaceTrackerPasskey({ id: passkey.id, newPasskey: fresh });
      setValue("");
      setEditing(false);
      onReplaced(r);
    } catch (e) {
      setError(message(e));
    } finally {
      setBusy(false);
    }
  }

  const id = `passkey-${passkey.id}`;
  const n = `${passkey.torrents} torrent${passkey.torrents === 1 ? "" : "s"}`;
  return (
    <li>
      <div className="s-list-main">
        <code>{passkey.masked}</code>
        <span className="muted">
          {passkey.hosts.join(", ")} · {n}
        </span>
      </div>
      {!editing && (
        <div className="s-list-actions">
          <button type="button" onClick={() => setEditing(true)}>
            Change…
          </button>
        </div>
      )}
      {editing && (
        <form
          className="s-list-edit"
          onSubmit={(e) => {
            e.preventDefault();
            void replace();
          }}
        >
          <label htmlFor={id}>The new passkey, as the tracker shows it</label>
          <div className="s-add">
            <input
              id={id}
              type="password"
              autoComplete="off"
              spellCheck={false}
              autoFocus
              value={value}
              onChange={(e) => {
                setValue(e.target.value);
                setError(null);
              }}
            />
            <button type="submit" className="primary" disabled={busy || fresh === ""}>
              {busy ? "Changing…" : `Change on ${n}`}
            </button>
            <button
              type="button"
              onClick={() => {
                setEditing(false);
                setValue("");
                setError(null);
              }}
            >
              Cancel
            </button>
          </div>
          {error && <p className="error-text">{error}</p>}
        </form>
      )}
    </li>
  );
}

/** This space's admins: the passkeys are the space's credentials. Acts at once. */
export function TrackerPasskeysSection() {
  const [passkeys, setPasskeys] = useState<TrackerPasskey[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [report, setReport] = useState<ReplacePasskeyReport | null>(null);
  const load = useCallback(() => {
    api.trackerPasskeys().then(setPasskeys, (e: unknown) => setError(message(e)));
  }, []);
  useEffect(load, [load]);

  return (
    <SettingCard
      description="After you change your passkey on a tracker's site, put the new one here: every torrent in this space that uses the old one gets the new one. Only the first and last characters are shown."
    >
      {passkeys && passkeys.length > 0 && (
        <ul className="s-list">
          {passkeys.map((p) => (
            <PasskeyRow
              key={p.id}
              passkey={p}
              onReplaced={(r) => {
                setReport(r);
                load();
              }}
            />
          ))}
        </ul>
      )}
      {passkeys?.length === 0 && <Empty>No torrent in this space has a passkey in its trackers.</Empty>}
      {report && (
        <p className="ok-text">
          Changed on {report.changed} torrent{report.changed === 1 ? "" : "s"}.
          {report.skipped.length > 0 && ` Left ${report.skipped.length} as they were:`}
        </p>
      )}
      {report && report.skipped.length > 0 && (
        <ul className="outcomes">
          {report.skipped.map((s) => (
            <li key={s.name}>
              {s.name}: <span className="muted">{s.reason}</span>
            </li>
          ))}
        </ul>
      )}
      {error && <p className="error-text">{error}</p>}
    </SettingCard>
  );
}

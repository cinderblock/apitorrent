// Settings → Tracker passkeys: the per-user tokens private trackers put in announce URLs,
// found in this space's torrents and changed on all of them at once (after rolling one at
// the tracker). The server never sends a passkey back; rows show its ends only.

import { useCallback, useEffect, useState } from "react";

import { ApiRequestError, api } from "../api/client";
import type { ReplacePasskeyReport } from "../api/generated/ReplacePasskeyReport";
import type { TrackerPasskey } from "../api/generated/TrackerPasskey";

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
  return (
    <div className="root-row">
      <div>
        <code>{passkey.masked}</code> on {passkey.hosts.join(", ")}{" "}
        <span className="muted">
          ({passkey.torrents} torrent{passkey.torrents === 1 ? "" : "s"})
        </span>{" "}
        {!editing && (
          <button type="button" className="link" onClick={() => setEditing(true)}>
            Change…
          </button>
        )}
      </div>
      {editing && (
        <>
          <label className="field" htmlFor={id}>
            <span>The new passkey, as the tracker shows it</span>
          </label>
          <input
            id={id}
            type="password"
            autoComplete="off"
            spellCheck={false}
            value={value}
            onChange={(e) => {
              setValue(e.target.value);
              setError(null);
            }}
          />
          <div className="inline-edit">
            <button type="button" disabled={busy || fresh === ""} onClick={() => void replace()}>
              {busy ? "Changing…" : `Change on ${passkey.torrents} torrent${passkey.torrents === 1 ? "" : "s"}`}
            </button>
            <button
              type="button"
              className="link"
              onClick={() => {
                setEditing(false);
                setValue("");
                setError(null);
              }}
            >
              Cancel
            </button>
            {error && <span className="error-text">{error}</span>}
          </div>
        </>
      )}
    </div>
  );
}

/** This space's admins: the passkeys are the space's credentials. */
export function TrackerPasskeysSection() {
  const [passkeys, setPasskeys] = useState<TrackerPasskey[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [report, setReport] = useState<ReplacePasskeyReport | null>(null);
  const load = useCallback(() => {
    api.trackerPasskeys().then(setPasskeys, (e: unknown) => setError(message(e)));
  }, []);
  useEffect(load, [load]);

  return (
    <fieldset className="settings-section">
      <legend>Tracker passkeys</legend>
      <p className="hint muted">
        Private trackers know you by a passkey in their announce addresses. After you change yours on the tracker&apos;s site, put the new one here: every
        torrent in this space that uses the old one gets the new one. Only its first and last characters are shown.
      </p>
      {passkeys?.map((p) => (
        <PasskeyRow
          key={p.id}
          passkey={p}
          onReplaced={(r) => {
            setReport(r);
            load();
          }}
        />
      ))}
      {passkeys?.length === 0 && <p className="hint muted">No torrent in this space has a passkey in its trackers.</p>}
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
    </fieldset>
  );
}

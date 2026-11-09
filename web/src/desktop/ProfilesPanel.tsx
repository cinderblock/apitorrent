// The list of saved profiles: connect, edit, remove, add. Switching profiles reloads the
// window, so the whole UI starts fresh against the new connection.

import { useState } from "react";

import { desktop, type DesktopState, type ProfileView } from "./bridge";
import { newLocal, newRemote, ProfileEditor } from "./ProfileEditor";
import { describe } from "./util";

export function ProfilesPanel({ state, onChanged }: { state: DesktopState; onChanged: () => void }) {
  const [editing, setEditing] = useState<ProfileView | null>(null);
  const [confirmRemove, setConfirmRemove] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function act(id: string, action: () => Promise<unknown>, reload: boolean) {
    setBusy(id);
    setError(null);
    try {
      await action();
      if (reload) location.reload();
      else onChanged();
    } catch (err) {
      setError(describe(err));
    } finally {
      setBusy(null);
    }
  }

  if (editing) {
    return (
      <ProfileEditor
        initial={editing}
        connected={state.connection?.profileId === editing.id}
        onCancel={() => setEditing(null)}
        onSaved={(saved) => {
          setEditing(null);
          // A saved active profile reconnects; reload so the UI uses the new connection.
          if (saved.id === state.activeId) location.reload();
          else onChanged();
        }}
      />
    );
  }

  const conn = state.connection;
  return (
    <div className="profiles">
      {state.connectError && <p className="error-text">Couldn't connect: {state.connectError}</p>}
      {state.profiles.length === 0 ? (
        <p className="muted">
          Connect to an apitorrent server (a seedbox or NAS running the daemon), or run apitorrent right here.
        </p>
      ) : (
        <ul className="profile-list">
          {state.profiles.map((p) => {
            const active = conn?.profileId === p.id;
            return (
              <li key={p.id} className={active ? "active" : ""}>
                <div className="profile-main">
                  <strong>{p.name}</strong> {active && <span className="chip chip-held">Connected</span>}
                  <div className="muted">
                    {p.mode === "remote" ? (
                      <>
                        <code>{p.baseUrl}</code>
                        {!p.hasToken && <span className="error-text"> · no API token saved</span>}
                        {p.trust.kind === "pinned" && " · paired key"}
                        {p.trust.kind === "privateCa" && " · private CA"}
                      </>
                    ) : (
                      <>Runs on this computer{p.lan && <> · also serves {p.lan.listen}</>}</>
                    )}
                  </div>
                  {active && conn?.lan && <LanInfo lan={conn.lan} />}
                </div>
                <div className="profile-actions">
                  {!active && (
                    <button type="button" className="primary" disabled={busy !== null} onClick={() => void act(p.id, () => desktop.activate(p.id), true)}>
                      {busy === p.id ? "Connecting…" : "Connect"}
                    </button>
                  )}
                  <button type="button" disabled={busy !== null} onClick={() => setEditing(p)}>
                    Edit
                  </button>
                  {confirmRemove === p.id ? (
                    <button type="button" className="danger" onClick={() => void act(p.id, () => desktop.deleteProfile(p.id), active)}>
                      Remove {p.name}?
                    </button>
                  ) : (
                    <button type="button" disabled={busy !== null} onClick={() => setConfirmRemove(p.id)}>
                      Remove
                    </button>
                  )}
                </div>
              </li>
            );
          })}
        </ul>
      )}
      {error && <p className="error-text">{error}</p>}
      <div className="actions actions-start">
        <button type="button" onClick={() => setEditing(newRemote())}>
          Add a server
        </button>
        <button type="button" onClick={() => setEditing(newLocal())}>
          Run on this computer
        </button>
        {conn && (
          <button type="button" onClick={() => void act("", () => desktop.disconnect(), true)}>
            Disconnect
          </button>
        )}
      </div>
    </div>
  );
}

function LanInfo({ lan }: { lan: NonNullable<DesktopState["connection"]>["lan"] & object }) {
  return (
    <div className="lan-info">
      {lan.error ? (
        <p className="error-text">Network access failed: {lan.error}</p>
      ) : (
        <p className="muted">
          Serving the web UI on {lan.pairingCode ? "https" : "http"}://{lan.listen}.
        </p>
      )}
      {lan.pairingCode && (
        <p>
          Pairing code for other apps: <code className="key">{lan.pairingCode}</code>
        </p>
      )}
      {lan.setupLink && (
        <p>
          No one can sign in from the network yet. Open this one-time link in a browser to create the first account:{" "}
          <code className="token-box">{lan.setupLink}</code>{" "}
          <button type="button" className="link" onClick={() => void navigator.clipboard.writeText(lan.setupLink!)}>
            Copy
          </button>
        </p>
      )}
    </div>
  );
}

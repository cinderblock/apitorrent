// The desktop window's outer layer: choose a profile before anything else, wait while the
// app connects, then show the normal UI (with desktop notices on top).

import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";

import { desktop, type DesktopState } from "./bridge";
import { DesktopNotices } from "./DesktopNotices";
import { addNotice } from "./notices";
import { ProfilesPanel } from "./ProfilesPanel";
import { describe } from "./util";
import "./desktop.css";

export function DesktopGate({ children }: { children: ReactNode }) {
  const [state, setState] = useState<DesktopState | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(() => {
    desktop.state().then((s) => {
      // The app reports a damaged profiles file once; keep it visible past the gate.
      if (s.notice) addNotice(s.notice);
      setState(s);
    }, (e: unknown) => setError(describe(e)));
  }, []);
  useEffect(load, [load]);

  // At launch the app reconnects with the last profile in the background; wait for it.
  const connecting = state !== null && state.activeId !== null && state.connection === null && state.connectError === null;
  useEffect(() => {
    if (!connecting) return;
    const t = setTimeout(load, 250);
    return () => clearTimeout(t);
  }, [connecting, state, load]);

  if (error) {
    return (
      <div className="auth-page">
        <section className="auth-card">
          <h1 className="brand">apitorrent</h1>
          <p className="error-text">{error}</p>
        </section>
      </div>
    );
  }
  if (state === null || connecting) {
    const name = state?.profiles.find((p) => p.id === state.activeId)?.name;
    return (
      <div className="auth-page">
        <p className="muted">{name ? `Connecting to ${name}…` : "Loading…"}</p>
      </div>
    );
  }
  if (state.connection === null) {
    return (
      <div className="auth-page">
        <section className="auth-card profiles-card">
          <h1 className="brand">apitorrent</h1>
          <ProfilesPanel state={state} onChanged={load} />
        </section>
        <DesktopNotices />
      </div>
    );
  }
  return (
    <>
      {children}
      <DesktopNotices />
    </>
  );
}

/** The profile list in a dialog, for the toolbar and for connection problems. */
export function ProfilesDialog({ onClose }: { onClose: () => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [state, setState] = useState<DesktopState | null>(null);
  const [error, setError] = useState<string | null>(null);
  const load = useCallback(() => {
    desktop.state().then(setState, (e: unknown) => setError(describe(e)));
  }, []);
  useEffect(() => {
    dialog.current?.showModal();
    load();
  }, [load]);
  return (
    <dialog ref={dialog} className="dialog wide" onClose={onClose} aria-labelledby="profiles-title">
      <h2 id="profiles-title">Connections</h2>
      {error && <p className="error-text">{error}</p>}
      {state ? <ProfilesPanel state={state} onChanged={load} /> : <p className="muted">Loading…</p>}
      <div className="actions">
        <button type="button" onClick={() => dialog.current?.close()}>
          Close
        </button>
      </div>
    </dialog>
  );
}

/** The toolbar's connection button: the profile's name; opens the profile list. */
export function DesktopMenu() {
  const [open, setOpen] = useState(false);
  const [name, setName] = useState<string | null>(null);
  useEffect(() => {
    void desktop.state().then((s) => setName(s.profiles.find((p) => p.id === s.connection?.profileId)?.name ?? null));
  }, []);
  return (
    <>
      <button type="button" className="account-button" onClick={() => setOpen(true)}>
        {name ?? "Connections"}
      </button>
      {open && <ProfilesDialog onClose={() => setOpen(false)} />}
    </>
  );
}

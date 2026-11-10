// The daemon's own updates, for admins: a toolbar button when a new release is out, and
// an "Updates" section in Settings. The daemon checks GitHub's latest published release
// once a day; it installs only a build signed with apitorrent's key, never an older one.

import { useEffect, useRef, useState, useSyncExternalStore } from "react";

import { ApiRequestError, api, updateApi } from "../api/client";
import type { UpdateSettings } from "../api/generated/UpdateSettings";
import type { UpdateStatus } from "../api/generated/UpdateStatus";
import { formatDateTime, formatUtc } from "../format";

// One shared copy of the status, so the toolbar and Settings agree.
let current: UpdateStatus | null = null;
const listeners = new Set<() => void>();
function publish(s: UpdateStatus | null): void {
  current = s;
  for (const l of listeners) l();
}
function subscribe(l: () => void): () => void {
  listeners.add(l);
  return () => listeners.delete(l);
}

/** Load the status now and every hour. `enabled`: only admins may ask. */
export function useUpdateStatus(enabled: boolean): UpdateStatus | null {
  useEffect(() => {
    if (!enabled) return;
    const load = () =>
      updateApi.status().then(publish, () => {
        // No updater (the desktop app's own core), or not allowed: show nothing.
        publish(null);
      });
    void load();
    const timer = window.setInterval(() => void load(), 60 * 60 * 1000);
    return () => window.clearInterval(timer);
  }, [enabled]);
  return useSyncExternalStore(subscribe, () => current);
}

function message(e: unknown): string {
  return e instanceof ApiRequestError || e instanceof Error ? e.message : String(e);
}

/** After an install: wait for the daemon to come back as the new version, then reload. */
async function waitForRestart(version: string): Promise<void> {
  const deadline = Date.now() + 5 * 60 * 1000;
  while (Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 2000));
    try {
      const info = await api.info();
      if (info.version === version) {
        window.location.reload();
        return;
      }
    } catch {
      // Still restarting.
    }
  }
  throw new Error("The daemon hasn't come back as the new version after 5 minutes. Check its log.");
}

function When({ ms }: { ms: number }) {
  return <time title={formatUtc(ms)}>{formatDateTime(ms)}</time>;
}

/** What's available, and the install button. */
function UpdateDetails({ status }: { status: UpdateStatus }) {
  const [busy, setBusy] = useState<"install" | "restart" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const latest = status.latest;

  async function install() {
    if (!latest) return;
    setBusy("install");
    setError(null);
    try {
      publish(await updateApi.install());
      setBusy("restart");
      await waitForRestart(latest.version);
    } catch (e) {
      setError(message(e));
      setBusy(null);
      updateApi.status().then(publish, () => {});
    }
  }

  if (!latest) return null;
  return (
    <div className="update-details">
      <p>
        <strong>apitorrent {latest.version}</strong> is available. This server runs {status.currentVersion}.
        {latest.publishedMs != null && (
          <>
            {" "}
            Released <When ms={latest.publishedMs} />.
          </>
        )}{" "}
        <a href={latest.url} target="_blank" rel="noreferrer noopener">
          Release notes
        </a>
      </p>
      {latest.notes && <pre className="update-notes">{latest.notes}</pre>}
      {status.installsEnabled ? (
        <button type="button" className="primary" disabled={busy !== null} onClick={() => void install()}>
          {busy === "install" ? "Downloading and verifying…" : busy === "restart" ? "Restarting…" : "Install and restart"}
        </button>
      ) : (
        <p className="hint muted">
          This build can&apos;t install updates: {status.installsDisabledReason}. Update it the way it was installed.
        </p>
      )}
      {busy === "restart" && <p className="hint muted">The page reloads when the new version answers.</p>}
      {error && <p className="error-text">{error}</p>}
    </div>
  );
}

/** A toolbar button for admins while a newer release exists. */
export function UpdateButton({ isAdmin }: { isAdmin: boolean }) {
  const status = useUpdateStatus(isAdmin);
  const [open, setOpen] = useState(false);
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    if (open) dialog.current?.showModal();
  }, [open]);
  if (!status?.available || !status.latest) return null;
  return (
    <>
      <button type="button" className="update-button" onClick={() => setOpen(true)}>
        Update to {status.latest.version}
      </button>
      {open && (
        <dialog ref={dialog} className="dialog" onClose={() => setOpen(false)} aria-labelledby="update-title">
          <h2 id="update-title">Update available</h2>
          <UpdateDetails status={status} />
          <div className="actions">
            <button type="button" onClick={() => dialog.current?.close()}>
              Close
            </button>
          </div>
        </dialog>
      )}
    </>
  );
}

/** Settings → Updates (admins). */
export function UpdatesSection() {
  const status = useUpdateStatus(true);
  const [checking, setChecking] = useState(false);
  const [error, setError] = useState<string | null>(null);

  if (!status) return null;

  async function check() {
    setChecking(true);
    setError(null);
    try {
      publish(await updateApi.check());
    } catch (e) {
      setError(message(e));
    } finally {
      setChecking(false);
    }
  }

  async function set(patch: Partial<UpdateSettings>) {
    if (!status) return;
    setError(null);
    try {
      publish(await updateApi.setSettings({ ...status.settings, ...patch }));
    } catch (e) {
      setError(message(e));
    }
  }

  return (
    <fieldset className="settings-section">
      <legend>Updates</legend>
      <p className="muted">
        This server runs apitorrent {status.currentVersion}.{" "}
        {status.checkedMs != null && (
          <>
            Last checked <When ms={status.checkedMs} />.
          </>
        )}
      </p>
      {status.available ? (
        <UpdateDetails status={status} />
      ) : (
        status.latest && <p className="muted">It is up to date.</p>
      )}
      {status.lastError && <p className="error-text">{status.lastError}</p>}
      {error && <p className="error-text">{error}</p>}
      <button type="button" disabled={checking} onClick={() => void check()}>
        {checking ? "Checking…" : "Check now"}
      </button>
      <label className="check">
        <input type="checkbox" checked={status.settings.checkDaily} onChange={(e) => void set({ checkDaily: e.target.checked })} />
        Check for a new release once a day
      </label>
      <label className="check">
        <input
          type="checkbox"
          checked={status.settings.autoInstall}
          disabled={!status.installsEnabled}
          onChange={(e) => void set({ autoInstall: e.target.checked })}
        />
        Install new releases automatically, and restart
      </label>
      <p className="hint muted">
        Only releases signed with apitorrent&apos;s update key install, and never an older version. Otherwise an admin installs from here, or with{" "}
        <code>apitorrentd update</code>.
      </p>
    </fieldset>
  );
}

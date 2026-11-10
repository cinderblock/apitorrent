// The Plugins page (/plugins): installed plugins, approving what they ask for, enabling
// and disabling them, their logs, and a settings form drawn from each plugin's schema.
//
// Server admins manage everything. A space's admins see the same list, and manage the
// settings and logs of the space-aware plugins' instances in their space; shared plugins
// (one instance for the whole server) are read-only to them.

import { useCallback, useEffect, useRef, useState, type ChangeEvent, type FormEvent } from "react";

import { ApiRequestError } from "../api/client";
import type { AuthStatus } from "../api/generated/AuthStatus";
import type { PluginInfo } from "../api/generated/PluginInfo";
import type { PluginLogEntry } from "../api/generated/PluginLogEntry";
import type { PluginManifest } from "../api/generated/PluginManifest";
import type { PluginSettings } from "../api/generated/PluginSettings";
import { Cancelled, describeError, withReauth } from "../auth/flow";
import { ReauthDialog } from "../auth/ReauthDialog";
import { formatDateTime, formatUtc } from "../format";
import { navigate } from "../router";
import { useSpaceContext } from "../spaces/context";
import { pluginsApi, readBundle } from "./api";
import { SchemaForm } from "./SchemaForm";
import {
  describeCapabilities,
  secretNames,
  toRequest,
  type JsonSchema,
  type SecretEdit,
  type Values,
} from "./settingsSchema";
import "./plugins.css";

type Change = (action: () => Promise<unknown>) => Promise<boolean>;

const STATUS_TEXT: Record<PluginInfo["status"], string> = {
  awaitingApproval: "Waiting for approval",
  disabled: "Disabled",
  starting: "Starting",
  running: "Running",
  failed: "Stopped on an error",
};

export function PluginsPage({ status, onStatus }: { status: AuthStatus; onStatus: (s: AuthStatus) => void }) {
  const ctx = useSpaceContext();
  const serverAdmin = ctx ? ctx.serverAdmin : status.authDisabled || status.user?.role === "admin";
  // With several spaces, a space-aware plugin's settings are this space's.
  const spaceName = ctx && ctx.spaces.length > 1 ? ctx.current.name : null;
  const [plugins, setPlugins] = useState<PluginInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [unavailable, setUnavailable] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [reauth, setReauth] = useState<{ resolve: () => void; reject: (e: unknown) => void } | null>(null);
  const [approving, setApproving] = useState<PluginInfo | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setPlugins(await pluginsApi.list());
      setUnavailable(null);
    } catch (e) {
      if (e instanceof ApiRequestError && (e.code === "plugins_disabled" || e.code === "admin_only")) {
        setUnavailable(e.message);
      } else {
        setError(describeError(e));
      }
    }
  }, []);

  // Statuses change on their own (starting, failing), so keep the list fresh.
  useEffect(() => {
    void load();
    const timer = setInterval(() => void load(), 3000);
    return () => clearInterval(timer);
  }, [load]);

  const confirmIdentity = useCallback(
    () => new Promise<void>((resolve, reject) => setReauth({ resolve, reject })),
    [],
  );

  const change: Change = async (action) => {
    setBusy(true);
    setError(null);
    try {
      await withReauth(action, confirmIdentity);
      await load();
      return true;
    } catch (e) {
      if (!(e instanceof Cancelled)) setError(describeError(e));
      return false;
    } finally {
      setBusy(false);
    }
  };

  async function install(e: ChangeEvent<HTMLInputElement>) {
    const file = e.target.files?.[0];
    e.target.value = "";
    if (!file) return;
    let installed: PluginInfo | null = null;
    let needsApproval = false;
    const ok = await change(async () => {
      const r = await pluginsApi.install(await readBundle(file));
      installed = r.plugin;
      needsApproval = r.needsApproval;
    });
    if (ok && installed) {
      const p: PluginInfo = installed;
      if (needsApproval) setApproving(p);
      else setNotice(`${p.active?.name ?? p.id} was updated; it asked for nothing new.`);
    }
  }

  return (
    <div className="account plugins-page">
      <header className="toolbar account-toolbar">
        <h1 className="brand">apitorrent</h1>
        <button type="button" className="link" onClick={() => navigate("/")}>
          ← Torrents
        </button>
        <span className="spacer" />
        {!unavailable && serverAdmin && (
          <label className="button-like">
            Install a plugin…
            <input type="file" accept=".json,application/json" hidden onChange={(e) => void install(e)} disabled={busy} />
          </label>
        )}
      </header>
      <section className="account-section">
        <h2>Plugins{spaceName && <span className="muted"> in {spaceName}</span>}</h2>
        {serverAdmin ? (
          <p className="muted">
            Plugins run inside apitorrent, each in its own sandbox. A plugin can do only what you approve: the hosts
            it may reach, what it may do with torrents, and the rest. An update that asks for more waits for your
            approval while the current version keeps running. Install files are <code>*.plugin.json</code>, made by{" "}
            <code>apitorrent-plugin-build</code>.
          </p>
        ) : (
          <p className="muted">
            A plugin marked <em>per space</em> runs separately in each space, with its own settings and data: you
            manage its settings{spaceName && <> for {spaceName}</>} here and can read its logs. A <em>shared</em>{" "}
            plugin runs once for the whole server; only a server admin installs, approves, enables or configures
            plugins.
          </p>
        )}
      </section>
      {unavailable && <p className="banner-warn">{unavailable}</p>}
      {error && (
        <p className="error-text" role="alert">
          {error}
        </p>
      )}
      {notice && (
        <p className="plugin-notice" role="status">
          {notice}{" "}
          <button type="button" className="link" onClick={() => setNotice(null)}>
            Dismiss
          </button>
        </p>
      )}
      {plugins?.length === 0 && <p className="muted">No plugins installed.</p>}
      {plugins?.map((p) => (
        <PluginCard
          key={p.id}
          plugin={p}
          busy={busy}
          change={change}
          onApprove={() => setApproving(p)}
          serverAdmin={serverAdmin}
          spaceName={spaceName}
        />
      ))}
      {approving && (
        <ApproveDialog
          plugin={approving}
          onCancel={() => setApproving(null)}
          onApprove={async (enable) => {
            const p = approving;
            const version = p.pending!.manifest.version;
            const ok = await change(async () => {
              await pluginsApi.approve(p.id, version);
              if (enable && !p.enabled) await pluginsApi.enable(p.id);
            });
            if (ok) setApproving(null);
          }}
        />
      )}
      {reauth && (
        <ReauthDialog
          status={status}
          reason="Installing, approving and removing plugins needs a fresh sign-in."
          onDone={(next) => {
            onStatus(next);
            reauth.resolve();
            setReauth(null);
          }}
          onCancel={() => {
            reauth.reject(new Cancelled());
            setReauth(null);
          }}
        />
      )}
    </div>
  );
}

function When({ ms }: { ms: number }) {
  return (
    <>
      <time>{formatDateTime(ms)}</time> <small className="muted">{formatUtc(ms)}</small>
    </>
  );
}

function ManifestSummary({ m }: { m: PluginManifest }) {
  return (
    <>
      <strong>{m.name}</strong> <span className="muted">{m.version}</span>
      {m.author && <span className="muted"> by {m.author}</span>}
      {m.description && <p className="plugin-description">{m.description}</p>}
    </>
  );
}

function PluginCard({
  plugin: p,
  busy,
  change,
  onApprove,
  serverAdmin,
  spaceName,
}: {
  plugin: PluginInfo;
  busy: boolean;
  change: Change;
  onApprove: () => void;
  serverAdmin: boolean;
  spaceName: string | null;
}) {
  const [open, setOpen] = useState<"settings" | "logs" | "grants" | null>(null);
  const [confirmRemove, setConfirmRemove] = useState(false);
  const manifest = p.active ?? p.pending?.manifest;
  const perSpace = manifest?.spaceAware === true;
  // A space's admin manages a per-space plugin's instance here; a shared one is read-only.
  const mine = serverAdmin || perSpace;
  const hasSettings = Boolean(manifest?.settings) && mine;
  const toggle = (section: typeof open) => setOpen(open === section ? null : section);

  return (
    <section className="account-section plugin-card">
      <div className="plugin-head">
        <div className="plugin-title">
          {manifest ? <ManifestSummary m={manifest} /> : <strong>{p.id}</strong>}
        </div>
        <span className="plugin-badges">
          <span className="badge" title={perSpace ? "Runs separately in each space" : "One instance for the whole server"}>
            {perSpace ? "per space" : "shared"}
          </span>
          <span className={`badge plugin-status plugin-status-${p.status}`}>{STATUS_TEXT[p.status]}</span>
        </span>
      </div>
      {!mine && <p className="muted">Shared by every space; a server admin manages it.</p>}
      {p.error && (
        <p className="error-text">
          {p.error}
          {p.enabled && " Enable it again to restart it."}
        </p>
      )}
      {p.pending && serverAdmin && (
        <p className="banner-warn">
          {p.active ? `Version ${p.pending.manifest.version} is waiting for approval.` : "Waiting for your approval."}{" "}
          It asks to: {p.pending.newCapabilities.join("; ") || "nothing beyond what it has"}.
        </p>
      )}
      <div className="actions start">
        {p.pending && serverAdmin && (
          <button type="button" className="primary" disabled={busy} onClick={onApprove}>
            Review and approve…
          </button>
        )}
        {p.active &&
          serverAdmin &&
          (p.enabled && p.status !== "failed" ? (
            <button type="button" disabled={busy} onClick={() => void change(() => pluginsApi.disable(p.id))}>
              Disable
            </button>
          ) : (
            <button type="button" disabled={busy} onClick={() => void change(() => pluginsApi.enable(p.id))}>
              {p.enabled ? "Restart" : "Enable"}
            </button>
          ))}
        {hasSettings && (
          <button type="button" aria-expanded={open === "settings"} onClick={() => toggle("settings")}>
            Settings
          </button>
        )}
        {mine && (
          <button type="button" aria-expanded={open === "logs"} onClick={() => toggle("logs")}>
            Logs
          </button>
        )}
        {p.active && (
          <button type="button" aria-expanded={open === "grants"} onClick={() => toggle("grants")}>
            Permissions
          </button>
        )}
        {!serverAdmin ? null : confirmRemove ? (
          <>
            <span className="muted">Remove it, with its settings and data?</span>
            <button
              type="button"
              className="danger"
              disabled={busy}
              onClick={() => void change(() => pluginsApi.uninstall(p.id))}
            >
              Remove
            </button>
            <button type="button" onClick={() => setConfirmRemove(false)}>
              Keep
            </button>
          </>
        ) : (
          <button type="button" className="link" onClick={() => setConfirmRemove(true)}>
            Remove…
          </button>
        )}
      </div>
      <p className="muted plugin-meta">
        Installed <When ms={p.installedAt} />; changed <When ms={p.updatedAt} />.
      </p>
      {open === "grants" && (
        <ul className="plugin-grants">
          {describeCapabilities(p.granted).map((g) => (
            <li key={g}>{g}</li>
          ))}
          {describeCapabilities(p.granted).length === 0 && <li>Nothing: it only computes.</li>}
        </ul>
      )}
      {open === "settings" && (
        <>
          {perSpace && spaceName && <p className="muted">Settings for {spaceName}; other spaces have their own.</p>}
          <SettingsPanel id={p.id} />
        </>
      )}
      {open === "logs" && <LogsPanel id={p.id} />}
    </section>
  );
}

function ApproveDialog({
  plugin,
  onApprove,
  onCancel,
}: {
  plugin: PluginInfo;
  onApprove: (enable: boolean) => Promise<void>;
  onCancel: () => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [enable, setEnable] = useState(true);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    dialog.current?.showModal();
  }, []);
  const pending = plugin.pending!;
  const all = describeCapabilities(pending.manifest.capabilities);

  async function submit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    try {
      await onApprove(enable);
    } finally {
      setBusy(false);
    }
  }

  return (
    <dialog ref={dialog} className="dialog wide" onClose={onCancel} aria-labelledby="approve-title">
      <form onSubmit={(e) => void submit(e)}>
        <h2 id="approve-title">Approve {pending.manifest.name}?</h2>
        <div>
          <ManifestSummary m={pending.manifest} />
        </div>
        {plugin.active ? (
          <>
            <p>Compared with version {plugin.active.version}, it asks to:</p>
            <ul className="plugin-grants">
              {pending.newCapabilities.map((c) => (
                <li key={c}>
                  <strong>{c}</strong>
                </li>
              ))}
            </ul>
          </>
        ) : (
          <p>It asks to:</p>
        )}
        <ul className="plugin-grants">
          {all.map((c) => (
            <li key={c}>{c}</li>
          ))}
          {all.length === 0 && <li>Nothing: it only computes.</li>}
        </ul>
        <p className="muted">
          Approving grants exactly these. It never gets files, programs or other network access.
        </p>
        {!plugin.enabled && (
          <label className="plugin-check">
            <input type="checkbox" checked={enable} onChange={(e) => setEnable(e.target.checked)} />
            <span>Enable it now</span>
          </label>
        )}
        <div className="actions">
          <button type="button" onClick={onCancel}>
            Cancel
          </button>
          <button type="submit" className="primary" disabled={busy}>
            Approve
          </button>
        </div>
      </form>
    </dialog>
  );
}

function SettingsPanel({ id }: { id: string }) {
  const [loaded, setLoaded] = useState<PluginSettings | null>(null);
  const [values, setValues] = useState<Values>({});
  const [secrets, setSecrets] = useState<Record<string, SecretEdit>>({});
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [busy, setBusy] = useState(false);

  const apply = useCallback((s: PluginSettings) => {
    setLoaded(s);
    setValues(s.values as Values);
    setSecrets(Object.fromEntries(secretNames(s.schema as JsonSchema | null).map((k) => [k, { kind: "keep" }])));
  }, []);

  useEffect(() => {
    pluginsApi.settings(id).then(apply, (e) => setError(describeError(e)));
  }, [id, apply]);

  if (!loaded) return error ? <p className="error-text">{error}</p> : <p className="muted">Loading…</p>;
  const schema = loaded.schema as JsonSchema | null;
  if (!schema) return <p className="muted">This plugin has no settings.</p>;

  async function save(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    setSaved(false);
    try {
      apply(await pluginsApi.saveSettings(id, toRequest(schema!, values, secrets)));
      setSaved(true);
    } catch (err) {
      setError(describeError(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form className="plugin-settings" onSubmit={(e) => void save(e)}>
      <SchemaForm
        schema={schema}
        values={values}
        onChange={(v) => {
          setValues(v);
          setSaved(false);
        }}
        secrets={secrets}
        secretsSet={loaded.secretsSet}
        onSecret={(k, edit) => setSecrets({ ...secrets, [k]: edit })}
      />
      {error && (
        <p className="error-text" role="alert">
          {error}
        </p>
      )}
      <div className="actions start">
        <button type="submit" className="primary" disabled={busy}>
          Save settings
        </button>
        {saved && <span className="muted">Saved; the plugin has them now.</span>}
      </div>
    </form>
  );
}

function LogsPanel({ id }: { id: string }) {
  const [entries, setEntries] = useState<PluginLogEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const last = useRef<number | undefined>(undefined);

  useEffect(() => {
    let stopped = false;
    const poll = async () => {
      try {
        const more = await pluginsApi.logs(id, last.current);
        if (stopped || more.length === 0) return;
        last.current = more[more.length - 1]!.seq;
        setEntries((old) => [...old, ...more].slice(-1000));
      } catch (e) {
        if (!stopped) setError(describeError(e));
      }
    };
    void poll();
    const timer = setInterval(() => void poll(), 2000);
    return () => {
      stopped = true;
      clearInterval(timer);
    };
  }, [id]);

  return (
    <div className="plugin-logs" role="log" aria-live="polite">
      {error && <p className="error-text">{error}</p>}
      {entries.length === 0 && <p className="muted">Nothing logged yet.</p>}
      {entries.map((e) => (
        <div key={e.seq} className={`plugin-log plugin-log-${e.level}`}>
          <time>
            {new Date(e.time).toLocaleTimeString()}{" "}
            <small className="muted">{new Date(e.time).toISOString().slice(11, 19)}Z</small>
          </time>
          <span className="plugin-log-level">{e.host ? `host ${e.level}` : e.level}</span>
          <span className="plugin-log-message">{e.message}</span>
        </div>
      ))}
    </div>
  );
}

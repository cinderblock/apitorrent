import { useEffect, useRef, useState } from "react";

import { ApiRequestError, api } from "../api/client";
import type { GroupSettings } from "../api/generated/GroupSettings";
import type { Settings } from "../api/generated/Settings";
import { fromKiB, toKiB } from "../format";

/** A number input that edits a non-negative integer; empty means 0. */
function NumberField({ label, value, onChange, hint }: { label: string; value: number; onChange: (v: number) => void; hint?: string }) {
  return (
    <label className="field">
      <span>
        {label}
        {hint && <span className="muted"> ({hint})</span>}
      </span>
      <input
        type="number"
        min={0}
        step={1}
        inputMode="numeric"
        value={value === 0 ? "" : value}
        placeholder="0"
        onChange={(e) => onChange(Math.max(0, Math.floor(Number(e.target.value) || 0)))}
      />
    </label>
  );
}

const NEW_GROUP: GroupSettings = { name: "", labels: [], downloadRate: 0, uploadRate: 0, maxConnections: 0 };

export function SettingsDialog({ onClose, onImport }: { onClose: () => void; onImport: () => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    dialog.current?.showModal();
    api.settings().then(setSettings, (e: unknown) => setError(e instanceof Error ? e.message : String(e)));
  }, []);

  const update = (patch: Partial<Settings>) => setSettings((s) => (s ? { ...s, ...patch } : s));
  const updateGroup = (i: number, patch: Partial<GroupSettings>) =>
    setSettings((s) => (s ? { ...s, groups: s.groups.map((g, j) => (j === i ? { ...g, ...patch } : g)) } : s));

  async function save(e: React.FormEvent) {
    e.preventDefault();
    if (!settings) return;
    setSaving(true);
    setError(null);
    try {
      await api.saveSettings({ ...settings, groups: settings.groups.map((g) => ({ ...g, name: g.name.trim() })) });
      onClose();
    } catch (err) {
      setError(err instanceof ApiRequestError ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  }

  return (
    <dialog ref={dialog} className="dialog wide" onClose={onClose} aria-labelledby="settings-title">
      <form onSubmit={save}>
        <h2 id="settings-title">Settings</h2>
        {!settings && !error && <p className="muted">Loading…</p>}
        {settings && (
          <>
            <p className="hint muted">Rates are in KiB/s. Empty or 0 means unlimited.</p>
            <fieldset className="settings-section">
              <legend>Bandwidth</legend>
              <div className="settings-grid">
                <NumberField label="Download" value={toKiB(settings.downloadRate)} onChange={(v) => update({ downloadRate: fromKiB(v) })} />
                <NumberField label="Upload" value={toKiB(settings.uploadRate)} onChange={(v) => update({ uploadRate: fromKiB(v) })} />
              </div>
            </fieldset>
            <fieldset className="settings-section">
              <legend>Connections</legend>
              <div className="settings-grid">
                <NumberField label="Open connections" value={settings.maxConnections} onChange={(v) => update({ maxConnections: v })} />
                <NumberField label="Connecting at once" value={settings.maxHalfOpen} onChange={(v) => update({ maxHalfOpen: v })} />
                <NumberField label="New connections" hint="per second" value={settings.connectRate} onChange={(v) => update({ connectRate: v })} />
              </div>
            </fieldset>
            <fieldset className="settings-section">
              <legend>Queue</legend>
              <p className="hint muted">Torrents beyond these numbers wait their turn, in queue order.</p>
              <div className="settings-grid">
                <NumberField
                  label="Active downloads"
                  value={settings.queue.maxActiveDownloads}
                  onChange={(v) => update({ queue: { ...settings.queue, maxActiveDownloads: v } })}
                />
                <NumberField label="Active seeds" value={settings.queue.maxActiveSeeds} onChange={(v) => update({ queue: { ...settings.queue, maxActiveSeeds: v } })} />
                <NumberField label="Active in total" value={settings.queue.maxActive} onChange={(v) => update({ queue: { ...settings.queue, maxActive: v } })} />
              </div>
            </fieldset>
            <fieldset className="settings-section">
              <legend>Library</legend>
              <label className="check">
                <input type="checkbox" checked={settings.library.enabled} onChange={(e) => update({ library: { ...settings.library, enabled: e.target.checked } })} />
                Reuse files other torrents already have instead of downloading them
              </label>
              <label className="check">
                <input
                  type="checkbox"
                  checked={settings.library.allowCopy}
                  disabled={!settings.library.enabled}
                  onChange={(e) => update({ library: { ...settings.library, allowCopy: e.target.checked } })}
                />
                Copy them when they are on another drive (otherwise they are hard-linked, using no extra space)
              </label>
              <label className="check">
                <input
                  type="checkbox"
                  checked={settings.library.retireSuperseded}
                  onChange={(e) => update({ library: { ...settings.library, retireSuperseded: e.target.checked } })}
                />
                Remove torrents a pack has replaced once their tracker drops them (their files stay)
              </label>
            </fieldset>
            <fieldset className="settings-section">
              <legend>File names</legend>
              <label className="field">
                <span>For new torrents</span>
                <select value={settings.naming} onChange={(e) => update({ naming: e.target.value as Settings["naming"] })}>
                  <option value="portable">Portable: names safe on every OS</option>
                  <option value="native">Native: change only what this filesystem forbids</option>
                </select>
              </label>
              <p className="hint muted">
                Portable names can be shared over SMB or moved to exFAT unchanged. Native keeps names like <code>a:b</code> where the filesystem allows them, and
                falls back to portable where it doesn&apos;t. Torrents already added keep their names.
              </p>
            </fieldset>
            <fieldset className="settings-section">
              <legend>Groups</legend>
              <p className="hint muted">Torrents whose label is listed share the group&apos;s limits, on top of the ones above.</p>
              {settings.groups.map((g, i) => (
                <div className="group-row" key={i}>
                  <label className="field">
                    <span>Name</span>
                    <input value={g.name} required onChange={(e) => updateGroup(i, { name: e.target.value })} />
                  </label>
                  <label className="field">
                    <span>Labels, comma-separated</span>
                    <input
                      value={g.labels.join(", ")}
                      onChange={(e) =>
                        updateGroup(i, {
                          labels: e.target.value
                            .split(",")
                            .map((l) => l.trim())
                            .filter((l) => l !== ""),
                        })
                      }
                    />
                  </label>
                  <NumberField label="Download" value={toKiB(g.downloadRate)} onChange={(v) => updateGroup(i, { downloadRate: fromKiB(v) })} />
                  <NumberField label="Upload" value={toKiB(g.uploadRate)} onChange={(v) => updateGroup(i, { uploadRate: fromKiB(v) })} />
                  <NumberField label="Connections" value={g.maxConnections} onChange={(v) => updateGroup(i, { maxConnections: v })} />
                  <button type="button" onClick={() => update({ groups: settings.groups.filter((_, j) => j !== i) })}>
                    Remove
                  </button>
                </div>
              ))}
              <button type="button" onClick={() => update({ groups: [...settings.groups, { ...NEW_GROUP }] })}>
                Add group
              </button>
            </fieldset>
          </>
        )}
        {error && <p className="error-text">{error}</p>}
        <div className="actions">
          <button type="button" className="link actions-start" onClick={onImport}>
            Import from Deluge…
          </button>
          <button type="button" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className="primary" disabled={!settings || saving}>
            {saving ? "Saving…" : "Save"}
          </button>
        </div>
      </form>
    </dialog>
  );
}

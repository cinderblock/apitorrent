import { useEffect, useRef, useState } from "react";

import { ApiRequestError, api } from "../api/client";
import type { Ban } from "../api/generated/Ban";
import type { GroupSettings } from "../api/generated/GroupSettings";
import type { Settings } from "../api/generated/Settings";
import { formatDateTime, fromKiB, toKiB } from "../format";
import { SeedLimitsFields } from "./SeedLimitsFields";
import { TlsSection } from "./TlsSection";
import { UpdatesSection } from "./Updates";

const BAN_REASONS: Record<Ban["reason"], string> = {
  badData: "sent data that failed verification",
  untrusted: "in too many failed pieces",
  manual: "banned by hand",
};

function BansSection() {
  const [bans, setBans] = useState<Ban[] | null>(null);
  const [ip, setIp] = useState("");
  const [error, setError] = useState<string | null>(null);
  const load = () => {
    api.bans().then(setBans, (e: unknown) => setError(e instanceof Error ? e.message : String(e)));
  };
  useEffect(load, []);
  return (
    <fieldset className="settings-section">
      <legend>Banned peers</legend>
      {bans && bans.length === 0 && <p className="hint muted">Nobody is banned.</p>}
      {bans && bans.length > 0 && (
        <ul className="outcomes">
          {bans.map((b) => (
            <li key={b.ip}>
              <code>{b.ip}</code> {BAN_REASONS[b.reason]}
              {b.count > 1 && ` (${b.count} times)`}, until {formatDateTime(b.until)}{" "}
              <button type="button" className="link" onClick={() => void api.unban(b.ip).then(load)}>
                Unban
              </button>
            </li>
          ))}
        </ul>
      )}
      <div className="inline-edit">
        <input aria-label="Address to ban" value={ip} placeholder="IP address" onChange={(e) => setIp(e.target.value)} />
        <button
          type="button"
          disabled={ip.trim() === ""}
          onClick={() =>
            void api.ban(ip.trim()).then(
              () => {
                setIp("");
                setError(null);
                load();
              },
              (e: unknown) => setError(e instanceof Error ? e.message : String(e)),
            )
          }
        >
          Ban
        </button>
      </div>
      {error && <p className="error-text">{error}</p>}
    </fieldset>
  );
}

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

const NEW_GROUP: GroupSettings = { name: "", labels: [], downloadRate: 0, uploadRate: 0, maxConnections: 0, seedLimits: null };

export function SettingsDialog({
  isAdmin,
  onClose,
  onImport,
  onSpaces,
  onUsers,
}: {
  /** Server admins change these settings; everyone else reads them. */
  isAdmin: boolean;
  onClose: () => void;
  /** Open the Deluge import; for this space's admins. */
  onImport?: () => void;
  /** Open the Spaces page; for server admins and spaces' admins. */
  onSpaces?: () => void;
  /** Open the Users page; only for server admins. */
  onUsers?: () => void;
}) {
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
          <fieldset className="settings-all" disabled={!isAdmin}>
            {!isAdmin && (
              <p className="banner-info">These settings apply to every space on this server. Only a server admin can change them.</p>
            )}
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
              <label className="field">
                <span>Encryption</span>
                <select value={settings.encryption} onChange={(e) => update({ encryption: e.target.value as Settings["encryption"] })}>
                  <option value="enabled">Encrypt when the peer can</option>
                  <option value="preferPlaintext">Obfuscate the handshake only, when the peer allows it</option>
                  <option value="required">Encrypted peers only</option>
                  <option value="disabled">Never encrypt</option>
                </select>
              </label>
              <p className="hint muted">
                Encryption hides BitTorrent traffic from network inspection and ISP throttling. It doesn&apos;t verify who the peer is, so it won&apos;t stop
                someone in the middle; your data is safe either way, since every piece is checked against the torrent&apos;s hashes. Requiring it shuts out
                peers that can&apos;t encrypt. Open connections keep theirs.
              </p>
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
              <label className="check">
                <input type="checkbox" checked={settings.queue.ignoreSlow} onChange={(e) => update({ queue: { ...settings.queue, ignoreSlow: e.target.checked } })} />
                Slow torrents don&apos;t count toward the download and seed limits
              </label>
              {settings.queue.ignoreSlow && (
                <>
                  <p className="hint muted">
                    A torrent that stays below these rates for this long keeps running but frees its slot, so the next one in the queue starts. Slow
                    torrents still count toward the total.
                  </p>
                  <div className="settings-grid">
                    <NumberField
                      label="Slow download"
                      hint="KiB/s"
                      value={toKiB(settings.queue.slowDownloadRate)}
                      onChange={(v) => update({ queue: { ...settings.queue, slowDownloadRate: fromKiB(v) } })}
                    />
                    <NumberField
                      label="Slow upload"
                      hint="KiB/s, seeding"
                      value={toKiB(settings.queue.slowUploadRate)}
                      onChange={(v) => update({ queue: { ...settings.queue, slowUploadRate: fromKiB(v) } })}
                    />
                    <NumberField
                      label="For at least"
                      hint="seconds"
                      value={settings.queue.slowAfterSecs}
                      onChange={(v) => update({ queue: { ...settings.queue, slowAfterSecs: v } })}
                    />
                  </div>
                </>
              )}
            </fieldset>
            <fieldset className="settings-section">
              <legend>Seeding limits</legend>
              <p className="hint muted">
                When a finished torrent has shared enough. A group, or the torrent itself (in its details), can set its own. A torrent you resume by
                hand after its limits act seeds on.
              </p>
              <SeedLimitsFields idPrefix="seed" value={settings.seedLimits} onChange={(seedLimits) => update({ seedLimits })} />
            </fieldset>
            <BansSection />
            <TlsSection />
            {isAdmin && <UpdatesSection />}
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
                  checked={settings.library.partialReuse}
                  disabled={!settings.library.enabled}
                  onChange={(e) => update({ library: { ...settings.library, partialReuse: e.target.checked } })}
                />
                Also reuse files that match only in places, such as a repack, and download just the parts that differ (needs a copy of the file)
              </label>
              <div className="settings-grid">
                <NumberField
                  label="Fingerprint files in idle time"
                  hint="KiB/s, 0 is off"
                  value={toKiB(settings.library.fingerprintRate)}
                  onChange={(v) => update({ library: { ...settings.library, fingerprintRate: fromKiB(v) } })}
                />
              </div>
              <p className="hint muted">
                Reads files that came from v1 torrents once, slowly, so a later v2 torrent of the same file is found without reading it again.
              </p>
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
                  <div className="group-seed">
                    <label className="check">
                      <input
                        type="checkbox"
                        checked={g.seedLimits !== null}
                        onChange={(e) => updateGroup(i, { seedLimits: e.target.checked ? { ...settings.seedLimits } : null })}
                      />
                      Its own seeding limits
                    </label>
                    {g.seedLimits && <SeedLimitsFields idPrefix={`group-${i}-seed`} value={g.seedLimits} onChange={(seedLimits) => updateGroup(i, { seedLimits })} />}
                  </div>
                </div>
              ))}
              <button type="button" onClick={() => update({ groups: [...settings.groups, { ...NEW_GROUP }] })}>
                Add group
              </button>
            </fieldset>
          </fieldset>
        )}
        {error && <p className="error-text">{error}</p>}
        <div className="actions">
          {onImport && (
            <button type="button" className="link actions-start" onClick={onImport}>
              Import from Deluge…
            </button>
          )}
          {onSpaces && (
            <button type="button" className="link" onClick={onSpaces}>
              Spaces…
            </button>
          )}
          {onUsers && (
            <button type="button" className="link" onClick={onUsers}>
              Users…
            </button>
          )}
          <button type="button" onClick={onClose}>
            {isAdmin ? "Cancel" : "Close"}
          </button>
          {isAdmin && (
            <button type="submit" className="primary" disabled={!settings || saving}>
              {saving ? "Saving…" : "Save"}
            </button>
          )}
        </div>
      </form>
    </dialog>
  );
}

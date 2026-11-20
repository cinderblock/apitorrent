import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";

import { api } from "../api/client";
import type { BulkReport } from "../api/generated/BulkReport";
import type { SeedLimits } from "../api/generated/SeedLimits";
import type { TorrentSettings } from "../api/generated/TorrentSettings";
import type { UnwantedPolicy } from "../api/generated/UnwantedPolicy";
import { formatRate, toKiB } from "../format";
import { buildEdit, common, describeReport, EMPTY_FORM, trackerCounts, type BulkForm, type Common } from "../state/bulkEdit";
import { describeSeedLimits, NO_SEED_LIMITS, SeedLimitsFields } from "./SeedLimitsFields";

interface Props {
  ids: readonly string[];
  /** Torrent names by id, for the failure list. */
  names: ReadonlyMap<string, string>;
  /** Labels in use in the space, offered as suggestions. */
  labels: readonly string[];
  /** Folders the space's torrents are in, offered as suggestions. */
  folders: readonly string[];
  onClose: () => void;
  /** Select these torrents (the ones that failed). */
  onSelect: (ids: string[]) => void;
}

/** "Mixed (3 values)", or the shared value through `show`. */
function shared<T>(c: Common<T>, show: (v: T) => string): string {
  if (c.kind === "same") return show(c.value);
  if (c.kind === "mixed") return `Mixed (${c.distinct} values)`;
  return "";
}

const rate = (bytes: number) => (bytes > 0 ? `${toKiB(bytes)} KiB/s (${formatRate(bytes)})` : "No limit");

/** A field that can be changed; once it is, "Keep as is" puts it back. */
function Changed({ on, onReset, extra, children }: { on: boolean; onReset: () => void; extra?: ReactNode; children: ReactNode }) {
  return (
    <div className={`bulk-row ${on ? "bulk-changed" : ""}`}>
      {children}
      {on ? (
        <button type="button" className="link" onClick={onReset}>
          Keep as is
        </button>
      ) : (
        extra
      )}
    </div>
  );
}

function numberOrNull(raw: string): number | null {
  if (raw.trim() === "") return null;
  const n = Number(raw);
  return Number.isFinite(n) && n >= 0 ? n : null;
}

/**
 * Change the settings of many torrents at once. Each field starts as "keep": it shows what
 * the selection has (one value, or "Mixed"), and only the fields someone changes are sent.
 */
export function BulkEdit({ ids, names, labels, folders, onClose, onSelect }: Props) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [settings, setSettings] = useState<TorrentSettings[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [form, setForm] = useState<BulkForm>(EMPTY_FORM);
  const [saving, setSaving] = useState(false);
  const [report, setReport] = useState<BulkReport | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);

  useEffect(() => {
    dialog.current?.showModal();
  }, []);
  useEffect(() => {
    let live = true;
    api.torrentSettings([...ids]).then(
      (s) => live && setSettings(s),
      (e: unknown) => live && setLoadError(e instanceof Error ? e.message : String(e)),
    );
    return () => {
      live = false;
    };
  }, [ids]);

  const now = useMemo(() => {
    const s = settings ?? [];
    return {
      label: common(s.map((t) => t.label)),
      down: common(s.map((t) => t.limits.downloadRate)),
      up: common(s.map((t) => t.limits.uploadRate)),
      peers: common(s.map((t) => t.limits.maxConnections)),
      seed: common(s.map((t) => t.seedLimits)),
      unwanted: common(s.map((t) => t.unwantedPolicy)),
      location: common(s.map((t) => t.savePath)),
      trackers: trackerCounts(s),
    };
  }, [settings]);

  const set = (patch: Partial<BulkForm>) => setForm((f) => ({ ...f, ...patch }));
  const edit = buildEdit(ids, form);
  const n = ids.length;
  const what = n === 1 ? "1 torrent" : `${n} torrents`;
  const seedNow = shared(now.seed, (l) => (l === null ? "Following its group or the settings" : `Own: ${describeSeedLimits(l)}`));
  const ownSeed: SeedLimits = form.seed.kind === "own" ? form.seed.limits : now.seed.kind === "same" && now.seed.value ? now.seed.value : NO_SEED_LIMITS;

  async function apply() {
    if (!edit) return;
    setSaving(true);
    setSaveError(null);
    try {
      const r = await api.bulkEdit(edit);
      if (r.failed.length === 0) onClose();
      else setReport(r);
    } catch (e) {
      setSaveError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }

  if (report) return <BulkResult title={describeReport(report)} report={report} names={names} onClose={onClose} onSelect={onSelect} />;

  return (
    <dialog ref={dialog} className="dialog wide" onClose={onClose} aria-labelledby="bulk-title">
      <form
        className="bulk-edit"
        onSubmit={(e) => {
          e.preventDefault();
          void apply();
        }}
      >
        <h2 id="bulk-title">Edit {what}</h2>
        <p className="muted bulk-intro">Only what you change is applied. Everything else stays as each torrent has it.</p>
        {loadError && <p className="error-text">Couldn&apos;t read their settings: {loadError}</p>}
        {!settings && !loadError && <p className="muted">Reading their settings…</p>}
        {settings && (
          <>
            <fieldset className="settings-section">
              <legend>Label</legend>
              <Changed
                on={form.label !== null}
                onReset={() => set({ label: null })}
                extra={
                  now.label.kind !== "same" || now.label.value !== null ? (
                    <button type="button" className="link" onClick={() => set({ label: "" })}>
                      Remove label
                    </button>
                  ) : undefined
                }
              >
                <label className="field grow">
                  <span>{form.label === "" ? "Left empty, the label is removed" : "One label for all of them"}</span>
                  <input
                    list="bulk-labels"
                    value={form.label ?? ""}
                    placeholder={now.label.kind === "same" ? (now.label.value ?? "None") : shared(now.label, String)}
                    onChange={(e) => set({ label: e.target.value })}
                  />
                </label>
              </Changed>
              <datalist id="bulk-labels">
                {labels.map((l) => (
                  <option key={l} value={l} />
                ))}
              </datalist>
            </fieldset>

            <fieldset className="settings-section">
              <legend>Transfer limits</legend>
              <div className="field-row">
                <Changed on={form.down !== null} onReset={() => set({ down: null })}>
                  <label className="field grow">
                    <span>Download, KiB/s (0 is none)</span>
                    <input type="number" min={0} inputMode="numeric" value={form.down ?? ""} placeholder={shared(now.down, rate)} onChange={(e) => set({ down: numberOrNull(e.target.value) })} />
                  </label>
                </Changed>
                <Changed on={form.up !== null} onReset={() => set({ up: null })}>
                  <label className="field grow">
                    <span>Upload, KiB/s (0 is none)</span>
                    <input type="number" min={0} inputMode="numeric" value={form.up ?? ""} placeholder={shared(now.up, rate)} onChange={(e) => set({ up: numberOrNull(e.target.value) })} />
                  </label>
                </Changed>
                <Changed on={form.peers !== null} onReset={() => set({ peers: null })}>
                  <label className="field grow">
                    <span>Peers (0 is the default)</span>
                    <input
                      type="number"
                      min={0}
                      inputMode="numeric"
                      value={form.peers ?? ""}
                      placeholder={shared(now.peers, (p) => (p === null ? "Default" : String(p)))}
                      onChange={(e) => set({ peers: numberOrNull(e.target.value) })}
                    />
                  </label>
                </Changed>
              </div>
            </fieldset>

            <fieldset className="settings-section">
              <legend>Seed limits</legend>
              <div className="bulk-choices" role="radiogroup" aria-label="Seed limits">
                <label className="check">
                  <input type="radio" name="bulk-seed" checked={form.seed.kind === "keep"} onChange={() => set({ seed: { kind: "keep" } })} />
                  Keep as is <span className="muted">({seedNow})</span>
                </label>
                <label className="check">
                  <input type="radio" name="bulk-seed" checked={form.seed.kind === "follow"} onChange={() => set({ seed: { kind: "follow" } })} />
                  Follow their group&apos;s, or the settings&apos;
                </label>
                <label className="check">
                  <input type="radio" name="bulk-seed" checked={form.seed.kind === "own"} onChange={() => set({ seed: { kind: "own", limits: ownSeed } })} />
                  Their own limits
                </label>
              </div>
              {form.seed.kind === "own" && <SeedLimitsFields idPrefix="bulk-seed" value={form.seed.limits} onChange={(limits) => set({ seed: { kind: "own", limits } })} />}
            </fieldset>

            <fieldset className="settings-section">
              <legend>Trackers</legend>
              {now.trackers.length === 0 ? (
                <p className="muted">None of them lists a tracker.</p>
              ) : (
                <ul className="bulk-trackers">
                  {now.trackers.map(({ url, count }) => {
                    const removing = form.remove.has(url);
                    const replacing = form.replace.has(url);
                    return (
                      <li key={url} className={removing ? "bulk-removed" : ""}>
                        <span className="bulk-tracker-url">{url}</span>
                        <span className="muted">{count === n ? `all ${n}` : `${count} of ${n}`}</span>
                        {!removing && !replacing && (
                          <button type="button" className="link" onClick={() => set({ replace: new Map(form.replace).set(url, url) })}>
                            Replace
                          </button>
                        )}
                        <button
                          type="button"
                          className="link"
                          onClick={() => {
                            const remove = new Set(form.remove);
                            if (removing) remove.delete(url);
                            else remove.add(url);
                            const replace = new Map(form.replace);
                            replace.delete(url);
                            set({ remove, replace });
                          }}
                        >
                          {removing ? "Keep" : "Remove"}
                        </button>
                        {replacing && (
                          <span className="bulk-replace">
                            <input
                              aria-label={`Replace ${url} with`}
                              value={form.replace.get(url)}
                              onChange={(e) => set({ replace: new Map(form.replace).set(url, e.target.value) })}
                            />
                            <button
                              type="button"
                              className="link"
                              onClick={() => {
                                const replace = new Map(form.replace);
                                replace.delete(url);
                                set({ replace });
                              }}
                            >
                              Cancel
                            </button>
                          </span>
                        )}
                      </li>
                    );
                  })}
                </ul>
              )}
              <label className="field">
                <span>Add trackers, one per line (each its own tier, where not listed yet)</span>
                <textarea rows={2} value={form.add} placeholder="https://tracker.example/announce" onChange={(e) => set({ add: e.target.value })} />
              </label>
            </fieldset>

            <fieldset className="settings-section">
              <legend>Location</legend>
              <Changed on={form.location !== null} onReset={() => set({ location: null })}>
                <label className="field grow">
                  <span>Move their files to this folder (on the server)</span>
                  <input
                    list="bulk-folders"
                    value={form.location ?? ""}
                    placeholder={now.location.kind === "mixed" ? `Now in ${now.location.distinct} folders` : shared(now.location, String)}
                    onChange={(e) => set({ location: e.target.value })}
                  />
                </label>
              </Changed>
              <datalist id="bulk-folders">
                {folders.map((f) => (
                  <option key={f} value={f} />
                ))}
              </datalist>
              {form.location !== null && form.location.trim() !== "" && (
                <p className="muted bulk-note">
                  The moves run one torrent at a time, in the background. Each shows as Moving until its files are there; one that fails stays where it was and
                  says why.
                </p>
              )}
            </fieldset>

            <fieldset className="settings-section">
              <legend>Deselected files with data</legend>
              <label className="field">
                <select
                  aria-label="What happens to deselected files"
                  value={form.unwanted ?? ""}
                  onChange={(e) => set({ unwanted: e.target.value === "" ? null : (e.target.value as UnwantedPolicy) })}
                >
                  <option value="">Keep as is ({shared(now.unwanted, (p) => (p === "hold" ? "moved to the held area" : "deleted"))})</option>
                  <option value="hold">Move to the held area</option>
                  <option value="delete">Delete</option>
                </select>
              </label>
            </fieldset>
          </>
        )}
        {saveError && <p className="error-text">{saveError}</p>}
        <div className="actions">
          <button type="button" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className="primary" disabled={!edit || saving}>
            {saving ? "Applying…" : `Apply to ${what}`}
          </button>
        </div>
      </form>
    </dialog>
  );
}

/** Which torrents a bulk change failed on, and why; a way to select just those. */
export function BulkResult({
  title,
  report,
  names,
  onClose,
  onSelect,
}: {
  title: string;
  report: BulkReport;
  names: ReadonlyMap<string, string>;
  onClose: () => void;
  onSelect: (ids: string[]) => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    dialog.current?.showModal();
  }, []);
  return (
    <dialog ref={dialog} className="dialog wide" onClose={onClose} aria-labelledby="bulk-result-title">
      <h2 id="bulk-result-title">{title}</h2>
      <ul className="outcomes">
        {report.failed.map((f) => (
          <li key={f.id}>
            <strong>{names.get(f.id) ?? f.id}</strong>: {f.message}
          </li>
        ))}
      </ul>
      <div className="actions">
        <button
          type="button"
          className="actions-start"
          onClick={() => {
            onSelect(report.failed.map((f) => f.id));
            onClose();
          }}
        >
          Select the failed ones
        </button>
        <button type="button" className="primary" onClick={onClose}>
          Close
        </button>
      </div>
    </dialog>
  );
}

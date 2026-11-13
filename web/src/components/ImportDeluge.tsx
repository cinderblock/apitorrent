import { useEffect, useMemo, useRef, useState } from "react";

import { ApiRequestError, api } from "../api/client";
import type { DelugePreview } from "../api/generated/DelugePreview";
import type { ImportReport } from "../api/generated/ImportReport";
import type { PathMapping } from "../api/generated/PathMapping";
import { formatBytes, formatPercent } from "../format";
import { navigate } from "../router";
import { spacesApi } from "../spaces/api";
import { loadSpaceContext, useSpaceContext } from "../spaces/context";
import { spaceHome } from "../spaces/prefix";
import { NO_FILTER, importable, matches, rootsFor, validSlug, within, type WizardFilter } from "./delugeImport";

/** The space to import into: one that exists (by slug), or a new one. */
type Target = { kind: "existing"; slug: string } | { kind: "new" };

const ANY = "\u0000any";

function message(err: unknown): string {
  return err instanceof ApiRequestError ? err.message : String(err);
}

/**
 * Bring in a Deluge installation's torrents, from a directory on the server: choose the
 * installation and the space, look at what it holds, tick what to take, import.
 */
export function ImportDeluge({ onClose }: { onClose: () => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const ctx = useSpaceContext();
  const [configDir, setConfigDir] = useState("");
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  const [target, setTarget] = useState<Target | null>(null);
  const [preview, setPreview] = useState<DelugePreview | null>(null);
  const [filter, setFilter] = useState<WizardFilter>(NO_FILTER);
  const [ticked, setTicked] = useState<Set<string>>(new Set());
  const [newSlug, setNewSlug] = useState("");
  const [newName, setNewName] = useState("");
  const [newRoots, setNewRoots] = useState("");
  // A space made by an earlier attempt that then failed to import: not made again.
  const [created, setCreated] = useState<string | null>(null);
  const [importSettings, setImportSettings] = useState(false);
  const [paused, setPaused] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [report, setReport] = useState<ImportReport | null>(null);

  useEffect(() => {
    dialog.current?.showModal();
  }, []);

  // Spaces this user may import into: the ones they administer.
  const spaces = useMemo(() => ctx?.spaces.filter((s) => s.role === "admin") ?? [], [ctx]);
  const canCreate = ctx?.serverAdmin ?? false;
  const chosen: Target = target ?? (ctx ? { kind: "existing", slug: ctx.current.slug } : { kind: "existing", slug: "" });
  // `null`: the page's own space (also when spaces couldn't be loaded).
  const chosenSlug = chosen.kind === "existing" ? (ctx && chosen.slug !== ctx.current.slug ? chosen.slug : null) : null;

  const pathMap = (): PathMapping[] => (from.trim() && to.trim() ? [{ from: from.trim(), to: to.trim() }] : []);

  async function look(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const p = await api.previewDeluge({ configDir: configDir.trim(), pathMap: pathMap(), newSpace: chosen.kind === "new" }, chosenSlug);
      setPreview(p);
      setFilter(NO_FILTER);
      setTicked(new Set(p.torrents.filter(importable).map((t) => t.id)));
      if (chosen.kind === "new") {
        if (!newSlug) setNewSlug(p.suggestedSlug);
        if (!newName) setNewName(p.suggestedSlug);
        if (!newRoots) {
          const paths = p.torrents.map((t) => t.savePath);
          if (p.downloadLocation) paths.push(p.downloadLocation);
          setNewRoots(rootsFor(paths).join("\n"));
        }
      }
    } catch (err) {
      setError(message(err));
    } finally {
      setBusy(false);
    }
  }

  const shown = useMemo(() => preview?.torrents.filter((t) => matches(t, filter)) ?? [], [preview, filter]);
  const shownImportable = shown.filter(importable);
  const allShownTicked = shownImportable.length > 0 && shownImportable.every((t) => ticked.has(t.id));
  const tickedTorrents = preview?.torrents.filter((t) => ticked.has(t.id)) ?? [];
  const tickedSize = tickedTorrents.reduce((n, t) => n + t.size, 0);

  function tick(id: string, on: boolean) {
    setTicked((s) => {
      const next = new Set(s);
      if (on) next.add(id);
      else next.delete(id);
      return next;
    });
  }

  function tickShown(on: boolean) {
    setTicked((s) => {
      const next = new Set(s);
      for (const t of shownImportable) {
        if (on) next.add(t.id);
        else next.delete(t.id);
      }
      return next;
    });
  }

  const newSpaceProblem =
    chosen.kind !== "new" || created
      ? null
      : !validSlug(newSlug)
        ? "The space's short name takes lowercase letters, digits and hyphens."
        : newName.trim() === ""
          ? "The space needs a name."
          : null;

  async function runImport() {
    if (!preview) return;
    setBusy(true);
    setError(null);
    try {
      let slug = chosenSlug;
      if (chosen.kind === "new") {
        slug = created;
        if (slug === null) {
          const roots = newRoots
            .split(/\r?\n/)
            .map((l) => l.trim())
            .filter((l) => l !== "");
          const download = preview.downloadLocation;
          const space = await spacesApi.create({
            slug: newSlug,
            name: newName.trim(),
            description: `Imported from Deluge (${configDir.trim()})`,
            settings: {
              storageRoots: roots,
              downloadDir: download !== null && within(download, roots) ? download : null,
              folderRules: [],
              downloadRate: 0,
              uploadRate: 0,
              maxConnections: 0,
              maxActive: 0,
              maxActiveDownloads: 0,
              maxActiveSeeds: 0,
            },
          });
          slug = space.slug;
          setCreated(slug);
          await loadSpaceContext();
        }
      }
      const filter = { labels: [], excludeLabels: [], trackers: [], savePaths: [], complete: null, paused: null, name: null, ids: [...ticked] };
      setReport(await api.importDeluge({ configDir: configDir.trim(), pathMap: pathMap(), importSettings, paused, filter }, slug));
    } catch (err) {
      setError(message(err));
    } finally {
      setBusy(false);
    }
  }

  // Where the imported torrents ended up, when that isn't this page's space.
  const elsewhere = chosen.kind === "new" ? created : chosenSlug;

  return (
    <dialog ref={dialog} className="dialog wide" onClose={onClose} aria-labelledby="import-title">
      <h2 id="import-title">Import from Deluge</h2>
      {report ? (
        <>
          <p>
            Imported {report.imported.length} torrent{report.imported.length === 1 ? "" : "s"}
            {report.skipped.length > 0 && `, skipped ${report.skipped.length}`}
            {report.notSelected > 0 && `, left out ${report.notSelected}`}.
          </p>
          {report.skipped.length > 0 && (
            <ul className="outcomes">
              {report.skipped.map((s) => (
                <li key={s.name}>
                  {s.name}: <span className="muted">{s.reason}</span>
                </li>
              ))}
            </ul>
          )}
          {report.warnings.length > 0 && (
            <ul className="outcomes muted">
              {report.warnings.map((w) => (
                <li key={w}>{w}</li>
              ))}
            </ul>
          )}
          <div className="actions">
            {elsewhere && (
              <button
                type="button"
                onClick={() => {
                  onClose();
                  navigate(spaceHome(elsewhere));
                }}
              >
                Open space {elsewhere}
              </button>
            )}
            <button type="button" className="primary" onClick={onClose}>
              Done
            </button>
          </div>
        </>
      ) : !preview ? (
        <form onSubmit={look}>
          <p className="hint muted">
            Torrents keep their files where Deluge left them, with their progress, labels, queue order and totals. Nothing is downloaded again. Deluge can keep
            running: a torrent starts here only once it is paused in Deluge (the import does that for the ones it starts, through Deluge&apos;s daemon), so only one
            client runs it at a time. Each Deluge installation can go into a space of its own.
          </p>
          <label className="field">
            <span>Deluge config directory, on the machine running apitorrent</span>
            <input value={configDir} required placeholder="~/.config/deluge or %APPDATA%\deluge" onChange={(e) => setConfigDir(e.target.value)} />
          </label>
          <div className="field-row">
            <label className="field grow">
              <span>Deluge saved under (optional)</span>
              <input value={from} placeholder="/srv/torrents" onChange={(e) => setFrom(e.target.value)} />
            </label>
            <label className="field grow">
              <span>which is now</span>
              <input value={to} placeholder="D:\torrents" onChange={(e) => setTo(e.target.value)} />
            </label>
          </div>
          {(spaces.length > 1 || canCreate) && (
            <label className="field">
              <span>Into the space</span>
              <select
                value={chosen.kind === "new" ? "" : chosen.slug}
                onChange={(e) => setTarget(e.target.value === "" ? { kind: "new" } : { kind: "existing", slug: e.target.value })}
              >
                {spaces.map((s) => (
                  <option key={s.slug} value={s.slug}>
                    {s.name} ({s.slug})
                  </option>
                ))}
                {canCreate && <option value="">A new space…</option>}
              </select>
            </label>
          )}
          {error && <p className="error-text">{error}</p>}
          <div className="actions">
            <button type="button" onClick={onClose}>
              Cancel
            </button>
            <button type="submit" className="primary" disabled={busy || configDir.trim() === ""}>
              {busy ? "Reading…" : "Next"}
            </button>
          </div>
        </form>
      ) : (
        <div>
          <p className="hint muted">
            {preview.torrents.length} torrent{preview.torrents.length === 1 ? "" : "s"} in {configDir.trim()}.{" "}
            {preview.torrents.some((t) => !importable(t)) && "Ones already in the space, or that can't be imported, can't be ticked."}
          </p>
          {preview.warnings.length > 0 && (
            <ul className="outcomes muted">
              {preview.warnings.map((w) => (
                <li key={w}>{w}</li>
              ))}
            </ul>
          )}
          <div className="field-row">
            <label className="field grow">
              <span>Label</span>
              <select
                value={filter.labels.length === 0 ? ANY : filter.labels[0]}
                onChange={(e) => setFilter({ ...filter, labels: e.target.value === ANY ? [] : [e.target.value] })}
              >
                <option value={ANY}>Any</option>
                <option value="">No label</option>
                {preview.labels.map((l) => (
                  <option key={l} value={l}>
                    {l}
                  </option>
                ))}
              </select>
            </label>
            <label className="field grow">
              <span>Tracker</span>
              <select value={filter.tracker} onChange={(e) => setFilter({ ...filter, tracker: e.target.value })}>
                <option value="">Any</option>
                {preview.trackers.map((h) => (
                  <option key={h} value={h}>
                    {h}
                  </option>
                ))}
              </select>
            </label>
            <label className="field grow">
              <span>Done</span>
              <select value={filter.state} onChange={(e) => setFilter({ ...filter, state: e.target.value as WizardFilter["state"] })}>
                <option value="any">Any</option>
                <option value="complete">Complete</option>
                <option value="incomplete">Incomplete</option>
              </select>
            </label>
            <label className="field grow">
              <span>In Deluge</span>
              <select value={filter.deluge} onChange={(e) => setFilter({ ...filter, deluge: e.target.value as WizardFilter["deluge"] })}>
                <option value="any">Any</option>
                <option value="running">Running</option>
                <option value="paused">Paused</option>
              </select>
            </label>
          </div>
          <label className="field">
            <span>Name contains</span>
            <input value={filter.name} onChange={(e) => setFilter({ ...filter, name: e.target.value })} />
          </label>
          <div className="import-list">
            <table className="files">
              <thead>
                <tr>
                  <th className="import-tick">
                    <input
                      type="checkbox"
                      aria-label="Tick every torrent shown"
                      checked={allShownTicked}
                      disabled={shownImportable.length === 0}
                      onChange={(e) => tickShown(e.target.checked)}
                    />
                  </th>
                  <th>Name</th>
                  <th>Label</th>
                  <th>Size</th>
                  <th>Done</th>
                </tr>
              </thead>
              <tbody>
                {shown.map((t) => {
                  const why = t.present ? "Already in the space" : t.problem;
                  return (
                    <tr key={t.id} className={why ? "muted" : undefined}>
                      <td className="import-tick">
                        <input
                          type="checkbox"
                          aria-label={`Import ${t.name}`}
                          checked={ticked.has(t.id)}
                          disabled={why !== null}
                          onChange={(e) => tick(t.id, e.target.checked)}
                        />
                      </td>
                      <td title={`${t.savePath}${t.trackers.length ? `\n${t.trackers.join(", ")}` : ""}`}>
                        <div className="import-name">{t.name}</div>
                        {why && <div className="import-why">{why}</div>}
                      </td>
                      <td>{t.label ?? ""}</td>
                      <td>{formatBytes(t.size)}</td>
                      <td>
                        {t.progress === null ? "?" : formatPercent(t.progress)}
                        {t.paused && " (paused)"}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
            {shown.length === 0 && <p className="muted">No torrent matches.</p>}
          </div>
          {chosen.kind === "new" && !created && (
            <fieldset className="settings-section">
              <legend>The new space</legend>
              <div className="field-row">
                <label className="field grow">
                  <span>Name</span>
                  <input value={newName} onChange={(e) => setNewName(e.target.value)} />
                </label>
                <label className="field grow">
                  <span>Short name (in its address)</span>
                  <input value={newSlug} onChange={(e) => setNewSlug(e.target.value.toLowerCase())} />
                </label>
              </div>
              <label className="field">
                <span>Storage roots, one per line: the space may save only in these. Empty: anywhere.</span>
                <textarea rows={3} value={newRoots} onChange={(e) => setNewRoots(e.target.value)} />
              </label>
            </fieldset>
          )}
          <label className="check">
            <input type="checkbox" checked={importSettings} onChange={(e) => setImportSettings(e.target.checked)} />
            Also use Deluge&apos;s speed, connection and queue limits
          </label>
          <label className="check">
            <input type="checkbox" checked={paused} onChange={(e) => setPaused(e.target.checked)} />
            Add them paused, and leave them running in Deluge (resuming one here takes it over then)
          </label>
          {newSpaceProblem && <p className="error-text">{newSpaceProblem}</p>}
          {error && <p className="error-text">{error}</p>}
          <div className="actions">
            <button type="button" className="actions-start" disabled={busy || created !== null} onClick={() => setPreview(null)}>
              Back
            </button>
            <button type="button" onClick={onClose}>
              Cancel
            </button>
            <button type="button" className="primary" disabled={busy || ticked.size === 0 || newSpaceProblem !== null} onClick={() => void runImport()}>
              {busy ? "Importing…" : `Import ${ticked.size} (${formatBytes(tickedSize)})`}
            </button>
          </div>
        </div>
      )}
    </dialog>
  );
}

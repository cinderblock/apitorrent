import type { ClientIdentity } from "../api/generated/ClientIdentity";
import type { DelugeImportOptions } from "../api/generated/DelugeImportOptions";
import type { DelugePreview } from "../api/generated/DelugePreview";
import type { DelugePreviewTorrent } from "../api/generated/DelugePreviewTorrent";
import { formatBytes } from "../format";
import { presetIdentity } from "./clientIdentity";
import { carried, labelAfter, renameLabel, settingRows } from "./delugeImport";

type IdentityKind = "deluge" | "none" | "qBittorrent" | "transmission";

function identityName(i: ClientIdentity): string {
  switch (i.client) {
    case "apitorrent":
      return "apitorrent";
    case "deluge":
      return `Deluge ${i.version}`;
    case "qBittorrent":
      return `qBittorrent ${i.version}`;
    case "transmission":
      return `Transmission ${i.version}`;
    case "custom":
      return i.userAgent || "a custom client";
  }
}

const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;
/** The verb for `n` things: `one` for a single one. */
const verb = (n: number, one: string, many: string) => (n === 1 ? one : many);

/** One option: a tick box (or a fixed tick), its name, and what it means for the ticked torrents. */
function Row({
  title,
  fixed = false,
  on,
  set,
  detail,
  children,
}: {
  title: string;
  /** Always comes over. */
  fixed?: boolean;
  on?: boolean;
  set?: (on: boolean) => void;
  detail: React.ReactNode;
  children?: React.ReactNode;
}) {
  return (
    <div className="carry-row">
      <label className="carry-head">
        {set && <input type="checkbox" checked={on} onChange={(e) => set(e.target.checked)} />}
        {fixed && (
          <span className="carry-fixed" aria-label="Always">
            ✓
          </span>
        )}
        <strong>{title}</strong>
      </label>
      <div className="carry-detail muted">{detail}</div>
      {children}
    </div>
  );
}

/**
 * Everything an import brings over from Deluge, for the ticked torrents: each option with
 * what it amounts to, a box to leave it out, and, for labels and the tracker identity, a
 * way to change it on the way in.
 */
export function ImportDelugeOptions({
  preview,
  ticked,
  options: o,
  setOptions,
  defaultSpace,
  paused,
  setPaused,
}: {
  preview: DelugePreview;
  ticked: DelugePreviewTorrent[];
  options: DelugeImportOptions;
  setOptions: (o: DelugeImportOptions) => void;
  /** Into the default space, whose settings are the instance's. */
  defaultSpace: boolean;
  paused: boolean;
  setPaused: (on: boolean) => void;
}) {
  const c = carried(ticked, preview.settings.stopAtRatio);
  const set = <K extends keyof DelugeImportOptions>(k: K) => (v: DelugeImportOptions[K]) => setOptions({ ...o, [k]: v });
  const none = "None of the ticked torrents have any.";
  const kind: IdentityKind = o.identity.kind === "other" ? (o.identity.identity.client as IdentityKind) : o.identity.kind;
  const covered = new Set(preview.coveredHosts);
  const settings = settingRows(preview.settings, defaultSpace);
  const anySetting = Object.values(o.settings).some(Boolean);

  function chooseIdentity(k: IdentityKind) {
    setOptions({
      ...o,
      identity: k === "deluge" || k === "none" ? { kind: k } : { kind: "other", identity: presetIdentity(k) },
    });
  }

  return (
    <fieldset className="settings-section carry">
      <legend>What comes over, for the {plural(c.torrents, "ticked torrent")}</legend>

      <Row
        title="Files and progress"
        fixed
        detail={
          <>
            {formatBytes(c.size)} stay where Deluge left them, with what Deluge had of them: nothing is downloaded again.
            {c.withRenamedFiles > 0 && ` ${plural(c.renamedFiles, "file")} renamed in Deluge (in ${plural(c.withRenamedFiles, "torrent")}) ${verb(c.renamedFiles, "keeps its name", "keep their names")}.`}
          </>
        }
      />

      <Row
        title="Labels"
        on={o.labels}
        set={set("labels")}
        detail={
          c.labels.length === 0 || (c.labels.length === 1 && c.labels[0]?.[0] === "")
            ? none
            : o.labels
              ? "Change a name to rename it here, or empty it to leave that label behind."
              : "Left behind: the torrents come over without labels."
        }
      >
        {o.labels && c.labels.some(([l]) => l !== "") && (
          <table className="carry-labels">
            <thead>
              <tr>
                <th>In Deluge</th>
                <th>Here</th>
              </tr>
            </thead>
            <tbody>
              {c.labels
                .filter(([l]) => l !== "")
                .map(([label, n]) => {
                  const after = labelAfter(o, label);
                  return (
                    <tr key={label}>
                      <td>
                        {label} <span className="muted">({n})</span>
                      </td>
                      <td>
                        <input
                          aria-label={`Label for Deluge's ${label}`}
                          value={after ?? ""}
                          placeholder="no label"
                          onChange={(e) => setOptions(renameLabel(o, label, e.target.value))}
                        />
                      </td>
                    </tr>
                  );
                })}
            </tbody>
          </table>
        )}
      </Row>

      <Row
        title="Queue order"
        on={o.queueOrder}
        set={set("queueOrder")}
        detail={o.queueOrder ? "Queued in Deluge's order, after what the space already has." : "Queued in the order they were added to Deluge."}
      />

      <Row
        title="Dates added"
        on={o.addedDates}
        set={set("addedDates")}
        detail={
          (o.addedDates ? "Each keeps the date it was added to Deluge." : "Each is dated now.") +
          (c.leftovers > 0
            ? ` ${plural(c.leftovers, "leftover .torrent file")} Deluge doesn't list ${verb(c.leftovers, "comes", "come")} over paused${o.addedDates ? ", dated when the file was written" : ""}.`
            : "")
        }
      />

      <Row
        title="Totals and seeding time"
        on={o.totals}
        set={set("totals")}
        detail={
          c.withTotals === 0
            ? none
            : o.totals
              ? `${formatBytes(c.downloaded)} downloaded and ${formatBytes(c.uploaded)} uploaded in all, and the time spent seeding. Ratios and seed limits count from these.`
              : "Start at zero here: ratios and seed limits count from now."
        }
      />

      <Row
        title="Skipped files"
        on={o.skippedFiles}
        set={set("skippedFiles")}
        detail={
          c.withSkippedFiles === 0
            ? none
            : o.skippedFiles
              ? `${plural(c.skippedFiles, "file")} Deluge doesn't download (in ${plural(c.withSkippedFiles, "torrent")}) ${verb(c.skippedFiles, "stays", "stay")} skipped.`
              : `${plural(c.skippedFiles, "file")} Deluge skips ${verb(c.skippedFiles, "is", "are")} downloaded here.`
        }
      />

      <Row
        title="Deluge's tracker lists"
        on={o.trackers}
        set={set("trackers")}
        detail={
          c.editedTrackers === 0
            ? "None of the ticked torrents' trackers were changed in Deluge."
            : o.trackers
              ? `${plural(c.editedTrackers, "torrent")} ${verb(c.editedTrackers, "announces", "announce")} to the trackers as edited in Deluge (a new passkey, say).`
              : `${plural(c.editedTrackers, "torrent")} ${verb(c.editedTrackers, "goes", "go")} back to the trackers in the .torrent file, undoing what was edited in Deluge.`
        }
      />

      <Row
        title="Speed, connection and upload slot limits"
        on={o.torrentLimits}
        set={set("torrentLimits")}
        detail={
          c.withLimits === 0
            ? none
            : o.torrentLimits
              ? `${plural(c.withLimits, "torrent")} ${verb(c.withLimits, "keeps its", "keep their")} own limits.`
              : `${plural(c.withLimits, "torrent")} ${verb(c.withLimits, "loses its", "lose their")} own limits; the space's apply.`
        }
      />

      <Row
        title="Stop seeding at a ratio"
        on={o.seedLimits}
        set={set("seedLimits")}
        detail={
          c.ownRatio === 0
            ? "None of the ticked torrents have a ratio of their own (Deluge's global one is below)."
            : o.seedLimits
              ? `${plural(c.ownRatio, "torrent")} ${verb(c.ownRatio, "keeps a stop-at-ratio of its", "keep a stop-at-ratio of their")} own.`
              : `${plural(c.ownRatio, "torrent")} ${verb(c.ownRatio, "loses its", "lose their")} own stop-at-ratio.`
        }
      />

      <Row
        title="Move when complete"
        on={o.moveCompleted}
        set={set("moveCompleted")}
        detail={
          c.moveCompleted === 0
            ? none
            : o.moveCompleted
              ? `${plural(c.moveCompleted, "torrent")} ${verb(c.moveCompleted, "moves its", "move their")} files where Deluge would once complete, and ${verb(c.moveCompleted, "seeds", "seed")} from there.`
              : `${plural(c.moveCompleted, "torrent")} ${verb(c.moveCompleted, "keeps its", "keep their")} files where they download.`
        }
      />

      <Row
        title="Sequential download"
        on={o.sequential}
        set={set("sequential")}
        detail={
          c.sequential === 0
            ? none
            : o.sequential
              ? `${plural(c.sequential, "torrent")} ${verb(c.sequential, "downloads its", "download their")} files in order, each from start to end.`
              : `${plural(c.sequential, "torrent")} ${verb(c.sequential, "downloads", "download")} rarest pieces first, as usual.`
        }
      />

      <Row
        title="First and last pieces first"
        on={o.firstLast}
        set={set("firstLast")}
        detail={
          c.firstLast === 0
            ? none
            : o.firstLast
              ? `${plural(c.firstLast, "torrent")} ${verb(c.firstLast, "fetches", "fetch")} each file's first and last piece before the rest, for previews.`
              : `${plural(c.firstLast, "torrent")} ${verb(c.firstLast, "downloads", "download")} rarest pieces first, as usual.`
        }
      />

      <Row
        title="Super-seeding"
        on={o.superSeed}
        set={set("superSeed")}
        detail={
          c.superSeed === 0
            ? none
            : o.superSeed
              ? `${plural(c.superSeed, "torrent")} ${verb(c.superSeed, "shows", "show")} peers a piece or two at a time, to get a new torrent's first copy out with less upload.`
              : `${plural(c.superSeed, "torrent")} ${verb(c.superSeed, "seeds", "seed")} as usual, showing peers every piece.`
        }
      />

      <Row
        title="Client private trackers see"
        detail={
          c.privateHosts.length === 0
            ? "None of the ticked torrents are private: public trackers see apitorrent."
            : "Private trackers knew these torrents as Deluge's, and many allow only clients on a list. Some forbid pretending to be another client: check your trackers' rules. Change it later in Settings → Client identity."
        }
      >
        {c.privateHosts.length > 0 && (
          <>
            <label className="field">
              <span>Present as</span>
              <select value={kind} onChange={(e) => chooseIdentity(e.target.value as IdentityKind)}>
                <option value="deluge">{identityName(preview.delugeIdentity)} (as before)</option>
                <option value="none">apitorrent (no rules)</option>
                <option value="qBittorrent">{identityName(presetIdentity("qBittorrent"))}</option>
                <option value="transmission">{identityName(presetIdentity("transmission"))}</option>
              </select>
            </label>
            {kind !== "none" && (
              <ul className="carry-hosts">
                {c.privateHosts.map(([host, n]) => {
                  const skip = o.identitySkipHosts.some((h) => h.toLowerCase() === host);
                  return (
                    <li key={host}>
                      <label className="check">
                        <input
                          type="checkbox"
                          checked={!covered.has(host) && !skip}
                          disabled={covered.has(host)}
                          onChange={(e) =>
                            setOptions({
                              ...o,
                              identitySkipHosts: e.target.checked ? o.identitySkipHosts.filter((h) => h.toLowerCase() !== host) : [...o.identitySkipHosts, host],
                            })
                          }
                        />
                        {host} <span className="muted">({plural(n, "torrent")})</span>
                        {covered.has(host) && <span className="muted"> · already has a rule, left as it is</span>}
                      </label>
                    </li>
                  );
                })}
              </ul>
            )}
          </>
        )}
      </Row>

      <Row
        title="Start the running ones here"
        on={!paused}
        set={(on) => setPaused(!on)}
        detail={
          paused
            ? `All come over paused and keep running in Deluge. Resuming one here pauses it in Deluge first.`
            : c.running === 0
              ? "None of the ticked torrents run in Deluge: they come over paused."
              : `${plural(c.running, "torrent")} running in Deluge ${verb(c.running, "is", "are")} paused there, then started here. The rest come over paused.`
        }
      />

      <div className="carry-row">
        <div className="carry-head">
          <strong>Deluge&apos;s own settings</strong>
        </div>
        <div className="carry-detail muted">
          {anySetting
            ? defaultSpace
              ? "The ticked ones replace this instance's settings."
              : `The ticked ones replace the space's settings${o.settings.stopAtRatio ? "; the ratio goes on each imported torrent, since spaces have no ratio setting" : ""}.`
            : "None taken: the space keeps its own."}
        </div>
        <ul className="carry-settings">
          {settings.map((r) => (
            <li key={r.key}>
              <label className="check">
                <input type="checkbox" checked={o.settings[r.key]} onChange={(e) => setOptions({ ...o, settings: { ...o.settings, [r.key]: e.target.checked } })} />
                {r.label}: <span className="muted">{r.value}</span>
                {r.changed && <span className="carry-changed">changed in Deluge</span>}
              </label>
              {r.note && <div className="carry-detail muted">{r.note}</div>}
            </li>
          ))}
        </ul>
      </div>

      {(c.notCarried.length > 0 || preview.labelOptions.length > 0) && (
        <div className="carry-row">
          <div className="carry-head">
            <span className="carry-fixed carry-no" aria-label="Not carried">
              ✗
            </span>
            <strong>Can&apos;t come over</strong>
          </div>
          <div className="carry-detail muted">apitorrent has nothing like these, so they stay behind:</div>
          <ul className="outcomes muted">
            {c.notCarried.map(([note, n]) => (
              <li key={note}>
                {note} ({plural(n, "torrent")})
              </li>
            ))}
            {preview.labelOptions.map((note) => (
              <li key={note}>{note}</li>
            ))}
          </ul>
        </div>
      )}
    </fieldset>
  );
}

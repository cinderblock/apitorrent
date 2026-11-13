import type { ClientIdentity } from "../api/generated/ClientIdentity";
import type { IdentitySettings } from "../api/generated/IdentitySettings";
import type { TrackerIdentityRule } from "../api/generated/TrackerIdentityRule";
import { CLIENT_LABELS, type ClientKind, describeIdentity, hostProblem, identityProblems, moveRule, presetIdentity } from "./clientIdentity";

const KINDS = Object.keys(CLIENT_LABELS) as ClientKind[];

/** The preset, its versions, and what trackers will see. */
function IdentityFields({ value, onChange, idPrefix }: { value: ClientIdentity; onChange: (v: ClientIdentity) => void; idPrefix: string }) {
  const seen = describeIdentity(value);
  return (
    <div className="identity-fields">
      <label className="field">
        <span>Present as</span>
        <select id={`${idPrefix}-client`} value={value.client} onChange={(e) => onChange(presetIdentity(e.target.value as ClientKind))}>
          {KINDS.map((k) => (
            <option key={k} value={k}>
              {CLIENT_LABELS[k]}
            </option>
          ))}
        </select>
      </label>
      {(value.client === "deluge" || value.client === "qBittorrent" || value.client === "transmission") && (
        <label className="field">
          <span>Version</span>
          <input id={`${idPrefix}-version`} value={value.version} required onChange={(e) => onChange({ ...value, version: e.target.value })} />
        </label>
      )}
      {value.client === "deluge" && (
        <label className="field">
          <span>libtorrent version</span>
          <input id={`${idPrefix}-libtorrent`} value={value.libtorrent} required onChange={(e) => onChange({ ...value, libtorrent: e.target.value })} />
        </label>
      )}
      {value.client === "custom" && (
        <>
          <label className="field">
            <span>Peer id prefix</span>
            <input id={`${idPrefix}-prefix`} value={value.peerIdPrefix} maxLength={20} required onChange={(e) => onChange({ ...value, peerIdPrefix: e.target.value })} />
          </label>
          <label className="field">
            <span>User agent</span>
            <input id={`${idPrefix}-agent`} value={value.userAgent} required onChange={(e) => onChange({ ...value, userAgent: e.target.value })} />
          </label>
          <label className="field">
            <span>Client name for peers</span>
            <input id={`${idPrefix}-name`} value={value.clientName} required onChange={(e) => onChange({ ...value, clientName: e.target.value })} />
          </label>
        </>
      )}
      {value.client !== "apitorrent" && (
        <p className="hint muted identity-preview">
          {seen ? (
            <>
              Peer id <code>{seen.peerIdPrefix}…</code>, user agent <code>{seen.userAgent}</code>
            </>
          ) : (
            <span className="error-text">Not a valid {CLIENT_LABELS[value.client]} identity</span>
          )}
        </p>
      )}
    </div>
  );
}

const NEW_RULE: TrackerIdentityRule = { host: "", identity: presetIdentity("deluge") };

/** The settings section: the default identity and the per-tracker rules. */
export function ClientIdentitySection({ value, onChange }: { value: IdentitySettings; onChange: (v: IdentitySettings) => void }) {
  const setRule = (i: number, patch: Partial<TrackerIdentityRule>) => onChange({ ...value, rules: value.rules.map((r, j) => (j === i ? { ...r, ...patch } : r)) });
  const problems = identityProblems(value);
  return (
    <fieldset className="settings-section">
      <legend>Client identity</legend>
      <p className="banner-warn">
        Many private trackers forbid presenting as another client, and can ban your account for it. Only use this for a tracker that allows it, or
        that only accepts clients on its list. Check its rules first.
      </p>
      <p className="hint muted">
        Trackers and peers see which client you use from its peer id and user agent. A torrent uses the first rule below that matches one of its
        trackers&apos; hosts, or else the default. A change applies from each torrent&apos;s next announce. The DHT always sees apitorrent.
      </p>
      <IdentityFields idPrefix="identity-default" value={value.default} onChange={(d) => onChange({ ...value, default: d })} />
      {value.rules.length > 0 && <h4 className="identity-rules-title">Per tracker</h4>}
      {value.rules.map((r, i) => {
        const bad = r.host.trim() !== "" ? hostProblem(r.host) : null;
        return (
          <div className="identity-rule" key={i}>
            <label className="field">
              <span>Tracker host</span>
              <input
                id={`identity-rule-${i}-host`}
                value={r.host}
                required
                placeholder="tracker.example.org or *.example.org"
                aria-invalid={bad !== null}
                onChange={(e) => setRule(i, { host: e.target.value })}
              />
              {bad && <span className="error-text">{bad}</span>}
            </label>
            <IdentityFields idPrefix={`identity-rule-${i}`} value={r.identity} onChange={(identity) => setRule(i, { identity })} />
            <div className="identity-rule-actions">
              <button type="button" aria-label="Move up" title="Move up: an earlier rule wins" disabled={i === 0} onClick={() => onChange({ ...value, rules: moveRule(value.rules, i, -1) })}>
                ↑
              </button>
              <button
                type="button"
                aria-label="Move down"
                title="Move down"
                disabled={i === value.rules.length - 1}
                onClick={() => onChange({ ...value, rules: moveRule(value.rules, i, 1) })}
              >
                ↓
              </button>
              <button type="button" onClick={() => onChange({ ...value, rules: value.rules.filter((_, j) => j !== i) })}>
                Remove
              </button>
            </div>
          </div>
        );
      })}
      <button type="button" onClick={() => onChange({ ...value, rules: [...value.rules, { ...NEW_RULE, identity: presetIdentity("deluge") }] })}>
        Add tracker rule
      </button>
      {problems.length > 0 && (
        <ul className="error-text identity-problems">
          {problems.map((p) => (
            <li key={p}>{p}</li>
          ))}
        </ul>
      )}
    </fieldset>
  );
}

import type { ClientIdentity } from "../api/generated/ClientIdentity";
import type { DhtBit } from "../api/generated/DhtBit";
import type { IdentitySettings } from "../api/generated/IdentitySettings";
import type { PeerWire } from "../api/generated/PeerWire";
import type { TrackerIdentityRule } from "../api/generated/TrackerIdentityRule";
import {
  CLIENT_LABELS,
  type ClientKind,
  MAX_REQUEST_QUEUE,
  describeIdentity,
  hostProblem,
  identityProblems,
  moveRule,
  presetIdentity,
  wireOf,
} from "./clientIdentity";
import { AmountInput, type Choice, ChoiceCards, Empty, Note, SettingCard } from "../settings/controls";

const KINDS = Object.keys(CLIENT_LABELS) as ClientKind[];

const DHT_BIT_CHOICES: readonly Choice<DhtBit>[] = [
  { value: "always", label: "Always", description: "libtorrent: Deluge, qBittorrent" },
  { value: "whenRunning", label: "While the DHT runs", description: "apitorrent" },
  { value: "publicWhenRunning", label: "Public torrents, while the DHT runs", description: "Transmission" },
];

/** A custom identity's handshake details, which only peers see. */
function WireFields({ value, onChange, idPrefix }: { value: PeerWire; onChange: (v: PeerWire) => void; idPrefix: string }) {
  const q = value.requestQueue;
  const badQueue = !Number.isInteger(q) || q < 1 || q > MAX_REQUEST_QUEUE;
  return (
    <fieldset className="identity-wire">
      <legend>What peers see</legend>
      <label className="field">
        <span>Request queue (reqq)</span>
        <AmountInput id={`${idPrefix}-reqq`} value={q} max={MAX_REQUEST_QUEUE} onChange={(requestQueue) => onChange({ ...value, requestQueue })} />
        {badQueue && <span className="error-text">1 to {MAX_REQUEST_QUEUE}</span>}
      </label>
      <div className="identity-wire-dht">
        <span>DHT bit in the handshake</span>
        <ChoiceCards label="DHT bit in the handshake" columns={3} value={value.dhtBit} options={DHT_BIT_CHOICES} onChange={(dhtBit) => onChange({ ...value, dhtBit })} />
      </div>
      <label className="check">
        <input type="checkbox" checked={value.v2Bit} onChange={(e) => onChange({ ...value, v2Bit: e.target.checked })} />
        v2 bit for hybrid torrents
      </label>
      <label className="check">
        <input type="checkbox" checked={value.libtorrentExtensions} onChange={(e) => onChange({ ...value, libtorrentExtensions: e.target.checked })} />
        Offer libtorrent&apos;s extensions (upload_only, lt_donthave, ut_holepunch)
      </label>
    </fieldset>
  );
}

/** "reqq 512, no v2 bit, …" in a few words. */
function WireWords({ wire }: { wire: PeerWire }) {
  const dht = { always: "the DHT bit always", whenRunning: "the DHT bit while the DHT runs", publicWhenRunning: "the DHT bit on public torrents" }[wire.dhtBit];
  return (
    <>
      <code>reqq {wire.requestQueue}</code>, {dht}, {wire.v2Bit ? "the v2 bit on hybrid torrents" : "no v2 bit on hybrid torrents"}
      {wire.libtorrentExtensions ? ", and libtorrent's extensions" : ", and none of libtorrent's extensions"}
    </>
  );
}

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
          <WireFields idPrefix={idPrefix} value={value.wire} onChange={(wire) => onChange({ ...value, wire })} />
        </>
      )}
      {value.client !== "apitorrent" && (
        <p className="hint muted identity-preview">
          {seen ? (
            <>
              Peer id <code>{seen.peerIdPrefix}…</code>, user agent <code>{seen.userAgent}</code>. Peers see <WireWords wire={wireOf(value)} />.
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

/** Settings → Client identity: the default identity and the per-tracker rules. */
export function ClientIdentitySection({ value, onChange }: { value: IdentitySettings; onChange: (v: IdentitySettings) => void }) {
  const setRule = (i: number, patch: Partial<TrackerIdentityRule>) => onChange({ ...value, rules: value.rules.map((r, j) => (j === i ? { ...r, ...patch } : r)) });
  const problems = identityProblems(value);
  return (
    <>
      <Note tone="warn">
        Many private trackers forbid presenting as another client, and can ban your account for it. Only use this for a tracker that allows it, or that only
        accepts clients on its list. Check its rules first.
      </Note>
      <SettingCard
        title="Default"
        description="Trackers and peers tell which client you use from its peer id and user agent. A change applies from each torrent's next announce. The DHT always sees apitorrent."
      >
        <IdentityFields idPrefix="identity-default" value={value.default} onChange={(d) => onChange({ ...value, default: d })} />
      </SettingCard>
      <SettingCard
        title="Per tracker"
        description="A torrent uses the first rule that matches one of its trackers' hosts, or else the default."
        footer={
          <button type="button" onClick={() => onChange({ ...value, rules: [...value.rules, { ...NEW_RULE, identity: presetIdentity("deluge") }] })}>
            + Add tracker rule
          </button>
        }
      >
        {value.rules.length === 0 && <Empty>No rules: every tracker sees the default.</Empty>}
        {value.rules.map((r, i) => {
          const bad = r.host.trim() !== "" ? hostProblem(r.host) : null;
          return (
            <div className="identity-rule" key={i}>
              <div className="identity-rule-head">
                <span className="identity-rule-n">{i + 1}</span>
                <label className="field identity-rule-host">
                  <span>Tracker host</span>
                  <input
                    id={`identity-rule-${i}-host`}
                    value={r.host}
                    placeholder="tracker.example.org or *.example.org"
                    aria-invalid={bad !== null}
                    onChange={(e) => setRule(i, { host: e.target.value })}
                  />
                  {bad && <span className="error-text">{bad}</span>}
                </label>
                <div className="identity-rule-actions">
                  <button type="button" aria-label={`Move rule ${i + 1} up`} disabled={i === 0} onClick={() => onChange({ ...value, rules: moveRule(value.rules, i, -1) })}>
                    ↑
                  </button>
                  <button
                    type="button"
                    aria-label={`Move rule ${i + 1} down`}
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
              <IdentityFields idPrefix={`identity-rule-${i}`} value={r.identity} onChange={(identity) => setRule(i, { identity })} />
            </div>
          );
        })}
        {problems.length > 0 && (
          <ul className="error-text identity-problems">
            {problems.map((p) => (
              <li key={p}>{p}</li>
            ))}
          </ul>
        )}
      </SettingCard>
    </>
  );
}

// Distribution: channels this instance publishes, and channels it subscribes to on other
// instances. Admin-only on the server.

import { useCallback, useEffect, useState, type FormEvent } from "react";

import { ApiRequestError } from "../api/client";
import type { ChannelInfo } from "../api/generated/ChannelInfo";
import type { RuleInfo } from "../api/generated/RuleInfo";
import type { SubscriptionInfo } from "../api/generated/SubscriptionInfo";
import type { SubscriptionTrust } from "../api/generated/SubscriptionTrust";
import { describeError } from "../auth/flow";
import { rulesApi } from "./api";
import { When } from "./When";

export function ChannelsSection({ rules }: { rules: RuleInfo[] }) {
  const [channels, setChannels] = useState<ChannelInfo[] | null>(null);
  const [subs, setSubs] = useState<SubscriptionInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [adminOnly, setAdminOnly] = useState(false);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    try {
      const [c, s] = await Promise.all([rulesApi.channels(), rulesApi.subscriptions()]);
      setChannels(c);
      setSubs(s);
    } catch (e) {
      if (e instanceof ApiRequestError && e.code === "admin_only") setAdminOnly(true);
      else setError(describeError(e));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const act = async (f: () => Promise<unknown>) => {
    setBusy(true);
    setError(null);
    try {
      await f();
      await load();
      return true;
    } catch (e) {
      setError(describeError(e));
      return false;
    } finally {
      setBusy(false);
    }
  };

  if (adminOnly) {
    return (
      <section className="account-section">
        <h2>Distribution</h2>
        <p className="muted">Only an admin can publish channels and subscribe to other servers.</p>
      </section>
    );
  }

  const key = channels?.[0]?.publisherKey;
  return (
    <>
      <section className="account-section">
        <h2>Channels this server publishes</h2>
        <p className="muted">
          A channel is a signed list of torrents, chosen by a rule or a label, that other apitorrent servers can
          subscribe to with an API token (a read-only one is enough). Each change gets a new revision; subscribers
          check the signature against this server's channel key and run their own rules over the list.
        </p>
        {key && (
          <p>
            Channel key, for subscribers to pin: <code className="rule-key">{key}</code>
          </p>
        )}
        {error && (
          <p className="error-text" role="alert">
            {error}
          </p>
        )}
        {channels?.length === 0 && <p className="muted">No channels yet.</p>}
        {channels && channels.length > 0 && (
          <table className="list">
            <thead>
              <tr>
                <th>Channel</th>
                <th>Publishes</th>
                <th>Revision</th>
                <th>Feed</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {channels.map((c) => (
                <tr key={c.id}>
                  <td>
                    <strong>{c.channel.name}</strong> <span className="muted">{c.id}</span>
                    {!c.channel.enabled && <span className="badge badge-warn">off</span>}
                    {c.channel.description && <div className="muted">{c.channel.description}</div>}
                  </td>
                  <td>
                    {c.channel.selector.kind === "label"
                      ? `label ${c.channel.selector.label}`
                      : `rule “${rules.find((r) => c.channel.selector.kind === "rule" && r.id === c.channel.selector.ruleId)?.rule.name ?? "?"}”`}
                    <div className="muted">
                      {c.entries} torrents, {c.channel.includeMetainfo ? "with .torrent files" : "magnet links only"}
                    </div>
                  </td>
                  <td>
                    {c.revision}
                    <div className="muted">
                      <When ms={c.updatedAt} />
                    </div>
                  </td>
                  <td>
                    <code>/api/v1/channels/{c.id}/feed</code>
                  </td>
                  <td>
                    <button
                      type="button"
                      disabled={busy}
                      onClick={() => void act(() => rulesApi.updateChannel(c.id, { ...c.channel, enabled: !c.channel.enabled }))}
                    >
                      {c.channel.enabled ? "Stop publishing" : "Publish"}
                    </button>{" "}
                    <button type="button" className="danger" disabled={busy} onClick={() => void act(() => rulesApi.removeChannel(c.id))}>
                      Delete
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        <NewChannel rules={rules} busy={busy} act={act} />
      </section>
      <section className="account-section">
        <h2>Channels this server follows</h2>
        <p className="muted">
          Entries from a subscribed channel become candidates from <code>channel:&lt;id&gt;</code>; your rules decide
          which to take. TLS is always verified: trust the publisher's certificate through this computer, a private
          CA, or its pairing code (pinned).
        </p>
        {subs?.length === 0 && <p className="muted">No subscriptions yet.</p>}
        {subs && subs.length > 0 && (
          <table className="list">
            <thead>
              <tr>
                <th>Subscription</th>
                <th>Revision</th>
                <th>Last checked</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {subs.map((s) => (
                <tr key={s.id}>
                  <td>
                    <strong>{s.name}</strong> <span className="muted">channel:{s.id}</span>
                    {!s.enabled && <span className="badge badge-warn">off</span>}
                    <div className="muted">
                      {s.channel} at {s.url}
                    </div>
                    <div className="muted">
                      {s.publisherKey ? (
                        <>
                          Publisher key pinned: <code className="rule-key">{s.publisherKey}</code>
                        </>
                      ) : (
                        "The first feed's key will be pinned."
                      )}
                    </div>
                  </td>
                  <td>
                    {s.revision ?? "–"}
                    <div className="muted">{s.entries} entries</div>
                  </td>
                  <td>
                    {s.lastChecked ? <When ms={s.lastChecked} /> : <span className="muted">not yet</span>}
                    {s.lastError && <div className="error-text">{s.lastError}</div>}
                  </td>
                  <td>
                    <button type="button" disabled={busy} onClick={() => void act(() => rulesApi.refresh(s.id))}>
                      Check now
                    </button>{" "}
                    <button type="button" className="danger" disabled={busy} onClick={() => void act(() => rulesApi.unsubscribe(s.id))}>
                      Unsubscribe
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        <NewSubscription busy={busy} act={act} />
      </section>
    </>
  );
}

type Act = (f: () => Promise<unknown>) => Promise<boolean>;

function NewChannel({ rules, busy, act }: { rules: RuleInfo[]; busy: boolean; act: Act }) {
  const [open, setOpen] = useState(false);
  const [id, setId] = useState("");
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [kind, setKind] = useState<"label" | "rule">("label");
  const [label, setLabel] = useState("");
  const [ruleId, setRuleId] = useState(rules[0]?.id ?? "");
  const [metainfo, setMetainfo] = useState(true);

  if (!open) {
    return (
      <button type="button" onClick={() => setOpen(true)}>
        New channel…
      </button>
    );
  }
  async function submit(e: FormEvent) {
    e.preventDefault();
    const ok = await act(() =>
      rulesApi.createChannel({
        id: id.trim(),
        name: name.trim(),
        description: description.trim(),
        selector: kind === "label" ? { kind, label: label.trim() } : { kind, ruleId },
        includeMetainfo: metainfo,
        enabled: true,
      }),
    );
    if (ok) setOpen(false);
  }
  return (
    <form className="rule-subform" onSubmit={(e) => void submit(e)}>
      <div className="field-row">
        <label className="field">
          <span>
            Id <span className="muted">(lowercase, digits, -)</span>
          </span>
          <input required pattern="[a-z][a-z0-9-]{0,63}" value={id} onChange={(e) => setId(e.target.value)} />
        </label>
        <label className="field grow">
          <span>Name</span>
          <input required value={name} onChange={(e) => setName(e.target.value)} />
        </label>
      </div>
      <label className="field">
        <span>Description</span>
        <input value={description} onChange={(e) => setDescription(e.target.value)} />
      </label>
      <div className="field-row">
        <label className="field">
          <span>Publish</span>
          <select value={kind} onChange={(e) => setKind(e.target.value as "label" | "rule")}>
            <option value="label">every torrent with a label</option>
            <option value="rule" disabled={rules.length === 0}>
              what a rule keeps
            </option>
          </select>
        </label>
        {kind === "label" ? (
          <label className="field grow">
            <span>Label</span>
            <input required value={label} onChange={(e) => setLabel(e.target.value)} />
          </label>
        ) : (
          <label className="field grow">
            <span>Rule</span>
            <select value={ruleId} onChange={(e) => setRuleId(e.target.value)}>
              {rules.map((r) => (
                <option key={r.id} value={r.id}>
                  {r.rule.name}
                </option>
              ))}
            </select>
          </label>
        )}
      </div>
      <label className="check">
        <input type="checkbox" checked={metainfo} onChange={(e) => setMetainfo(e.target.checked)} />
        Include the .torrent files (otherwise magnet links only)
      </label>
      <div className="actions start">
        <button type="submit" className="primary" disabled={busy}>
          Create
        </button>
        <button type="button" onClick={() => setOpen(false)}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function NewSubscription({ busy, act }: { busy: boolean; act: Act }) {
  const [open, setOpen] = useState(false);
  const [name, setName] = useState("");
  const [url, setUrl] = useState("https://");
  const [channel, setChannel] = useState("");
  const [token, setToken] = useState("");
  const [trustKind, setTrustKind] = useState<SubscriptionTrust["kind"]>("pinned");
  const [pin, setPin] = useState("");
  const [caPem, setCaPem] = useState("");
  const [key, setKey] = useState("");
  const [interval, setIntervalMinutes] = useState(30);

  if (!open) {
    return (
      <button type="button" onClick={() => setOpen(true)}>
        Subscribe to a channel…
      </button>
    );
  }
  async function submit(e: FormEvent) {
    e.preventDefault();
    const trust: SubscriptionTrust =
      trustKind === "pinned" ? { kind: "pinned", pin: pin.trim() }
      : trustKind === "privateCa" ? { kind: "privateCa", caPem }
      : { kind: "os" };
    const ok = await act(() =>
      rulesApi.subscribe({
        name: name.trim(),
        url: url.trim(),
        channel: channel.trim(),
        token: token.trim() || null,
        trust,
        publisherKey: key.trim() || null,
        intervalMinutes: interval,
        enabled: true,
      }),
    );
    if (ok) setOpen(false);
  }
  return (
    <form className="rule-subform" onSubmit={(e) => void submit(e)} autoComplete="off">
      <div className="field-row">
        <label className="field grow">
          <span>Name</span>
          <input required value={name} onChange={(e) => setName(e.target.value)} />
        </label>
        <label className="field grow">
          <span>Publisher's address</span>
          <input required type="url" value={url} onChange={(e) => setUrl(e.target.value)} />
        </label>
        <label className="field">
          <span>Channel id</span>
          <input required value={channel} onChange={(e) => setChannel(e.target.value)} />
        </label>
      </div>
      <label className="field">
        <span>
          API token for the publisher <span className="muted">(a read-only token is enough; it is never shown again)</span>
        </span>
        <input type="password" value={token} onChange={(e) => setToken(e.target.value)} />
      </label>
      <div className="field-row">
        <label className="field">
          <span>Trust its certificate</span>
          <select value={trustKind} onChange={(e) => setTrustKind(e.target.value as SubscriptionTrust["kind"])}>
            <option value="pinned">by its pairing code (self-signed)</option>
            <option value="os">through this computer (a public CA)</option>
            <option value="privateCa">through a private CA</option>
          </select>
        </label>
        {trustKind === "pinned" && (
          <label className="field grow">
            <span>
              Pairing code <span className="muted">(shown in the publisher's Settings, ABCD-EFGH-…)</span>
            </span>
            <input required value={pin} onChange={(e) => setPin(e.target.value)} />
          </label>
        )}
      </div>
      {trustKind === "privateCa" && (
        <label className="field">
          <span>CA certificate (PEM)</span>
          <textarea required rows={4} value={caPem} onChange={(e) => setCaPem(e.target.value)} />
        </label>
      )}
      <div className="field-row">
        <label className="field grow">
          <span>
            Publisher's channel key <span className="muted">(from its Rules page; empty: pin the first feed's key)</span>
          </span>
          <input value={key} onChange={(e) => setKey(e.target.value)} />
        </label>
        <label className="field">
          <span>Check every (minutes)</span>
          <input type="number" min={5} max={10080} value={interval} onChange={(e) => setIntervalMinutes(Number(e.target.value) || 30)} />
        </label>
      </div>
      <div className="actions start">
        <button type="submit" className="primary" disabled={busy}>
          Subscribe
        </button>
        <button type="button" onClick={() => setOpen(false)}>
          Cancel
        </button>
      </div>
    </form>
  );
}

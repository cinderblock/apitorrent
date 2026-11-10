// The Rules page (/rules): rules over release candidates (what sources offer), what each
// rule took, its "latest" alias, and distribution channels.

import { useCallback, useEffect, useMemo, useState } from "react";

import type { AuthStatus } from "../api/generated/AuthStatus";
import type { RuleInfo } from "../api/generated/RuleInfo";
import type { StoredCandidate } from "../api/generated/StoredCandidate";
import { describeError } from "../auth/flow";
import { navigate } from "../router";
import { atLeast, useSpaceContext } from "../spaces/context";
import { rulesApi } from "./api";
import { ChannelsSection } from "./ChannelsSection";
import { RuleEditor } from "./RuleEditor";
import { ITEM_STATE_TEXT, describeRule, newRule } from "./ruleForm";
import { When } from "./When";
import "./rules.css";

type Editing = { id: string | null; rule: RuleInfo["rule"] } | null;

export function RulesPage({ status }: { status: AuthStatus }) {
  const [rules, setRules] = useState<RuleInfo[] | null>(null);
  const [candidates, setCandidates] = useState<StoredCandidate[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<Editing>(null);
  const [busy, setBusy] = useState(false);
  // What the caller may do in this space (the instance role, with an older daemon).
  const space = useSpaceContext();
  const mayWrite = space
    ? atLeast(space.current.role, "user")
    : status.authDisabled || (status.user !== null && status.user.role !== "viewer");
  const admin = space ? atLeast(space.current.role, "admin") : status.authDisabled || status.user?.role === "admin";

  const load = useCallback(async () => {
    try {
      const [r, c] = await Promise.all([rulesApi.rules(), rulesApi.candidates()]);
      setRules(r);
      setCandidates(c);
    } catch (e) {
      setError(describeError(e));
    }
  }, []);

  // Rules act in the background (torrents finish, aliases move), so keep the page fresh.
  useEffect(() => {
    void load();
    const timer = setInterval(() => void load(), 10_000);
    return () => clearInterval(timer);
  }, [load]);

  const sources = useMemo(() => {
    const counts = new Map<string, number>();
    for (const c of candidates) counts.set(c.source, (counts.get(c.source) ?? 0) + 1);
    return [...counts.entries()].sort(([a], [b]) => a.localeCompare(b));
  }, [candidates]);

  const act = async (f: () => Promise<unknown>) => {
    setBusy(true);
    setError(null);
    try {
      await f();
      await load();
    } catch (e) {
      setError(describeError(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="account rules-page">
      <header className="toolbar account-toolbar">
        <h1 className="brand">apitorrent</h1>
        <button type="button" className="link" onClick={() => navigate("/")}>
          ← Torrents
        </button>
        <span className="spacer" />
        {mayWrite && (
          <>
            <button type="button" disabled={busy} onClick={() => void act(() => rulesApi.run())}>
              Run rules now
            </button>
            <button type="button" className="primary" onClick={() => setEditing({ id: null, rule: newRule() })}>
              New rule…
            </button>
          </>
        )}
      </header>
      <section className="account-section">
        <h2>Rules</h2>
        <p className="muted">
          Sources (plugins such as RSS feeds and Ubuntu releases, and channels this server follows) offer releases.
          A rule picks some, orders them by version, and keeps the latest as torrents: with a label, a folder and
          the files you want. When a newer release arrives, the rule does what you chose with the older one. Rules
          run every minute and whenever a source offers something new.
        </p>
        <p className="muted">
          {sources.length === 0
            ? "No source has offered anything yet."
            : `Offered so far: ${sources.map(([s, n]) => `${n} from ${s}`).join(", ")}.`}
        </p>
      </section>
      {error && (
        <p className="error-text" role="alert">
          {error}
        </p>
      )}
      {editing && (
        <RuleEditor
          key={editing.id ?? "new"}
          id={editing.id}
          initial={editing.rule}
          sources={sources.map(([s]) => s)}
          onCancel={() => setEditing(null)}
          onSaved={() => {
            setEditing(null);
            void load();
          }}
        />
      )}
      {rules?.length === 0 && !editing && <p className="account-section muted">No rules yet.</p>}
      {rules?.map((r) => (
        <RuleCard
          key={r.id}
          rule={r}
          busy={busy}
          mayWrite={mayWrite}
          onEdit={() => setEditing({ id: r.id, rule: r.rule })}
          onToggle={() => void act(() => rulesApi.update(r.id, { ...r.rule, enabled: !r.rule.enabled }))}
          onDelete={() => void act(() => rulesApi.remove(r.id))}
        />
      ))}
      {admin && rules && <ChannelsSection rules={rules} />}
    </div>
  );
}

function RuleCard({
  rule: r,
  busy,
  mayWrite,
  onEdit,
  onToggle,
  onDelete,
}: {
  rule: RuleInfo;
  busy: boolean;
  mayWrite: boolean;
  onEdit: () => void;
  onToggle: () => void;
  onDelete: () => void;
}) {
  const [confirm, setConfirm] = useState(false);
  const [all, setAll] = useState(false);
  const items = all ? r.items : r.items.filter((i) => i.state === "active" || i.state === "failed");
  return (
    <section className="account-section rule-card">
      <div className="rule-head">
        <div>
          <strong>{r.rule.name}</strong> {!r.rule.enabled && <span className="badge badge-warn">off</span>}
          <p className="muted rule-summary">{describeRule(r.rule)}</p>
        </div>
        {mayWrite && (
          <div className="actions">
            <button type="button" disabled={busy} onClick={onEdit}>
              Change…
            </button>
            <button type="button" disabled={busy} onClick={onToggle}>
              {r.rule.enabled ? "Turn off" : "Turn on"}
            </button>
            {confirm ? (
              <>
                <button type="button" className="danger" disabled={busy} onClick={onDelete}>
                  Delete the rule
                </button>
                <button type="button" onClick={() => setConfirm(false)}>
                  Keep
                </button>
              </>
            ) : (
              <button type="button" className="link" onClick={() => setConfirm(true)}>
                Delete…
              </button>
            )}
          </div>
        )}
      </div>
      {confirm && <p className="muted">Its torrents and its alias file stay.</p>}
      {r.error && <p className="error-text">{r.error}</p>}
      {r.alias && (
        <p className="rule-alias">
          Latest: <code>{r.alias.path}</code>{" "}
          {r.alias.file ? (
            <>
              → {r.alias.file}{" "}
              <span className="muted">
                ({r.alias.method === "hardLink" ? "hard link" : "copy"}
                {r.alias.updatedAt !== null && (
                  <>
                    , <When ms={r.alias.updatedAt} />
                  </>
                )}
                )
              </span>
            </>
          ) : (
            <span className="muted">waiting for a completed release</span>
          )}
          {r.alias.error && <span className="error-text"> {r.alias.error}</span>}
        </p>
      )}
      {r.items.length > 0 && (
        <table className="list">
          <thead>
            <tr>
              <th>Release</th>
              <th>Version</th>
              <th>State</th>
              <th>Taken</th>
            </tr>
          </thead>
          <tbody>
            {items.map((i) => (
              <tr key={`${i.source}\n${i.candidateId}`} className={`item-${i.state}`}>
                <td>
                  {i.title}
                  <div className="muted rule-source">{i.source}</div>
                </td>
                <td>{i.version ?? ""}</td>
                <td>
                  {ITEM_STATE_TEXT[i.state]}
                  {i.error && <div className="error-text">{i.error}</div>}
                </td>
                <td>
                  <When ms={i.takenAt} />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {r.items.length > items.length || all ? (
        <button type="button" className="link" onClick={() => setAll(!all)}>
          {all ? "Show only what it keeps" : `Show all ${r.items.length}, superseded and removed too`}
        </button>
      ) : null}
    </section>
  );
}

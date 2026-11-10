// Invitation pieces shared by the Spaces page (a space's admins invite into it) and the
// Users page (server admins invite into any spaces, with any server role).

import { useState, type FormEvent } from "react";

import type { CreatedInvitation } from "../api/generated/CreatedInvitation";
import type { InvitationInfo } from "../api/generated/InvitationInfo";
import type { Role } from "../api/generated/Role";
import type { SpaceRole } from "../api/generated/SpaceRole";
import type { SpaceView } from "../api/generated/SpaceView";
import { formatDateTime, formatUtc } from "../format";
import { invitationsApi } from "./api";
import { LIFETIMES, ROLE_TEXT, SPACE_ROLE_TEXT, STATE_TEXT, describeGrants, timeLeft } from "./format";
import "./users.css";

/** Runs a change (asking for a fresh sign-in when the server wants one); true if it worked. */
export type Change = (action: () => Promise<unknown>) => Promise<boolean>;

const SPACE_ROLES: SpaceRole[] = ["viewer", "user", "admin"];

/**
 * A form for a new invitation. With `only`, it invites into that one space; otherwise the
 * caller (a server admin) picks spaces and the server role.
 */
export function InviteForm({
  spaces,
  only,
  change,
  onCreated,
  onCancel,
}: {
  spaces: SpaceView[];
  only?: SpaceView;
  change: Change;
  onCreated: (c: CreatedInvitation) => void;
  onCancel: () => void;
}) {
  const [role, setRole] = useState<Role>("user");
  const [picked, setPicked] = useState<Record<string, SpaceRole>>(() => (only ? { [only.slug]: "user" } : {}));
  const [days, setDays] = useState(7);
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);

  async function submit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    let made: CreatedInvitation | null = null;
    await change(async () => {
      made = await invitationsApi.create({
        role: only ? null : role,
        spaces: Object.entries(picked).map(([slug, r]) => ({ slug, role: r })),
        expiresInDays: days,
        note: note.trim() || null,
      });
    });
    setBusy(false);
    if (made) onCreated(made);
  }

  const toggle = (slug: string, on: boolean) =>
    setPicked((p) => {
      const next = { ...p };
      if (on) next[slug] = "user";
      else delete next[slug];
      return next;
    });

  return (
    <form className="invite-form" onSubmit={(e) => void submit(e)}>
      {only ? (
        <label className="field">
          <span>Their role in {only.name}</span>
          <select value={picked[only.slug]} onChange={(e) => setPicked({ [only.slug]: e.target.value as SpaceRole })}>
            {SPACE_ROLES.map((r) => (
              <option key={r} value={r}>
                {SPACE_ROLE_TEXT[r]}
              </option>
            ))}
          </select>
        </label>
      ) : (
        <>
          <label className="field">
            <span>Server role</span>
            <select value={role} onChange={(e) => setRole(e.target.value as Role)}>
              {(["user", "viewer", "admin"] as Role[]).map((r) => (
                <option key={r} value={r}>
                  {ROLE_TEXT[r]}
                </option>
              ))}
            </select>
          </label>
          <fieldset className="invite-spaces">
            <legend>Spaces</legend>
            {spaces.map((s) => (
              <div key={s.slug} className="invite-space">
                <label className="check grow">
                  <input type="checkbox" checked={s.slug in picked} onChange={(e) => toggle(s.slug, e.target.checked)} />
                  <span>{s.name}</span>
                </label>
                {s.slug in picked && (
                  <select
                    aria-label={`Role in ${s.name}`}
                    value={picked[s.slug]}
                    onChange={(e) => setPicked({ ...picked, [s.slug]: e.target.value as SpaceRole })}
                  >
                    {SPACE_ROLES.map((r) => (
                      <option key={r} value={r}>
                        {SPACE_ROLE_TEXT[r]}
                      </option>
                    ))}
                  </select>
                )}
              </div>
            ))}
            {role === "admin" && <p className="muted">A server admin uses every space without joining it.</p>}
          </fieldset>
        </>
      )}
      <div className="field-row">
        <label className="field">
          <span>The link works for</span>
          <select value={days} onChange={(e) => setDays(Number(e.target.value))}>
            {LIFETIMES.map((d) => (
              <option key={d} value={d}>
                {d === 1 ? "1 day" : `${d} days`}
              </option>
            ))}
          </select>
        </label>
        <label className="field grow">
          <span>Note (who it is for; only admins see it)</span>
          <input value={note} maxLength={200} onChange={(e) => setNote(e.target.value)} />
        </label>
      </div>
      <div className="actions">
        <button type="button" onClick={onCancel}>
          Cancel
        </button>
        <button type="submit" className="primary" disabled={busy || (!only && role !== "admin" && Object.keys(picked).length === 0)}>
          Create the link
        </button>
      </div>
    </form>
  );
}

/** A new link, shown once, with a copy button. */
export function NewLink({ created, onDone }: { created: CreatedInvitation; onDone: () => void }) {
  const [copied, setCopied] = useState(false);
  const recovery = created.invitation.kind === "recovery";
  async function copy() {
    try {
      await navigator.clipboard.writeText(created.link);
      setCopied(true);
    } catch {
      setCopied(false);
    }
  }
  return (
    <div className="new-link" role="status">
      <p>
        <strong>{recovery ? "Recovery link" : "Invitation link"}</strong>: send it to{" "}
        {recovery ? created.invitation.userName : "the person it is for"} privately. It works once, and this is the only
        time it is shown.
      </p>
      <div className="new-link-row">
        <input readOnly value={created.link} aria-label="The link" onFocus={(e) => e.target.select()} />
        <button type="button" onClick={() => void copy()}>
          {copied ? "Copied" : "Copy"}
        </button>
      </div>
      <div className="actions start">
        <button type="button" className="link" onClick={onDone}>
          Done
        </button>
      </div>
    </div>
  );
}

/** Invitations and recovery links, with a revoke button on those still waiting. */
export function InvitationList({
  list,
  change,
  showSpaces = true,
}: {
  list: InvitationInfo[];
  change: Change;
  showSpaces?: boolean;
}) {
  const now = Date.now();
  if (list.length === 0) return <p className="muted">No invitations.</p>;
  return (
    <ul className="invitation-list">
      {list.map((inv) => (
        <li key={inv.id} className={`invitation invitation-${inv.state}`}>
          <div className="grow">
            <div>
              <span className={`badge invitation-state-${inv.state}`}>{STATE_TEXT[inv.state]}</span>{" "}
              {inv.kind === "recovery" ? (
                <>
                  Recovery for <strong>{inv.userName}</strong>
                </>
              ) : (
                <>
                  {inv.role && inv.role !== "user" && <strong>{ROLE_TEXT[inv.role]}, </strong>}
                  {showSpaces ? describeGrants(inv) : SPACE_ROLE_TEXT[inv.spaces[0]?.role ?? "user"]}
                  {inv.userName && <> → {inv.userName}</>}
                </>
              )}
            </div>
            <small className="muted">
              {inv.note && <>{inv.note} · </>}
              by {inv.createdBy ?? "a deleted user"}
              {inv.state === "pending" && <> · expires {timeLeft(inv.expiresAt, now)}</>}
              {inv.state === "used" && inv.usedAt !== null && (
                <>
                  {" "}
                  · used <time title={formatUtc(inv.usedAt)}>{formatDateTime(inv.usedAt)}</time>
                </>
              )}
            </small>
          </div>
          {inv.state === "pending" && (
            <button type="button" onClick={() => void change(() => invitationsApi.revoke(inv.id))}>
              Revoke
            </button>
          )}
        </li>
      ))}
    </ul>
  );
}

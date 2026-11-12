// The Users page (/users), for server admins: everyone with an account, their server role,
// spaces and credentials; disabling, deleting and recovery links; and every invitation.

import { useCallback, useEffect, useState } from "react";

import type { AuthStatus } from "../api/generated/AuthStatus";
import type { CreatedInvitation } from "../api/generated/CreatedInvitation";
import type { InvitationInfo } from "../api/generated/InvitationInfo";
import type { ManagedUser } from "../api/generated/ManagedUser";
import type { Role } from "../api/generated/Role";
import type { SpaceView } from "../api/generated/SpaceView";
import { Cancelled, describeError, withReauth } from "../auth/flow";
import { ReauthDialog } from "../auth/ReauthDialog";
import { formatDateTime, formatUtc } from "../format";
import { navigate } from "../router";
import { spacesApi } from "../spaces/api";
import { useSpaceContext } from "../spaces/context";
import { invitationsApi, usersApi } from "./api";
import { ROLE_TEXT, SPACE_ROLE_TEXT } from "./format";
import { InvitationList, InviteForm, NewLink, type Change } from "./Invitations";
import "./users.css";

export function UsersPage({ status, onStatus }: { status: AuthStatus; onStatus: (s: AuthStatus) => void }) {
  const ctx = useSpaceContext();
  const [users, setUsers] = useState<ManagedUser[] | null>(null);
  const [invitations, setInvitations] = useState<InvitationInfo[]>([]);
  const [spaces, setSpaces] = useState<SpaceView[]>([]);
  const [inviting, setInviting] = useState(false);
  const [created, setCreated] = useState<CreatedInvitation | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [reauth, setReauth] = useState<{ resolve: () => void; reject: (e: unknown) => void } | null>(null);

  const load = useCallback(async () => {
    try {
      const [u, i, s] = await Promise.all([usersApi.list(), invitationsApi.list(), spacesApi.list()]);
      setUsers(u);
      setInvitations(i);
      setSpaces(s);
    } catch (e) {
      setError(describeError(e));
    }
  }, []);
  useEffect(() => void load(), [load]);

  const confirmIdentity = useCallback(
    () => new Promise<void>((resolve, reject) => setReauth({ resolve, reject })),
    [],
  );

  const change: Change = async (action) => {
    setBusy(true);
    setError(null);
    try {
      await withReauth(action, confirmIdentity);
      await load();
      return true;
    } catch (e) {
      if (!(e instanceof Cancelled)) setError(describeError(e));
      return false;
    } finally {
      setBusy(false);
    }
  };

  if (ctx && !ctx.serverAdmin) {
    return (
      <div className="account">
        <p className="account-section muted">Only a server admin manages users.</p>
      </div>
    );
  }

  return (
    <div className="account users-page">
      <header className="toolbar account-toolbar">
        <h1 className="brand">apitorrent</h1>
        <button type="button" className="link" onClick={() => navigate("/")}>
          ← Torrents
        </button>
        <span className="spacer" />
        <button
          type="button"
          className="primary"
          onClick={() => {
            setInviting(true);
            setCreated(null);
          }}
        >
          Invite someone…
        </button>
      </header>
      <section className="account-section">
        <h2>Users</h2>
        <p className="muted">
          New people join with an invitation: a link that works once and expires. It gives them the spaces and roles
          you choose; they pick a username and register a passkey. Someone who lost their passkey gets a recovery link
          instead. Disabling a user signs them out everywhere at once and stops their API tokens until you enable them
          again.
        </p>
        {status.authDisabled && <p className="banner-warn">Sign-in is disabled (development mode): there are no users.</p>}
      </section>
      {error && (
        <p className="error-text" role="alert">
          {error}
        </p>
      )}
      {(inviting || created) && (
        <section className="account-section">
          <h3>{created?.invitation.kind === "recovery" ? "Recovery link" : "New invitation"}</h3>
          {created ? (
            <NewLink
              created={created}
              onDone={() => {
                setCreated(null);
                setInviting(false);
              }}
            />
          ) : (
            <InviteForm spaces={spaces} change={change} onCreated={setCreated} onCancel={() => setInviting(false)} />
          )}
        </section>
      )}
      {users?.map((u) => (
        <UserCard key={u.id} user={u} busy={busy} change={change} onLink={setCreated} />
      ))}
      <section className="account-section">
        <h2>Invitations and recovery links</h2>
        <InvitationList list={invitations} change={change} />
      </section>
      {reauth && (
        <ReauthDialog
          status={status}
          reason="Inviting and managing users needs a fresh sign-in."
          onDone={(next) => {
            onStatus(next);
            reauth.resolve();
            setReauth(null);
          }}
          onCancel={() => {
            reauth.reject(new Cancelled());
            setReauth(null);
          }}
        />
      )}
    </div>
  );
}

function UserCard({
  user: u,
  busy,
  change,
  onLink,
}: {
  user: ManagedUser;
  busy: boolean;
  change: Change;
  onLink: (c: CreatedInvitation) => void;
}) {
  const [confirmDelete, setConfirmDelete] = useState(false);
  const credentials = [
    u.passkeys === 1 ? "1 passkey" : `${u.passkeys} passkeys`,
    u.hasPassword ? "a password" : null,
    u.tokens === 0 ? null : u.tokens === 1 ? "1 API token" : `${u.tokens} API tokens`,
  ].filter(Boolean);
  return (
    <section className="account-section user-card">
      <div className="user-head">
        <div className="grow">
          <strong>{u.displayName}</strong> <span className="muted">({u.name})</span>
          {u.role === "admin" && <span className="badge">server admin</span>}
          {u.role === "viewer" && <span className="badge">viewer</span>}
          {u.disabled && <span className="badge badge-danger">disabled</span>}
          {u.you && <span className="badge">you</span>}
        </div>
      </div>
      <p className="muted user-facts">
        {credentials.join(", ") || "no credentials"}
        {" · "}
        {u.lastSeenAt !== null ? (
          <>
            last seen <time>{formatDateTime(u.lastSeenAt)}</time> <small className="muted">{formatUtc(u.lastSeenAt)}</small>
          </>
        ) : (
          "not signed in"
        )}
      </p>
      {u.spaces.length > 0 ? (
        <ul className="user-spaces">
          {u.spaces.map((s) => (
            <li key={s.space}>
              {s.name} <span className="muted">({SPACE_ROLE_TEXT[s.role].toLowerCase()})</span>
            </li>
          ))}
        </ul>
      ) : (
        <p className="muted user-facts">{u.role === "admin" ? "Every space, as a server admin." : "In no space yet."}</p>
      )}
      {!u.you && (
        <div className="actions start">
          <label className="inline-field">
            <span>Server role</span>
            <select
              aria-label={`${u.name}'s server role`}
              value={u.role}
              disabled={busy}
              onChange={(e) => void change(() => usersApi.update(u.id, { role: e.target.value as Role, disabled: null }))}
            >
              {(["user", "viewer", "admin"] as Role[]).map((r) => (
                <option key={r} value={r}>
                  {ROLE_TEXT[r]}
                </option>
              ))}
            </select>
          </label>
          <button
            type="button"
            disabled={busy}
            onClick={() => void change(() => usersApi.update(u.id, { role: null, disabled: !u.disabled }))}
          >
            {u.disabled ? "Enable" : "Disable"}
          </button>
          <button
            type="button"
            disabled={busy}
            onClick={() =>
              void change(async () => {
                onLink(await usersApi.recovery(u.id, null));
              })
            }
          >
            Recovery link…
          </button>
          {confirmDelete ? (
            <>
              <span className="muted">Delete {u.name}, with their passkeys, tokens and memberships?</span>
              <button type="button" className="danger" disabled={busy} onClick={() => void change(() => usersApi.remove(u.id))}>
                Delete
              </button>
              <button type="button" onClick={() => setConfirmDelete(false)}>
                Keep
              </button>
            </>
          ) : (
            <button type="button" className="link" onClick={() => setConfirmDelete(true)}>
              Delete…
            </button>
          )}
        </div>
      )}
    </section>
  );
}

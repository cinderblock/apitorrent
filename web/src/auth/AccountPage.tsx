import { useCallback, useEffect, useRef, useState, type FormEvent, type ReactNode } from "react";

import { authApi } from "../api/client";
import type { ApiTokenInfo } from "../api/generated/ApiTokenInfo";
import type { AuthStatus } from "../api/generated/AuthStatus";
import type { CreatedToken } from "../api/generated/CreatedToken";
import type { PasskeyInfo } from "../api/generated/PasskeyInfo";
import type { SessionInfo } from "../api/generated/SessionInfo";
import type { TokenScope } from "../api/generated/TokenScope";
import { formatDateTime, formatUtc } from "../format";
import { navigate } from "../router";
import { Cancelled, describeError, describeUserAgent, withReauth } from "./flow";
import { InsecureBanner } from "./LoginPage";
import { ReauthDialog } from "./ReauthDialog";
import { createPasskey, passkeysSupported } from "./webauthn";

interface Props {
  status: AuthStatus;
  onStatus: (status: AuthStatus) => void;
  refresh: () => Promise<void>;
}

function When({ ms, empty = "–" }: { ms: number | null; empty?: string }) {
  if (ms === null) return <>{empty}</>;
  return <time title={formatUtc(ms)}>{formatDateTime(ms)}</time>;
}

const METHOD_LABEL = {
  passkey: "passkey",
  password: "password",
  passkeyAndPassword: "passkey and password",
} as const;

export function AccountPage({ status, onStatus, refresh }: Props) {
  const user = status.user!;
  const [passkeys, setPasskeys] = useState<PasskeyInfo[]>([]);
  const [tokens, setTokens] = useState<ApiTokenInfo[]>([]);
  const [sessions, setSessions] = useState<SessionInfo[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [reauth, setReauth] = useState<{ resolve: () => void; reject: (e: unknown) => void } | null>(null);
  const [created, setCreated] = useState<CreatedToken | null>(null);

  const load = useCallback(async () => {
    if (status.authDisabled) return;
    try {
      const [p, t, s] = await Promise.all([authApi.passkeys(), authApi.tokens(), authApi.sessions()]);
      setPasskeys(p);
      setTokens(t);
      setSessions(s);
    } catch (e) {
      setError(describeError(e));
    }
  }, [status.authDisabled]);

  useEffect(() => {
    void load();
  }, [load]);

  /** Opens the "confirm it's you" dialog; resolves once the session is fresh again. */
  const confirmIdentity = useCallback(
    () => new Promise<void>((resolve, reject) => setReauth({ resolve, reject })),
    [],
  );

  /** Run a change, re-authenticating first if the server asks, then reload the lists. */
  async function change(action: () => Promise<unknown>) {
    setBusy(true);
    setError(null);
    try {
      await withReauth(action, confirmIdentity);
      await load();
    } catch (e) {
      if (!(e instanceof Cancelled)) setError(describeError(e));
    } finally {
      setBusy(false);
    }
  }

  async function signOut() {
    try {
      await authApi.logout();
    } finally {
      navigate("/", { replace: true });
      await refresh();
    }
  }

  if (status.authDisabled) {
    return (
      <div className="account">
        <AccountHeader />
        <p className="banner-warn">
          Authentication is disabled (<code>--insecure-no-auth-loopback</code>). There are no accounts to manage.
        </p>
      </div>
    );
  }

  const session = status.session;
  return (
    <div className="account">
      <AccountHeader>
        <button type="button" onClick={() => void signOut()}>
          Sign out
        </button>
      </AccountHeader>
      <InsecureBanner status={status} />
      <section className="account-section">
        <h2>{user.displayName}</h2>
        <p className="muted">
          Signed in as <strong>{user.name}</strong> ({user.role})
          {session && (
            <>
              {" "}
              with a {METHOD_LABEL[session.method]}. This session ends by <When ms={session.expiresAt} />.
            </>
          )}
        </p>
      </section>
      {error && (
        <p className="error-text" role="alert">
          {error}
        </p>
      )}

      <PasskeysSection status={status} passkeys={passkeys} busy={busy} change={change} />
      {status.passwordMode !== "off" && <PasswordSection status={status} busy={busy} change={change} />}
      <TokensSection
        tokens={tokens}
        busy={busy}
        change={change}
        onCreated={setCreated}
      />
      <SessionsSection sessions={sessions} busy={busy} change={change} />

      {created && <ShowToken created={created} onClose={() => setCreated(null)} />}
      {reauth && (
        <ReauthDialog
          status={status}
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

function AccountHeader({ children }: { children?: ReactNode }) {
  return (
    <header className="toolbar account-toolbar">
      <h1 className="brand">apitorrent</h1>
      <button type="button" className="link" onClick={() => navigate("/")}>
        ← Torrents
      </button>
      <span className="spacer" />
      {children}
    </header>
  );
}

type Change = (action: () => Promise<unknown>) => Promise<void>;

function PasskeysSection({
  status,
  passkeys,
  busy,
  change,
}: {
  status: AuthStatus;
  passkeys: PasskeyInfo[];
  busy: boolean;
  change: Change;
}) {
  const [name, setName] = useState("");
  const canAdd = status.passkeysAvailable && passkeysSupported();
  const [renaming, setRenaming] = useState<{ id: string; name: string } | null>(null);

  function add(e: FormEvent) {
    e.preventDefault();
    void change(async () => {
      const start = await authApi.passkeyStart();
      const credential = await createPasskey(start.options);
      await authApi.passkeyFinish({ ceremonyId: start.ceremonyId, credential, name: name || null });
      setName("");
    });
  }

  return (
    <section className="account-section">
      <h3>Passkeys</h3>
      <table className="list">
        <thead>
          <tr>
            <th>Name</th>
            <th>Works on</th>
            <th>Added</th>
            <th>Last used</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {passkeys.map((p) => (
            <tr key={p.id}>
              <td>
                {renaming?.id === p.id ? (
                  <form
                    className="inline-form"
                    onSubmit={(e) => {
                      e.preventDefault();
                      const next = renaming.name;
                      setRenaming(null);
                      void change(() => authApi.renamePasskey(p.id, next));
                    }}
                  >
                    <input
                      value={renaming.name}
                      onChange={(e) => setRenaming({ id: p.id, name: e.target.value })}
                      aria-label="Passkey name"
                      autoFocus
                    />
                    <button type="submit">Save</button>
                    <button type="button" onClick={() => setRenaming(null)}>
                      Cancel
                    </button>
                  </form>
                ) : (
                  p.name
                )}
              </td>
              <td>
                <code>{p.rpId}</code>
              </td>
              <td>
                <When ms={p.createdAt} />
              </td>
              <td>
                <When ms={p.lastUsedAt} empty="never" />
              </td>
              <td className="end">
                <button type="button" className="link" onClick={() => setRenaming({ id: p.id, name: p.name })}>
                  Rename
                </button>
                <button
                  type="button"
                  className="link danger-text"
                  disabled={busy}
                  onClick={() => void change(() => authApi.removePasskey(p.id))}
                >
                  Remove
                </button>
              </td>
            </tr>
          ))}
          {passkeys.length === 0 && (
            <tr>
              <td colSpan={5} className="muted">
                No passkeys.
              </td>
            </tr>
          )}
        </tbody>
      </table>
      {canAdd ? (
        <form className="inline-form" onSubmit={add}>
          <input
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="Name (phone, security key…)"
            aria-label="New passkey name"
          />
          <button type="submit" className="primary" disabled={busy}>
            Add a passkey
          </button>
        </form>
      ) : (
        <p className="muted">
          Passkeys can't be added at this address.
          {status.passkeyHint && <> Open {status.passkeyHint} to add one.</>}
        </p>
      )}
    </section>
  );
}

function PasswordSection({ status, busy, change }: { status: AuthStatus; busy: boolean; change: Change }) {
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const has = status.user?.hasPassword === true;
  const mismatch = confirm !== "" && confirm !== password;
  return (
    <section className="account-section">
      <h3>Password</h3>
      <p className="muted">
        {status.passwordMode === "fallback"
          ? "A password can sign you in where passkeys don't work."
          : "This instance asks for your password after your passkey, if you set one."}{" "}
        {has ? "You have one set." : "You haven't set one."}
      </p>
      <form
        className="inline-form"
        onSubmit={(e) => {
          e.preventDefault();
          void change(async () => {
            await authApi.setPassword(password);
            setPassword("");
            setConfirm("");
          });
        }}
      >
        <input
          type="password"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          autoComplete="new-password"
          placeholder="New password (12+ characters)"
          aria-label="New password"
          minLength={12}
        />
        <input
          type="password"
          value={confirm}
          onChange={(e) => setConfirm(e.target.value)}
          autoComplete="new-password"
          placeholder="Again"
          aria-label="Confirm new password"
        />
        <button type="submit" disabled={busy || password.length < 12 || password !== confirm}>
          {has ? "Change password" : "Set password"}
        </button>
        {has && (
          <button type="button" className="link danger-text" disabled={busy} onClick={() => void change(() => authApi.removePassword())}>
            Remove password
          </button>
        )}
      </form>
      {mismatch && <p className="error-text">The passwords don't match.</p>}
    </section>
  );
}

const EXPIRY_CHOICES: { label: string; days: number | null }[] = [
  { label: "Never", days: null },
  { label: "30 days", days: 30 },
  { label: "90 days", days: 90 },
  { label: "1 year", days: 365 },
];

function TokensSection({
  tokens,
  busy,
  change,
  onCreated,
}: {
  tokens: ApiTokenInfo[];
  busy: boolean;
  change: Change;
  onCreated: (t: CreatedToken) => void;
}) {
  const [name, setName] = useState("");
  const [scope, setScope] = useState<TokenScope>("read");
  const [expiry, setExpiry] = useState(0);
  return (
    <section className="account-section">
      <h3>API tokens</h3>
      <p className="muted">
        For scripts and other apps. Send one as <code>Authorization: Bearer &lt;token&gt;</code>. Tokens can't
        manage passkeys, passwords or other tokens.
      </p>
      <table className="list">
        <thead>
          <tr>
            <th>Name</th>
            <th>Access</th>
            <th>Token</th>
            <th>Created</th>
            <th>Expires</th>
            <th>Last used</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {tokens.map((t) => (
            <tr key={t.id}>
              <td>{t.name}</td>
              <td>{t.scope === "full" ? "Full" : "Read only"}</td>
              <td>
                <code>{t.prefix}…</code>
              </td>
              <td>
                <When ms={t.createdAt} />
              </td>
              <td>
                <When ms={t.expiresAt} empty="never" />
              </td>
              <td>
                <When ms={t.lastUsedAt} empty="never" />
              </td>
              <td className="end">
                <button
                  type="button"
                  className="link danger-text"
                  disabled={busy}
                  onClick={() => void change(() => authApi.revokeToken(t.id))}
                >
                  Revoke
                </button>
              </td>
            </tr>
          ))}
          {tokens.length === 0 && (
            <tr>
              <td colSpan={7} className="muted">
                No tokens.
              </td>
            </tr>
          )}
        </tbody>
      </table>
      <form
        className="inline-form"
        onSubmit={(e) => {
          e.preventDefault();
          void change(async () => {
            const t = await authApi.createToken({
              name,
              scope,
              expiresInDays: EXPIRY_CHOICES[expiry]?.days ?? null,
            });
            setName("");
            onCreated(t);
          });
        }}
      >
        <input
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="Name (what uses it)"
          aria-label="Token name"
          required
        />
        <select value={scope} onChange={(e) => setScope(e.target.value as TokenScope)} aria-label="Access">
          <option value="read">Read only</option>
          <option value="full">Full access</option>
        </select>
        <select value={expiry} onChange={(e) => setExpiry(Number(e.target.value))} aria-label="Expires">
          {EXPIRY_CHOICES.map((c, i) => (
            <option key={c.label} value={i}>
              Expires: {c.label}
            </option>
          ))}
        </select>
        <button type="submit" className="primary" disabled={busy || !name.trim()}>
          Create token
        </button>
      </form>
    </section>
  );
}

function SessionsSection({ sessions, busy, change }: { sessions: SessionInfo[]; busy: boolean; change: Change }) {
  return (
    <section className="account-section">
      <h3>Signed-in devices</h3>
      <table className="list">
        <thead>
          <tr>
            <th>Device</th>
            <th>Address</th>
            <th>Signed in</th>
            <th>Last active</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {sessions.map((s) => (
            <tr key={s.id}>
              <td title={s.userAgent ?? undefined}>
                {describeUserAgent(s.userAgent)} {s.current && <span className="badge">this device</span>}
              </td>
              <td>{s.ip ?? "–"}</td>
              <td>
                <When ms={s.createdAt} />
              </td>
              <td>
                <When ms={s.lastSeenAt} />
              </td>
              <td className="end">
                {!s.current && (
                  <button
                    type="button"
                    className="link danger-text"
                    disabled={busy}
                    onClick={() => void change(() => authApi.revokeSession(s.id))}
                  >
                    Sign out
                  </button>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </section>
  );
}

/** The new token, shown once. */
function ShowToken({ created, onClose }: { created: CreatedToken; onClose: () => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    dialog.current?.showModal();
  }, []);
  return (
    <dialog ref={dialog} className="dialog" onClose={onClose} aria-labelledby="token-title">
      <h2 id="token-title">Token “{created.info.name}” created</h2>
      <p>Copy it now. It won't be shown again.</p>
      <p className="token-box">
        <code>{created.token}</code>
      </p>
      <div className="actions">
        <button
          type="button"
          onClick={() => {
            void navigator.clipboard?.writeText(created.token).then(() => setCopied(true));
          }}
        >
          {copied ? "Copied" : "Copy"}
        </button>
        <button type="button" className="primary" onClick={onClose}>
          Done
        </button>
      </div>
    </dialog>
  );
}

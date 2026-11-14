// The page an invitation or recovery link opens (`/enroll#token=…`). An invitation lets the
// person pick a username and register a passkey (or a password, when the daemon allows
// passwords); a recovery link registers new credentials for an existing account in place of
// the lost ones. Someone already signed in may instead take an invitation into their own
// account ("Join as …"), which adds its spaces to it.

import { useEffect, useState, type FormEvent } from "react";

import { ApiRequestError } from "../api/client";
import type { AuthStatus } from "../api/generated/AuthStatus";
import type { EnrollInfo } from "../api/generated/EnrollInfo";
import { navigate } from "../router";
import { spaceHome } from "../spaces/prefix";
import { enrollApi } from "../users/api";
import { ROLE_TEXT, SPACE_ROLE_TEXT, enrollLinkAt, validUsername } from "../users/format";
import { formatDateTime, formatUtc } from "../format";
import { describeError, readSetupToken } from "./flow";
import { AuthHeader } from "./AuthHeader";
import { InsecureBanner } from "./LoginPage";
import { createPasskey, passkeysSupported } from "./webauthn";

interface Props {
  status: AuthStatus;
  onSignedIn: (status: AuthStatus) => void;
}

export function EnrollPage({ status, onSignedIn }: Props) {
  // The secret arrives in the fragment (it never reaches a server log); it is taken out of
  // the address bar and history at once.
  const [token] = useState(() => readSetupToken(location.hash));
  const [info, setInfo] = useState<EnrollInfo | null>(null);
  const [linkError, setLinkError] = useState<string | null>(null);
  const [username, setUsername] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [passkeyName, setPasskeyName] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (location.hash) history.replaceState(null, "", location.pathname + location.search);
    if (!token) return;
    enrollApi.info(token).then(setInfo, (e) => {
      setLinkError(
        e instanceof ApiRequestError && e.code === "invalid_link"
          ? "This link doesn't work: it was already used, was revoked, or has expired. Ask whoever sent it for a new one."
          : describeError(e),
      );
    });
  }, [token]);

  // Browser sign-in links, from the desktop app, are for an existing account too.
  const browser = info?.kind === "browser";
  const recovery = info?.kind === "recovery" || browser;
  const canPasskey = status.passkeysAvailable && passkeysSupported();
  // A browser sign-in link takes a passkey and nothing else: it is how this browser
  // signs in from now on.
  const canPassword = info?.passwordMode === "fallback" && !browser;
  const nameOk = recovery || validUsername(username);

  async function run(action: () => Promise<AuthStatus>) {
    setBusy(true);
    setError(null);
    try {
      const next = await action();
      navigate("/", { replace: true });
      onSignedIn(next);
    } catch (e) {
      setError(describeError(e));
    } finally {
      setBusy(false);
    }
  }

  function withPasskey(e: FormEvent) {
    e.preventDefault();
    if (!token) return;
    void run(async () => {
      const start = await enrollApi.start({ token, username, displayName: displayName || null });
      const credential = await createPasskey(start.options);
      return enrollApi.finish(start.ceremonyId, credential, passkeyName || null);
    });
  }

  function withPassword() {
    if (!token) return;
    void run(() => enrollApi.password({ token, username, displayName: displayName || null, password }));
  }

  async function join() {
    if (!token) return;
    setBusy(true);
    setError(null);
    try {
      const joined = await enrollApi.accept(token);
      // A full load, so the space switcher and roles start afresh in the new space.
      const first = joined.spaces[0];
      location.assign(first ? spaceHome(first.slug) : "/");
    } catch (e) {
      setError(describeError(e));
      setBusy(false);
    }
  }

  const joinAs = status.user && info?.joinable ? status.user : null;

  let body;
  if (!token) {
    body = (
      <p>
        This page needs the link from your invitation. Open the whole link you were sent; the part after{" "}
        <code>#</code> is the invitation itself.
      </p>
    );
  } else if (linkError) {
    body = (
      <p className="auth-error" role="alert">
        {linkError}
      </p>
    );
  } else if (!info) {
    body = <p className="muted">Checking the link…</p>;
  } else {
    body = (
      <>
        <Summary info={info} />
        {joinAs && (
          <fieldset className="auth-alt">
            <legend>With your account</legend>
            <p>
              Add these spaces to <strong>{joinAs.displayName}</strong> ({joinAs.name}), the account you&apos;re signed in with. A role you already
              have that is higher stays.
            </p>
            <div className="actions">
              <button type="button" className="primary" disabled={busy} onClick={() => void join()}>
                Join as {joinAs.displayName}
              </button>
            </div>
          </fieldset>
        )}
        {status.user && (
          <p className="banner-warn">
            {joinAs ? "Or create a new account below. " : ""}You're signed in as {status.user.displayName} ({status.user.name}).{" "}
            {joinAs ? "Creating a new account" : "Continuing"} signs you out of that account here.
          </p>
        )}
        {status.user && !recovery && info.kind === "invitation" && !info.joinable && (
          <p className="muted">This invitation makes a server admin, so it can only create a new account.</p>
        )}
        {!recovery && (
          <div className="field-row">
            <label className="field grow">
              <span>Username</span>
              <input
                value={username}
                onChange={(e) => setUsername(e.target.value)}
                autoComplete="username"
                autoCapitalize="none"
                spellCheck={false}
                required
                pattern="[A-Za-z0-9][A-Za-z0-9._\-]{0,63}"
                aria-describedby="enroll-username-hint"
              />
              <small id="enroll-username-hint" className="field-hint">
                Letters, digits, ., _ or -
              </small>
            </label>
            <label className="field grow">
              <span>
                Display name <span className="optional">optional</span>
              </span>
              <input value={displayName} onChange={(e) => setDisplayName(e.target.value)} autoComplete="name" />
            </label>
          </div>
        )}
        {canPasskey ? (
          <>
            <label className="field">
              <span>
                Passkey name <span className="optional">optional</span>
              </span>
              <input
                value={passkeyName}
                onChange={(e) => setPasskeyName(e.target.value)}
                placeholder="Laptop, phone, security key…"
              />
            </label>
            <button type="submit" className={joinAs ? "auth-submit" : "primary auth-submit"} disabled={busy || !nameOk}>
              {browser ? "Create a passkey and sign in" : recovery ? "Register a new passkey" : "Create my account with a passkey"}
            </button>
          </>
        ) : (
          <EnrollPasskeyHint status={status} token={token} />
        )}
        {canPassword && (
          <fieldset className="auth-alt">
            <legend>{canPasskey ? "or use a password" : "Use a password"}</legend>
            <label className="field">
              <span>Password</span>
              <input
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                autoComplete="new-password"
                minLength={12}
                aria-describedby="enroll-password-hint"
              />
              <small id="enroll-password-hint" className="field-hint">
                At least 12 characters
              </small>
            </label>
            <button
              type="button"
              className={canPasskey || joinAs ? "auth-submit" : "primary auth-submit"}
              disabled={busy || !nameOk || password.length < 12}
              onClick={withPassword}
            >
              {recovery ? "Set a new password" : "Create my account with a password"}
            </button>
          </fieldset>
        )}
        {error && (
          <p className="auth-error" role="alert">
            {error}
          </p>
        )}
      </>
    );
  }

  return (
    <div className="auth-page">
      <form className="auth-card" onSubmit={withPasskey}>
        <AuthHeader title={browser ? "Sign in on this browser" : recovery ? "Recover your account" : "Join apitorrent"} />
        <InsecureBanner status={status} />
        {body}
        {status.user && (
          <button type="button" className="link auth-secondary" onClick={() => navigate("/", { replace: true })}>
            Back to apitorrent
          </button>
        )}
      </form>
    </div>
  );
}

function Summary({ info }: { info: EnrollInfo }) {
  const expires = (
    <>
      <time>{formatDateTime(info.expiresAt)}</time>{" "}
      <small className="muted">{formatUtc(info.expiresAt)}</small>
    </>
  );
  if (info.kind === "browser") {
    return (
      <p>
        Your app sent you here to sign in as <strong>{info.displayName ?? info.username}</strong> ({info.username}). First create a passkey for this
        browser: it is how you sign in here from now on. Nothing else about your account changes. This link works once, until {expires}.
      </p>
    );
  }
  if (info.kind === "recovery") {
    return (
      <p>
        This link registers new sign-in credentials for <strong>{info.displayName ?? info.username}</strong> (
        {info.username}). Your old passkeys and password stop working, and every device signed in to the account is
        signed out. It works once, until {expires}.
      </p>
    );
  }
  return (
    <>
      <p>
        {info.invitedBy ? <strong>{info.invitedBy}</strong> : "Someone"} invited you
        {info.role === "admin" ? " as a server admin" : info.role === "viewer" ? ` (${ROLE_TEXT.viewer.toLowerCase()})` : ""}
        . The invitation works once, until {expires}.
      </p>
      {info.spaces.length > 0 && (
        <ul className="enroll-spaces">
          {info.spaces.map((s) => (
            <li key={s.name}>
              {s.name}: {SPACE_ROLE_TEXT[s.role].toLowerCase()}
            </li>
          ))}
        </ul>
      )}
    </>
  );
}

/** Passkeys need a secure context and a relying party for this host; say where they work. */
function EnrollPasskeyHint({ status, token }: { status: AuthStatus; token: string }) {
  if (!passkeysSupported()) {
    return <p className="auth-note">This browser doesn't support passkeys.</p>;
  }
  return (
    <p className="auth-note">
      Passkeys don't work at this address.
      {status.passkeyHint && (
        <>
          {" "}
          Open <a href={enrollLinkAt(status.passkeyHint, token)}>this invitation at {status.passkeyHint}</a> instead.
        </>
      )}
    </p>
  );
}

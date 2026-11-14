import { useState, type FormEvent } from "react";

import { authApi } from "../api/client";
import type { AuthStatus } from "../api/generated/AuthStatus";
import { AuthHeader } from "./AuthHeader";
import { describeError } from "./flow";
import { passkeysSupported, signWithPasskey } from "./webauthn";

interface Props {
  status: AuthStatus;
  onSignedIn: (status: AuthStatus) => void;
}

/** Plain HTTP across the network: say so, every time. */
export function InsecureBanner({ status }: { status: AuthStatus }) {
  if (!status.insecureTransport) return null;
  return (
    <p className="banner-warn" role="alert">
      This connection isn't encrypted. Anyone on the network can read what you send, including your
      password. Put apitorrent behind a reverse proxy with HTTPS.
    </p>
  );
}

/** Where passkeys work, when they don't work here. */
export function PasskeyHint({ status }: { status: AuthStatus }) {
  if (status.passkeysAvailable && passkeysSupported()) return null;
  if (!passkeysSupported()) {
    return <p className="auth-note">This browser doesn't support passkeys.</p>;
  }
  return (
    <p className="auth-note">
      Passkeys don't work at this address.
      {status.passkeyHint && (
        <>
          {" "}
          Open <a href={status.passkeyHint}>{status.passkeyHint}</a> instead.
        </>
      )}
    </p>
  );
}

export function LoginPage({ status, onSignedIn }: Props) {
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const secondFactor = status.secondFactorRequired;
  const canPasskey = status.passkeysAvailable && passkeysSupported();

  async function run(action: () => Promise<AuthStatus>) {
    setBusy(true);
    setError(null);
    try {
      onSignedIn(await action());
    } catch (e) {
      setError(describeError(e));
    } finally {
      setBusy(false);
    }
  }

  function withPasskey() {
    void run(async () => {
      const start = await authApi.loginStart();
      const credential = await signWithPasskey(start.options);
      return authApi.loginFinish(start.ceremonyId, credential);
    });
  }

  function withPassword(e: FormEvent) {
    e.preventDefault();
    void run(() => authApi.loginPassword(secondFactor ? null : username, password));
  }

  function cancelSecondFactor() {
    void run(async () => {
      await authApi.logout();
      return authApi.status();
    });
  }

  if (secondFactor) {
    return (
      <div className="auth-page">
        <form className="auth-card" onSubmit={withPassword}>
          <AuthHeader title="Enter your password" lead="Your passkey worked. This server also asks for your password." />
          <InsecureBanner status={status} />
          <label className="field">
            <span>Password</span>
            <input
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              autoComplete="current-password"
              autoFocus
              required
            />
          </label>
          <button type="submit" className="primary auth-submit" disabled={busy || !password}>
            Sign in
          </button>
          <button type="button" className="link auth-secondary" onClick={cancelSecondFactor} disabled={busy}>
            Cancel and sign out
          </button>
          {error && (
            <p className="auth-error" role="alert">
              {error}
            </p>
          )}
        </form>
      </div>
    );
  }

  return (
    <div className="auth-page">
      <section className="auth-card">
        <AuthHeader title="Sign in" />
        <InsecureBanner status={status} />
        {canPasskey ? (
          <button type="button" className="primary auth-submit" onClick={withPasskey} disabled={busy}>
            {busy ? "Waiting for your passkey…" : "Sign in with a passkey"}
          </button>
        ) : (
          <PasskeyHint status={status} />
        )}
        {status.passwordMode === "fallback" && (
          <form onSubmit={withPassword}>
            <fieldset className="auth-alt">
              <legend>{canPasskey ? "or use your password" : "Sign in with your password"}</legend>
              <label className="field">
                <span>Username</span>
                <input
                  value={username}
                  onChange={(e) => setUsername(e.target.value)}
                  autoComplete="username"
                  autoCapitalize="none"
                  spellCheck={false}
                  required
                />
              </label>
              <label className="field">
                <span>Password</span>
                <input
                  type="password"
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                  autoComplete="current-password"
                  required
                />
              </label>
              <button
                type="submit"
                className={canPasskey ? "auth-submit" : "primary auth-submit"}
                disabled={busy || !username || !password}
              >
                Sign in with password
              </button>
            </fieldset>
          </form>
        )}
        {error && (
          <p className="auth-error" role="alert">
            {error}
          </p>
        )}
      </section>
    </div>
  );
}

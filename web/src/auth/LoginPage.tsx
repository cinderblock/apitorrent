import { useState, type FormEvent } from "react";

import { authApi } from "../api/client";
import type { AuthStatus } from "../api/generated/AuthStatus";
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
    return <p className="muted">This browser doesn't support passkeys.</p>;
  }
  return (
    <p className="muted">
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
          <h1 className="brand">apitorrent</h1>
          <h2>Enter your password</h2>
          <InsecureBanner status={status} />
          <p className="muted">Your passkey worked. This instance also asks for your password.</p>
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
          {error && (
            <p className="error-text" role="alert">
              {error}
            </p>
          )}
          <div className="actions">
            <button type="button" onClick={cancelSecondFactor} disabled={busy}>
              Cancel
            </button>
            <button type="submit" className="primary" disabled={busy || !password}>
              Sign in
            </button>
          </div>
        </form>
      </div>
    );
  }

  return (
    <div className="auth-page">
      <section className="auth-card">
        <h1 className="brand">apitorrent</h1>
        <h2>Sign in</h2>
        <InsecureBanner status={status} />
        {canPasskey ? (
          <div className="actions start">
            <button type="button" className="primary wide" onClick={withPasskey} disabled={busy}>
              Sign in with a passkey
            </button>
          </div>
        ) : (
          <PasskeyHint status={status} />
        )}
        {status.passwordMode === "fallback" && (
          <form className="auth-alt" onSubmit={withPassword}>
            <p className="muted">{canPasskey ? "Or with your password:" : "Sign in with your password:"}</p>
            <label className="field">
              <span>Username</span>
              <input
                value={username}
                onChange={(e) => setUsername(e.target.value)}
                autoComplete="username"
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
            <div className="actions">
              <button type="submit" disabled={busy || !username || !password}>
                Sign in with password
              </button>
            </div>
          </form>
        )}
        {error && (
          <p className="error-text" role="alert">
            {error}
          </p>
        )}
      </section>
    </div>
  );
}

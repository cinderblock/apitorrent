import { useEffect, useState, type FormEvent } from "react";

import { authApi } from "../api/client";
import type { AuthStatus } from "../api/generated/AuthStatus";
import { describeError, readSetupToken } from "./flow";
import { InsecureBanner, PasskeyHint } from "./LoginPage";
import { createPasskey, passkeysSupported } from "./webauthn";

interface Props {
  status: AuthStatus;
  onSignedIn: (status: AuthStatus) => void;
}

/** First run: the setup token from the daemon's console creates the first admin. */
export function SetupPage({ status, onSignedIn }: Props) {
  const [token, setToken] = useState("");
  const [username, setUsername] = useState("admin");
  const [displayName, setDisplayName] = useState("");
  const [passkeyName, setPasskeyName] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Take the token from a `/setup#token=…` link, then drop it from the address bar and
  // history so it isn't left lying around.
  useEffect(() => {
    const fromLink = readSetupToken(location.hash);
    if (fromLink) {
      setToken(fromLink);
      history.replaceState(null, "", location.pathname + location.search);
    }
  }, []);

  const canPasskey = status.passkeysAvailable && passkeysSupported();
  const canPassword = status.passwordMode === "fallback";

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

  function withPasskey(e: FormEvent) {
    e.preventDefault();
    void run(async () => {
      const start = await authApi.setupStart({ token, username, displayName: displayName || null });
      const credential = await createPasskey(start.options);
      return authApi.setupFinish(start.ceremonyId, credential, passkeyName || null);
    });
  }

  function withPassword() {
    void run(() => authApi.setupPassword({ token, username, displayName: displayName || null, password }));
  }

  return (
    <div className="auth-page">
      <form className="auth-card" onSubmit={withPasskey}>
        <h1 className="brand">apitorrent</h1>
        <h2>Create the first admin</h2>
        <InsecureBanner status={status} />
        <p className="muted">
          The daemon printed a one-time setup token on its console when it started (and a new one each hour
          until it's used).
        </p>
        <label className="field">
          <span>Setup token</span>
          <input
            value={token}
            onChange={(e) => setToken(e.target.value)}
            autoComplete="off"
            spellCheck={false}
            required
            placeholder="xxxx-xxxx-xxxx-…"
          />
        </label>
        <div className="field-row">
          <label className="field grow">
            <span>Username</span>
            <input
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              autoComplete="username"
              required
              pattern="[A-Za-z0-9][A-Za-z0-9._\-]{0,63}"
              aria-describedby="username-hint"
            />
            <small id="username-hint" className="muted">
              Letters, digits, ., _ or -, starting with a letter or digit
            </small>
          </label>
          <label className="field grow">
            <span>Display name (optional)</span>
            <input value={displayName} onChange={(e) => setDisplayName(e.target.value)} autoComplete="name" />
          </label>
        </div>
        {canPasskey ? (
          <>
            <label className="field">
              <span>Name for this passkey (optional)</span>
              <input
                value={passkeyName}
                onChange={(e) => setPasskeyName(e.target.value)}
                placeholder="Laptop, phone, security key…"
              />
            </label>
            <div className="actions">
              <button type="submit" className="primary" disabled={busy || !token}>
                Create admin with a passkey
              </button>
            </div>
          </>
        ) : (
          <PasskeyHint status={status} />
        )}
        {canPassword && (
          <fieldset className="auth-alt">
            <legend>Or with a password</legend>
            <label className="field">
              <span>Password (at least 12 characters)</span>
              <input
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                autoComplete="new-password"
                minLength={12}
              />
            </label>
            <div className="actions">
              <button type="button" disabled={busy || !token || password.length < 12} onClick={withPassword}>
                Create admin with a password
              </button>
            </div>
          </fieldset>
        )}
        {error && (
          <p className="error-text" role="alert">
            {error}
          </p>
        )}
      </form>
    </div>
  );
}

import { useEffect, useState, type FormEvent } from "react";

import { authApi } from "../api/client";
import type { AuthStatus } from "../api/generated/AuthStatus";
import { describeError, readSetupToken } from "./flow";
import { AuthHeader } from "./AuthHeader";
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
        <AuthHeader title="Set up apitorrent" lead="Create the first admin account for this server." />
        <InsecureBanner status={status} />
        <label className="field">
          <span>Setup token</span>
          <input
            className="mono"
            value={token}
            onChange={(e) => setToken(e.target.value)}
            autoComplete="off"
            autoCapitalize="none"
            spellCheck={false}
            required
            placeholder="xxxx-xxxx-xxxx-…"
            aria-describedby="setup-token-hint"
          />
          <small id="setup-token-hint" className="field-hint">
            The daemon prints it on its console when it starts, and a new one each hour until it's used.
          </small>
        </label>
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
              aria-describedby="username-hint"
            />
            <small id="username-hint" className="field-hint">
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
            <button type="submit" className="primary auth-submit" disabled={busy || !token}>
              {busy ? "Waiting for your passkey…" : "Create admin with a passkey"}
            </button>
          </>
        ) : (
          <PasskeyHint status={status} />
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
                aria-describedby="setup-password-hint"
              />
              <small id="setup-password-hint" className="field-hint">
                At least 12 characters
              </small>
            </label>
            <button
              type="button"
              className={canPasskey ? "auth-submit" : "primary auth-submit"}
              disabled={busy || !token || password.length < 12}
              onClick={withPassword}
            >
              Create admin with a password
            </button>
          </fieldset>
        )}
        {error && (
          <p className="auth-error" role="alert">
            {error}
          </p>
        )}
      </form>
    </div>
  );
}

import { useEffect, useRef, useState, type FormEvent } from "react";

import { authApi } from "../api/client";
import type { AuthStatus } from "../api/generated/AuthStatus";
import { describeError } from "./flow";
import { passkeysSupported, signWithPasskey } from "./webauthn";

interface Props {
  status: AuthStatus;
  onDone: (status: AuthStatus) => void;
  onCancel: () => void;
  /** Why a fresh sign-in is needed; credential changes by default. */
  reason?: string;
}

/** "Confirm it's you": a fresh sign-in before credential changes. */
export function ReauthDialog({ status, onDone, onCancel, reason }: Props) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    dialog.current?.showModal();
  }, []);

  const canPasskey = status.passkeysAvailable && passkeysSupported();
  const canPassword = status.passwordMode === "fallback" && status.user?.hasPassword === true;

  async function run(action: () => Promise<AuthStatus>) {
    setBusy(true);
    setError(null);
    try {
      onDone(await action());
    } catch (e) {
      setError(describeError(e));
    } finally {
      setBusy(false);
    }
  }

  function withPasskey() {
    void run(async () => {
      const start = await authApi.reauthStart();
      const credential = await signWithPasskey(start.options);
      return authApi.reauthFinish(start.ceremonyId, credential);
    });
  }

  function withPassword(e: FormEvent) {
    e.preventDefault();
    void run(() => authApi.reauthPassword(password));
  }

  return (
    <dialog ref={dialog} className="dialog" onClose={onCancel} aria-labelledby="reauth-title">
      <form onSubmit={withPassword}>
        <h2 id="reauth-title">Confirm it's you</h2>
        <p className="muted">{reason ?? "Changing how you sign in needs a fresh sign-in."}</p>
        {canPasskey && (
          <div className="actions start">
            <button type="button" className="primary" onClick={withPasskey} disabled={busy}>
              Use a passkey
            </button>
          </div>
        )}
        {canPassword && (
          <label className="field">
            <span>Or your password</span>
            <input
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              autoComplete="current-password"
            />
          </label>
        )}
        {!canPasskey && !canPassword && (
          <p className="error-text">
            There's no way to confirm it here.
            {status.passkeyHint && <> Open {status.passkeyHint} to use your passkey.</>}
          </p>
        )}
        {error && (
          <p className="error-text" role="alert">
            {error}
          </p>
        )}
        <div className="actions">
          <button type="button" onClick={onCancel}>
            Cancel
          </button>
          {canPassword && (
            <button type="submit" disabled={busy || !password}>
              Confirm with password
            </button>
          )}
        </div>
      </form>
    </dialog>
  );
}

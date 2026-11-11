import { useCallback, useEffect, useState } from "react";

import { App } from "../App";
import { authApi, onAuthLost } from "../api/client";
import type { AuthStatus } from "../api/generated/AuthStatus";
import { isDesktop } from "../desktop/bridge";
import { DesktopProblem } from "../desktop/DesktopProblem";
import { navigate, usePath } from "../router";
import { PluginsPage } from "../plugins/PluginsPage";
import { RulesPage } from "../rules/RulesPage";
import { SpacesPage } from "../spaces/SpacesPage";
import { UsersPage } from "../users/UsersPage";
import { AccountPage } from "./AccountPage";
import { EnrollPage } from "./EnrollPage";
import { viewFor } from "./flow";
import { LoginPage } from "./LoginPage";
import { SetupPage } from "./SetupPage";

/** Shows setup, sign-in, or the app, depending on the auth status. */
export function AuthGate() {
  // null while loading, undefined if the daemon couldn't be reached.
  const [status, setStatus] = useState<AuthStatus | null | undefined>(null);
  const path = usePath();

  const refresh = useCallback(async () => {
    try {
      setStatus(await authApi.status());
    } catch {
      setStatus(undefined);
    }
  }, []);

  useEffect(() => {
    void refresh();
    return onAuthLost(() => void refresh());
  }, [refresh]);

  const view = viewFor(status);
  useEffect(() => {
    if (view === "app" && path === "/setup") navigate("/", { replace: true });
  }, [view, path]);

  if (view === "loading") {
    return (
      <div className="auth-page">
        <p className="muted">Loading…</p>
      </div>
    );
  }
  if (isDesktop() && view !== "app") return <DesktopProblem view={view} retry={() => void refresh()} />;
  if (view === "unreachable" || !status) {
    return (
      <div className="auth-page">
        <section className="auth-card">
          <h1 className="brand">apitorrent</h1>
          <p>Can't reach the apitorrent daemon.</p>
          <div className="actions">
            <button type="button" className="primary" onClick={() => void refresh()}>
              Try again
            </button>
          </div>
        </section>
      </div>
    );
  }
  if (view === "setup") return <SetupPage status={status} onSignedIn={setStatus} />;
  // An invitation or recovery link, signed in or not.
  if (path === "/enroll" && !status.authDisabled) return <EnrollPage status={status} onSignedIn={setStatus} />;
  if (view === "login" || view === "secondFactor") return <LoginPage status={status} onSignedIn={setStatus} />;
  // `/pair` is the Account page with a device's code to approve (from the desktop app).
  if (path === "/account" || path === "/pair") return <AccountPage status={status} onStatus={setStatus} refresh={refresh} />;
  if (path === "/plugins") return <PluginsPage status={status} onStatus={setStatus} />;
  if (path === "/rules") return <RulesPage status={status} />;
  if (path === "/spaces") return <SpacesPage status={status} onStatus={setStatus} />;
  if (path === "/users") return <UsersPage status={status} onStatus={setStatus} />;
  return <App status={status} />;
}

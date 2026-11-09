// What the desktop window shows instead of the browser's sign-in and setup pages: the app
// signs in with the profile's API token, so those pages can't help here.

import { useEffect, useState } from "react";

import { authApi } from "../api/client";
import type { AuthView } from "../auth/flow";
import { ProfilesDialog } from "./DesktopGate";
import { describe } from "./util";

const EXPLAIN: Record<Exclude<AuthView, "app" | "loading">, string> = {
  unreachable: "apitorrent couldn't reach the server.",
  login:
    "The server didn't accept this connection's API token, or none is saved. Create a token on the server's Account page in a browser, then paste it into this connection's settings.",
  secondFactor: "The server is waiting for a password; the desktop app signs in with an API token instead. Check this connection's token.",
  setup: "The server has no accounts yet. Open the setup link it printed in a browser to create the first one, then create an API token for this app.",
};

export function DesktopProblem({ view, retry }: { view: AuthView; retry: () => void }) {
  const [detail, setDetail] = useState<string | null>(null);
  const [profiles, setProfiles] = useState(false);
  useEffect(() => {
    if (view !== "unreachable") return;
    // The status call failed; ask again to show why.
    authApi.status().then(
      () => setDetail(null),
      (e: unknown) => setDetail(describe(e)),
    );
  }, [view]);
  const text = view === "app" || view === "loading" ? "" : EXPLAIN[view];
  return (
    <div className="auth-page">
      <section className="auth-card">
        <h1 className="brand">apitorrent</h1>
        <p>{text}</p>
        {detail && <p className="error-text">{detail}</p>}
        <div className="actions">
          <button type="button" onClick={() => setProfiles(true)}>
            Connections
          </button>
          <button type="button" className="primary" onClick={retry}>
            Try again
          </button>
        </div>
      </section>
      {profiles && <ProfilesDialog onClose={() => setProfiles(false)} />}
    </div>
  );
}

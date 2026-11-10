// "A new version of the app is available", in the corner with the other desktop notices.
// The app checks once a day; it installs only an update signed with apitorrent's key, and
// only when asked. In Local mode the core runs inside the app, so this updates it too.

import { useEffect, useState } from "react";

import { desktop, type AppUpdateView } from "./bridge";
import { describe } from "./util";

export function AppUpdateNotice() {
  const [view, setView] = useState<AppUpdateView | null>(null);
  const [dismissed, setDismissed] = useState(false);
  const [installing, setInstalling] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    void desktop.appUpdateCheck().then(setView, () => {});
    const off = desktop.onAppUpdate((v) => {
      setView(v);
      setDismissed(false);
    });
    return () => void off.then((f) => f());
  }, []);

  if (!view?.available || dismissed) return null;

  async function install() {
    setInstalling(true);
    setError(null);
    try {
      // On success the app restarts and this page goes away.
      await desktop.appUpdateInstall();
    } catch (e) {
      setError(describe(e));
      setInstalling(false);
    }
  }

  return (
    <div className="desktop-notice app-update" role="status">
      <div>
        <div>
          apitorrent {view.latestVersion} is available (this is {view.currentVersion}).
        </div>
        {!view.installsEnabled && <div className="hint muted">{view.installsDisabledReason}.</div>}
        {error && <div className="error-text">{error}</div>}
      </div>
      <div className="app-update-actions">
        {view.installsEnabled && (
          <button type="button" className="primary" disabled={installing} onClick={() => void install()}>
            {installing ? "Installing…" : "Install and restart"}
          </button>
        )}
        <button type="button" className="link" disabled={installing} onClick={() => setDismissed(true)}>
          Later
        </button>
      </div>
    </div>
  );
}

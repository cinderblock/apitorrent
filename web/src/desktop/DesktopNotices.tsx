import { AppUpdateNotice } from "./AppUpdateNotice";
import { dismissNotice, useNotices } from "./notices";

/** Messages from the desktop app, in the corner until dismissed. */
export function DesktopNotices() {
  const notices = useNotices();
  return (
    <div className="desktop-notices" role="status">
      <AppUpdateNotice />
      {notices.map((n) => (
        <div key={n.id} className="desktop-notice">
          <span>{n.text}</span>
          <button type="button" className="link" onClick={() => dismissNotice(n.id)}>
            Dismiss
          </button>
        </div>
      ))}
    </div>
  );
}

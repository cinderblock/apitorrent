import { dismissNotice, useNotices } from "./notices";

/** Messages from the desktop app, in the corner until dismissed. */
export function DesktopNotices() {
  const notices = useNotices();
  if (notices.length === 0) return null;
  return (
    <div className="desktop-notices" role="status">
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

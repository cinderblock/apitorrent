/** The torrent list of a space with no torrents yet: the ways to add some. */
export function EmptyStart({ mayWrite, mayImport, narrow, onAdd, onImport }: { mayWrite: boolean; mayImport: boolean; narrow: boolean; onAdd: () => void; onImport: () => void }) {
  if (!mayWrite) return <>No torrents in this space yet.</>;
  return (
    <div className="empty-start">
      <p>No torrents here yet.</p>
      <p>
        {narrow ? "Use Add for a .torrent file or a magnet link." : "Drop a .torrent file anywhere, paste a magnet link with Add, or open one from your browser."}
      </p>
      <div className="actions">
        <button type="button" className="primary" onClick={onAdd}>
          Add a torrent…
        </button>
        {mayImport && (
          <button type="button" onClick={onImport}>
            Import from Deluge…
          </button>
        )}
      </div>
      {mayImport && <p className="muted">Moving from Deluge? The import keeps each torrent's files, progress and labels, and finds Deluge on this server for you.</p>}
    </div>
  );
}

/** "Loading 1,200 of 9,800 torrents…", while a snapshot arrives in parts. */
export function loadingText({ received, total }: { received: number; total: number }): string {
  return `Loading ${received.toLocaleString()} of ${total.toLocaleString()} torrents…`;
}

/** The status chip's short form: "Loading 12%". */
export function loadingCount({ received, total }: { received: number; total: number }): string {
  return `Loading ${Math.floor((100 * received) / Math.max(total, 1))}%`;
}

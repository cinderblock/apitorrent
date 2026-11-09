// Magnet links and .torrent files the OS hands the desktop app (a click on a magnet link,
// "Open with" on a .torrent file), turned into input for the Add dialog.

import { useEffect, useRef } from "react";

import { desktop, isDesktop, type IncomingLink } from "./bridge";
import { addNotice } from "./notices";

export interface LinkBatch {
  files: File[];
  magnets: string[];
}

/** Split incoming links into files and magnets; problems become notices. */
export function toBatch(links: IncomingLink[]): LinkBatch {
  const batch: LinkBatch = { files: [], magnets: [] };
  for (const l of links) {
    if (l.kind === "magnet") batch.magnets.push(l.uri);
    else if (l.kind === "torrentFile") {
      const bytes = Uint8Array.from(atob(l.data), (c) => c.charCodeAt(0));
      batch.files.push(new File([bytes], l.name, { type: "application/x-bittorrent" }));
    } else addNotice(l.message);
  }
  return batch;
}

/** Call `onBatch` with links waiting now, and with each new arrival. */
export function useIncomingLinks(onBatch: (batch: LinkBatch) => void): void {
  const handler = useRef(onBatch);
  handler.current = onBatch;
  useEffect(() => {
    if (!isDesktop()) return;
    let stopped = false;
    const take = () => {
      void desktop.takeLinks().then((links) => {
        if (stopped || links.length === 0) return;
        const batch = toBatch(links);
        if (batch.files.length > 0 || batch.magnets.length > 0) handler.current(batch);
      });
    };
    take();
    const unlisten = desktop.onLinks(take);
    return () => {
      stopped = true;
      void unlisten.then((f) => f());
    };
  }, []);
}

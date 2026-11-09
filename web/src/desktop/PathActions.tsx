// "Show in folder" and "Open" for a torrent's files, in the desktop app. They work through a
// verified path mapping (Remote) or directly (Local); otherwise they say why not.

import { useEffect, useState } from "react";

import { desktop, isDesktop, type PathAction } from "./bridge";
import { addNotice } from "./notices";
import { describe } from "./util";

// One availability check per save path for a while, not one per file row.
const AVAILABILITY_TTL_MS = 15_000;
const availability = new Map<string, { at: number; result: Promise<string | null> }>();

function checkAvailable(savePath: string): Promise<string | null> {
  const hit = availability.get(savePath);
  if (hit && Date.now() - hit.at < AVAILABILITY_TTL_MS) return hit.result;
  const result = desktop.pathAvailable(savePath).catch((e: unknown) => describe(e));
  availability.set(savePath, { at: Date.now(), result });
  return result;
}

/** `undefined` while checking, `null` when available, else why not. */
function useAvailable(savePath: string): string | null | undefined {
  const [state, setState] = useState<string | null | undefined>(undefined);
  useEffect(() => {
    let live = true;
    void checkAvailable(savePath).then((r) => live && setState(r));
    return () => {
      live = false;
    };
  }, [savePath]);
  return state;
}

function run(action: PathAction, savePath: string, file?: string) {
  desktop.pathAction(action, savePath, file).catch((e: unknown) => {
    // The mapping may have changed since; check again next time.
    availability.delete(savePath);
    addNotice(describe(e));
  });
}

interface Props {
  savePath: string;
  /** A path inside the save directory, joined with `/` (a torrent's name or file path). */
  file: string;
  /** Show why the buttons aren't there (once per pane, not on every file row). */
  explain?: boolean;
}

export function PathActions({ savePath, file, explain = false }: Props) {
  const available = useAvailable(savePath);
  if (!isDesktop() || available === undefined) return null;
  if (available !== null) return explain ? <span className="muted path-unavailable">{available}</span> : null;
  return (
    <span className="path-actions">
      <button type="button" className="link" onClick={() => run("reveal", savePath, file)}>
        Show in folder
      </button>
      <button type="button" className="link" onClick={() => run("open", savePath, file)}>
        Open
      </button>
    </span>
  );
}

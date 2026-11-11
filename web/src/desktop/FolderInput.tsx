// A text field for a folder on this computer. In the desktop app it gets a "Browse…" button
// that opens the OS's folder picker; in a browser it stays a typed path (a page can't learn
// local paths, and the folder may be on the server anyway).

import { useEffect, useState, type InputHTMLAttributes } from "react";

import { desktop, isDesktop } from "./bridge";
import { addNotice } from "./notices";
import { describe } from "./util";

interface Props extends Omit<InputHTMLAttributes<HTMLInputElement>, "value" | "onChange"> {
  value: string;
  onChange: (value: string) => void;
  /** The picker's title, such as "Download folder". */
  pickerTitle: string;
  /** Offer the picker (default: in the desktop app). Off for paths on a remote server. */
  browse?: boolean;
}

export function FolderInput({ value, onChange, pickerTitle, browse = isDesktop(), className, ...rest }: Props) {
  const [busy, setBusy] = useState(false);
  async function pick() {
    setBusy(true);
    try {
      const picked = await desktop.pickFolder(pickerTitle, value.trim() || undefined);
      if (picked !== null) onChange(picked);
    } catch (e) {
      addNotice(describe(e));
    } finally {
      setBusy(false);
    }
  }
  const input = <input {...rest} className={browse ? undefined : className} value={value} onChange={(e) => onChange(e.target.value)} />;
  if (!browse) return input;
  return (
    <span className={`folder-input ${className ?? ""}`.trim()}>
      {input}
      <button type="button" disabled={busy || rest.disabled} onClick={() => void pick()}>
        Browse…
      </button>
    </span>
  );
}

/**
 * Whether the window is the desktop app connected to the core running on this computer
 * (Local mode), where the server's paths are this computer's paths. `false` in a browser
 * and in Remote mode; `false` until known.
 */
export function useLocalMode(): boolean {
  const [local, setLocal] = useState(false);
  useEffect(() => {
    if (!isDesktop()) return;
    let live = true;
    desktop.state().then(
      (s) => live && setLocal(s.connection?.local ?? false),
      () => {},
    );
    return () => {
      live = false;
    };
  }, []);
  return local;
}

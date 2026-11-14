// A link to a file or folder on the web server that serves its storage root (Settings → Files →
// Storage roots, an `https://` locator). Shown in browsers and in the desktop app alike.

import { useEffect, useState } from "react";

import { api } from "../api/client";
import type { StorageRoot } from "../api/generated/StorageRoot";
import { isDesktop } from "../desktop/bridge";
import { webUrl } from "./webLinks";

// One roots request for every link on the page, refreshed now and then.
const ROOTS_TTL_MS = 60_000;
let cached: { at: number; roots: Promise<StorageRoot[]> } | null = null;

function loadRoots(): Promise<StorageRoot[]> {
  if (cached && Date.now() - cached.at < ROOTS_TTL_MS) return cached.roots;
  // No links rather than an error: the links are a convenience.
  const roots = api.roots().catch(() => []);
  cached = { at: Date.now(), roots };
  return roots;
}

/** Fetch the roots again next time (an admin changed their locators). */
export function forgetRoots() {
  cached = null;
}

function useRoots(): StorageRoot[] {
  const [roots, setRoots] = useState<StorageRoot[]>([]);
  useEffect(() => {
    let live = true;
    void loadRoots().then((r) => live && setRoots(r));
    return () => {
      live = false;
    };
  }, []);
  return roots;
}

interface Props {
  savePath: string;
  /** A path inside the save directory, joined with `/`; empty for the directory itself. */
  path?: string;
  /** A folder: links to its listing. */
  dir?: boolean;
  children: React.ReactNode;
}

export function WebLink({ savePath, path = "", dir = false, children }: Props) {
  const url = webUrl(useRoots(), savePath, path, dir);
  if (!url) return null;
  // The desktop app opens web pages in the default browser when its window would navigate
  // away, so it needs a plain link; a browser gets a new tab.
  const newTab = isDesktop() ? {} : { target: "_blank" };
  return (
    <a className="web-link" href={url} rel="noreferrer" {...newTab}>
      {children}
    </a>
  );
}

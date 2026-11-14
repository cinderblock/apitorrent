// Settings → Files → Storage roots: where other computers reach each of the instance's download
// directories (share locators). The desktop app suggests path mappings from them; web locators
// give files download links, and the daemon checks them by fetching the root's marker file.

import { useEffect, useState } from "react";

import { api, ApiRequestError } from "../api/client";
import type { LocatorCheck } from "../api/generated/LocatorCheck";
import type { StorageRoot } from "../api/generated/StorageRoot";
import { Empty, SettingRow } from "../settings/controls";
import { forgetRoots } from "./WebLink";
import { webBase } from "./webLinks";

/** One locator per line (folder names may hold commas), blanks dropped. */
export function parseLocators(text: string): string[] {
  return text
    .split(/\r?\n/)
    .map((l) => l.trim())
    .filter((l) => l !== "");
}

function RootRow({ root, onSaved }: { root: StorageRoot; onSaved: (r: StorageRoot) => void }) {
  const [text, setText] = useState(root.locators.join("\n"));
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<{ ok: boolean; text: string } | null>(null);
  const dirty = parseLocators(text).join("\n") !== root.locators.join("\n");
  // The daemon's check of each saved web locator, `null` while it runs.
  const [checks, setChecks] = useState<[string, LocatorCheck | null][]>([]);
  const web = root.locators.filter((l) => webBase(l) !== null);

  async function check(locators: string[]) {
    setChecks(locators.map((l) => [l, null]));
    const results = await Promise.all(
      locators.map(async (l): Promise<[string, LocatorCheck]> => {
        try {
          return [l, await api.checkRootLocator(root.id, l)];
        } catch (e) {
          return [l, { verified: false, message: e instanceof ApiRequestError ? e.message : String(e) }];
        }
      }),
    );
    setChecks(results);
  }

  async function save() {
    setBusy(true);
    setMessage(null);
    try {
      const saved = await api.setRootLocators(root.id, parseLocators(text));
      setText(saved.locators.join("\n"));
      onSaved(saved);
      forgetRoots();
      setMessage({ ok: true, text: "Saved." });
      void check(saved.locators.filter((l) => webBase(l) !== null));
    } catch (e) {
      setMessage({ ok: false, text: e instanceof ApiRequestError ? e.message : String(e) });
    } finally {
      setBusy(false);
    }
  }

  const id = `locators-${root.id}`;
  return (
    <SettingRow label={<code>{root.path}</code>} description="Shared as (one per line)" htmlFor={id} wide>
      <textarea
        id={id}
        className="s-text mono"
        rows={Math.max(2, parseLocators(text).length + 1)}
        spellCheck={false}
        value={text}
        placeholder="Not shared yet"
        onChange={(e) => {
          setText(e.target.value);
          setMessage(null);
        }}
      />
      <div className="s-inline">
        <button type="button" disabled={busy || !dirty} onClick={() => void save()}>
          {busy ? "Saving…" : "Save"}
        </button>
        {web.length > 0 && (
          <button type="button" disabled={busy || checks.some(([, c]) => c === null)} onClick={() => void check(web)}>
            Check web {web.length === 1 ? "address" : "addresses"}
          </button>
        )}
        {message && <span className={message.ok ? "ok-text" : "error-text"}>{message.text}</span>}
      </div>
      {checks.length > 0 && (
        <ul className="locator-checks">
          {checks.map(([l, c]) => (
            <li key={l}>
              <code>{l}</code>:{" "}
              {c === null ? <span className="muted">checking…</span> : <span className={c.verified ? "ok-text" : "warn-text"}>{c.message}</span>}
            </li>
          ))}
        </ul>
      )}
    </SettingRow>
  );
}

/** Server admins only: roots belong to the instance. Each folder saves on its own. */
export function StorageRoots() {
  const [roots, setRoots] = useState<StorageRoot[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    api.roots().then(setRoots, (e: unknown) => setError(e instanceof Error ? e.message : String(e)));
  }, []);
  return (
    <>
      {roots?.map((r) => (
        <RootRow key={r.id} root={r} onSaved={(saved) => setRoots(roots.map((x) => (x.id === saved.id ? saved : x)))} />
      ))}
      {roots?.length === 0 && <Empty>This server reports no storage roots.</Empty>}
      {error && <p className="error-text">{error}</p>}
    </>
  );
}

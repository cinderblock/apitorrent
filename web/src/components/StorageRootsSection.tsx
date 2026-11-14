// Settings → Files → Storage roots: where other computers reach each of the instance's download
// directories (share locators). The desktop app suggests path mappings from them.

import { useEffect, useState } from "react";

import { api, ApiRequestError } from "../api/client";
import type { StorageRoot } from "../api/generated/StorageRoot";
import { Empty, SettingRow } from "../settings/controls";

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

  async function save() {
    setBusy(true);
    setMessage(null);
    try {
      const saved = await api.setRootLocators(root.id, parseLocators(text));
      setText(saved.locators.join("\n"));
      onSaved(saved);
      setMessage({ ok: true, text: "Saved." });
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
        {message && <span className={message.ok ? "ok-text" : "error-text"}>{message.text}</span>}
      </div>
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

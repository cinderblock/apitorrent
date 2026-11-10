// The Spaces page (/spaces), for server admins: every space, its storage and limits, and
// its members. A space keeps its own torrents, folders, limits, labels, rules and plugin
// settings; one torrent in two spaces is downloaded once and shared.

import { useCallback, useEffect, useState } from "react";

import type { Space } from "../api/generated/Space";
import type { SpaceMember } from "../api/generated/SpaceMember";
import type { SpaceRole } from "../api/generated/SpaceRole";
import type { SpaceView } from "../api/generated/SpaceView";
import type { UserSummary } from "../api/generated/UserSummary";
import { describeError } from "../auth/flow";
import { navigate } from "../router";
import { spacesApi } from "./api";
import { loadSpaceContext, useSpaceContext } from "./context";
import { emptyForm, formOf, settingsOf, slugFrom, type SpaceForm } from "./form";
import { spaceHome } from "./prefix";
import "./spaces.css";

const ROLES: { value: SpaceRole; text: string }[] = [
  { value: "viewer", text: "Viewer (reads only)" },
  { value: "user", text: "User (adds and manages torrents)" },
  { value: "admin", text: "Admin (also rules, channels and imports)" },
];

type Editing = { slug: string | null; form: SpaceForm } | null;

export function SpacesPage() {
  const ctx = useSpaceContext();
  const [spaces, setSpaces] = useState<SpaceView[] | null>(null);
  const [editing, setEditing] = useState<Editing>(null);
  const [members, setMembers] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    try {
      setSpaces(await spacesApi.list());
    } catch (e) {
      setError(describeError(e));
    }
  }, []);
  useEffect(() => void load(), [load]);

  const act = async (f: () => Promise<unknown>) => {
    setBusy(true);
    setError(null);
    try {
      await f();
      await load();
      await loadSpaceContext();
      return true;
    } catch (e) {
      setError(describeError(e));
      return false;
    } finally {
      setBusy(false);
    }
  };

  async function edit(slug: string) {
    setError(null);
    try {
      const s: Space = await spacesApi.get(slug);
      setEditing({ slug, form: formOf(s.slug, s.name, s.description, s.settings) });
      setMembers(null);
    } catch (e) {
      setError(describeError(e));
    }
  }

  async function save(e: React.FormEvent) {
    e.preventDefault();
    if (!editing) return;
    const settings = settingsOf(editing.form);
    if (typeof settings === "string") {
      setError(settings);
      return;
    }
    const f = editing.form;
    const ok = await act(() =>
      editing.slug === null
        ? spacesApi.create({ slug: f.slug, name: f.name, description: f.description, settings })
        : spacesApi.update(editing.slug, { name: f.name, description: f.description, settings }),
    );
    if (ok) setEditing(null);
  }

  if (ctx && !ctx.serverAdmin) {
    return (
      <div className="account">
        <p className="account-section muted">Only a server admin manages spaces.</p>
      </div>
    );
  }

  const set = (k: keyof SpaceForm) => (e: React.ChangeEvent<HTMLInputElement | HTMLTextAreaElement>) =>
    setEditing((ed) => {
      if (!ed) return ed;
      const form = { ...ed.form, [k]: e.target.value };
      // A new space's slug follows its name until it is typed itself.
      if (ed.slug === null && k === "name" && ed.form.slug === slugFrom(ed.form.name)) form.slug = slugFrom(e.target.value);
      return { ...ed, form };
    });

  return (
    <div className="account spaces-page">
      <header className="toolbar account-toolbar">
        <h1 className="brand">apitorrent</h1>
        <button type="button" className="link" onClick={() => navigate("/")}>
          ← Torrents
        </button>
        <span className="spacer" />
        <button type="button" className="primary" onClick={() => setEditing({ slug: null, form: emptyForm() })}>
          New space…
        </button>
      </header>
      <section className="account-section">
        <h2>Spaces</h2>
        <p className="muted">
          Each space has its own torrents, folders, limits, labels, rules and members, behind one peer port. Its pages
          are at <code>/s/&lt;slug&gt;/</code>; an API token made in a space works there only, which is how a
          qBittorrent client (Sonarr, Radarr) lands in the right one. The same torrent in two spaces is downloaded once,
          and each space gets its own copy of the files (a hard link where the disk allows).
        </p>
      </section>
      {error && (
        <p className="error-text" role="alert">
          {error}
        </p>
      )}
      {editing && (
        <form className="account-section space-editor" onSubmit={(e) => void save(e)}>
          <h3>{editing.slug === null ? "New space" : `Space ${editing.slug}`}</h3>
          <div className="field-row">
            <label className="field grow">
              <span>Name</span>
              <input required value={editing.form.name} onChange={set("name")} />
            </label>
            <label className="field">
              <span>Slug (in URLs)</span>
              <input
                required
                pattern="[a-z0-9](?:[a-z0-9-]{0,38}[a-z0-9])?"
                disabled={editing.slug !== null}
                value={editing.form.slug}
                onChange={set("slug")}
              />
            </label>
          </div>
          <label className="field">
            <span>Description</span>
            <input value={editing.form.description} onChange={set("description")} />
          </label>
          <label className="field">
            <span>Storage roots: folders it may save into, one per line (none: anywhere)</span>
            <textarea rows={2} value={editing.form.roots} onChange={set("roots")} />
          </label>
          <label className="field">
            <span>Default folder (empty: its first storage root)</span>
            <input value={editing.form.downloadDir} onChange={set("downloadDir")} />
          </label>
          <label className="field">
            <span>Folders by label: label = folder, one per line (a relative folder is inside the default one)</span>
            <textarea rows={2} value={editing.form.folderRules} onChange={set("folderRules")} />
          </label>
          <div className="field-row">
            <label className="field grow">
              <span>Download limit (KiB/s)</span>
              <input inputMode="numeric" value={editing.form.downloadKib} onChange={set("downloadKib")} />
            </label>
            <label className="field grow">
              <span>Upload limit (KiB/s)</span>
              <input inputMode="numeric" value={editing.form.uploadKib} onChange={set("uploadKib")} />
            </label>
            <label className="field grow">
              <span>Connections</span>
              <input inputMode="numeric" value={editing.form.maxConnections} onChange={set("maxConnections")} />
            </label>
          </div>
          <div className="field-row">
            <label className="field grow">
              <span>Active torrents</span>
              <input inputMode="numeric" value={editing.form.maxActive} onChange={set("maxActive")} />
            </label>
            <label className="field grow">
              <span>Active downloads</span>
              <input inputMode="numeric" value={editing.form.maxActiveDownloads} onChange={set("maxActiveDownloads")} />
            </label>
            <label className="field grow">
              <span>Active seeds</span>
              <input inputMode="numeric" value={editing.form.maxActiveSeeds} onChange={set("maxActiveSeeds")} />
            </label>
          </div>
          <p className="muted">Empty limits leave only the server's own.</p>
          <div className="actions">
            <button type="button" onClick={() => setEditing(null)}>
              Cancel
            </button>
            <button type="submit" className="primary" disabled={busy}>
              {editing.slug === null ? "Create" : "Save"}
            </button>
          </div>
        </form>
      )}
      {spaces?.map((s) => (
        <section key={s.id} className="account-section space-card">
          <div className="space-head">
            <div>
              <strong>{s.name}</strong> <code className="muted">{s.slug}</code>
              {ctx?.current.slug === s.slug && <span className="badge">this page</span>}
              {s.description && <p className="muted">{s.description}</p>}
            </div>
            <div className="actions">
              <a className="button-like" href={spaceHome(s.slug)}>
                Open
              </a>
              <button type="button" disabled={busy} onClick={() => void edit(s.slug)}>
                Change…
              </button>
              <button type="button" disabled={busy} onClick={() => setMembers(members === s.slug ? null : s.slug)}>
                Members
              </button>
              {s.slug !== "default" && (
                <DeleteButton busy={busy} onDelete={() => void act(() => spacesApi.remove(s.slug))} />
              )}
            </div>
          </div>
          {members === s.slug && <Members slug={s.slug} onError={setError} />}
        </section>
      ))}
    </div>
  );
}

function DeleteButton({ busy, onDelete }: { busy: boolean; onDelete: () => void }) {
  const [confirm, setConfirm] = useState(false);
  if (!confirm) {
    return (
      <button type="button" disabled={busy} onClick={() => setConfirm(true)}>
        Delete…
      </button>
    );
  }
  return (
    <>
      <button type="button" className="danger" disabled={busy} onClick={onDelete}>
        Delete the space
      </button>
      <button type="button" onClick={() => setConfirm(false)}>
        Keep
      </button>
    </>
  );
}

function Members({ slug, onError }: { slug: string; onError: (e: string | null) => void }) {
  const [list, setList] = useState<SpaceMember[] | null>(null);
  const [users, setUsers] = useState<UserSummary[]>([]);
  const [adding, setAdding] = useState({ user: "", role: "user" as SpaceRole });

  const load = useCallback(async () => {
    try {
      const [m, u] = await Promise.all([spacesApi.members(slug), spacesApi.users()]);
      setList(m);
      setUsers(u);
    } catch (e) {
      onError(describeError(e));
    }
  }, [slug, onError]);
  useEffect(() => void load(), [load]);

  const act = async (f: () => Promise<unknown>) => {
    onError(null);
    try {
      await f();
      await load();
    } catch (e) {
      onError(describeError(e));
    }
  };

  const outside = users.filter((u) => !list?.some((m) => m.userId === u.id));
  return (
    <div className="space-members">
      <p className="muted">Server admins can use every space without being members.</p>
      {list?.length === 0 && <p className="muted">No members yet.</p>}
      <ul className="space-member-list">
        {list?.map((m) => (
          <li key={m.userId}>
            <span className="grow">
              {m.displayName} <span className="muted">({m.name})</span>
            </span>
            <select
              aria-label={`${m.name}'s role`}
              value={m.role}
              onChange={(e) => void act(() => spacesApi.setMember(slug, m.userId, e.target.value as SpaceRole))}
            >
              {ROLES.map((r) => (
                <option key={r.value} value={r.value}>
                  {r.text}
                </option>
              ))}
            </select>
            <button type="button" onClick={() => void act(() => spacesApi.removeMember(slug, m.userId))}>
              Remove
            </button>
          </li>
        ))}
      </ul>
      {outside.length > 0 && (
        <div className="field-row">
          <select aria-label="User" value={adding.user} onChange={(e) => setAdding({ ...adding, user: e.target.value })}>
            <option value="">Add a user…</option>
            {outside.map((u) => (
              <option key={u.id} value={u.id}>
                {u.displayName} ({u.name})
              </option>
            ))}
          </select>
          <select
            aria-label="Role"
            value={adding.role}
            onChange={(e) => setAdding({ ...adding, role: e.target.value as SpaceRole })}
          >
            {ROLES.map((r) => (
              <option key={r.value} value={r.value}>
                {r.text}
              </option>
            ))}
          </select>
          <button
            type="button"
            disabled={adding.user === ""}
            onClick={() =>
              void act(async () => {
                await spacesApi.setMember(slug, adding.user, adding.role);
                setAdding({ user: "", role: "user" });
              })
            }
          >
            Add
          </button>
        </div>
      )}
    </div>
  );
}

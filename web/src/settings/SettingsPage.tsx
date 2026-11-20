// The Settings page (/settings/<section>): a nav of sections, grouped by what they apply to
// (the server, this space, this device), and one section at a time. On a phone the nav is
// a screen of its own.
//
// The instance's settings are one draft: change anything in the sections that edit it and a
// bar offers Save and Discard, naming what changed. Sections that act at once (bans,
// passkeys, folders, updates, the theme) have their own buttons and never wait for Save.

import { useCallback, useEffect, useMemo, useRef, useState, type MouseEvent } from "react";

import { ApiRequestError, api } from "../api/client";
import type { AuthStatus } from "../api/generated/AuthStatus";
import type { Settings } from "../api/generated/Settings";
import { ClientIdentitySection } from "../components/ClientIdentitySection";
import { showDelugeImport, type JobRef } from "../components/importDialog";
import { TlsSection } from "../components/TlsSection";
import { TrackerPasskeysSection } from "../components/TrackerPasskeysSection";
import { UpdatesSection } from "../components/Updates";
import { navigate, usePath } from "../router";
import { useSpaceContext } from "../spaces/context";
import { withPrefix } from "../spaces/prefix";
import { useNarrow } from "../useNarrow";
import { dirtySections, draftProblems, filterSections, forSaving, listTitles, sectionById, visibleSections, type Section, type SectionGroup, type SectionId } from "./model";
import { AppearanceSection, BansSection, DelugeSection } from "./OtherSections";
import { FilesSection, GroupsSection, LibrarySection, NetworkSection, SeedingSection, TransfersSection, type DraftProps } from "./ServerSections";
import "./settings.css";

// The draft outlives the page, so going back to the torrents and forward again (the
// browser's buttons, which the page can't stop) keeps unsaved changes.
let kept: { saved: Settings; draft: Settings } | null = null;

function message(e: unknown): string {
  return e instanceof ApiRequestError || e instanceof Error ? e.message : String(e);
}

const same = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b);

function settingsPath(id?: SectionId): string {
  return id ? `/settings/${id}` : "/settings";
}

/** A link inside the app: a real href (so it can open in a new tab), routed in place on a plain click. */
function onAppLink(e: MouseEvent, go: () => void) {
  if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
  e.preventDefault();
  go();
}

/**
 * Open the Deluge import wizard. With a job (from the jobs tray): on that job, over the page
 * already showing. Without (from the empty torrent list): at its start, over Settings → Deluge.
 */
export function openDelugeImport(job?: JobRef): void {
  if (!job) navigate(settingsPath("deluge"));
  showDelugeImport(job ?? null);
}

export function SettingsPage({ status }: { status: AuthStatus }) {
  const path = usePath();
  const narrow = useNarrow();
  const space = useSpaceContext();
  // Without a space context (an older daemon), the instance role decides.
  const serverAdmin = space ? space.serverAdmin : status.authDisabled || status.user?.role === "admin";
  const spaceAdmin = serverAdmin || space?.current.role === "admin";
  const administersASpace = serverAdmin || (space?.spaces.some((s) => s.role === "admin") ?? false);
  const usersPage = serverAdmin && !status.authDisabled;

  const sections = useMemo(() => visibleSections({ serverAdmin, spaceAdmin }), [serverAdmin, spaceAdmin]);
  const requested = path.split("/")[2] ?? "";
  const current = sections.find((s) => s.id === requested);

  // `/settings` on a wide screen, or a section this person can't see: the first section.
  useEffect(() => {
    if (current) return;
    if (requested === "" && narrow) return;
    navigate(settingsPath(sections[0]?.id), { replace: true });
  }, [current, requested, narrow, sections]);

  // The settings, as saved and as edited.
  const [saved, setSaved] = useState<Settings | null>(kept?.saved ?? null);
  const [draft, setDraft] = useState<Settings | null>(kept?.draft ?? null);
  const [loadError, setLoadError] = useState<string | null>(null);
  useEffect(() => {
    api.settings().then(
      (s) => {
        setSaved(s);
        // Keep an unsaved draft only while the server still has what it was based on.
        setDraft((d) => (kept && same(kept.saved, s) && d ? d : s));
      },
      (e: unknown) => setLoadError(message(e)),
    );
  }, []);
  useEffect(() => {
    kept = saved && draft && !same(saved, draft) ? { saved, draft } : null;
  }, [saved, draft]);

  const update = useCallback((patch: Partial<Settings>) => setDraft((d) => (d ? { ...d, ...patch } : d)), []);
  const dirty = useMemo(() => (saved && draft ? dirtySections(saved, draft) : []), [saved, draft]);
  const problems = useMemo(() => (draft ? draftProblems(draft) : []), [draft]);

  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [justSaved, setJustSaved] = useState(false);
  const savedTimer = useRef<number | undefined>(undefined);

  const save = useCallback(async (): Promise<boolean> => {
    if (!draft || problems.length > 0) return false;
    setSaving(true);
    setSaveError(null);
    try {
      const body = forSaving(draft);
      await api.saveSettings(body);
      // Read back what the server kept (it may normalise).
      const fresh = await api.settings().catch(() => body);
      setSaved(fresh);
      setDraft(fresh);
      setJustSaved(true);
      window.clearTimeout(savedTimer.current);
      savedTimer.current = window.setTimeout(() => setJustSaved(false), 3000);
      return true;
    } catch (e) {
      setSaveError(message(e));
      return false;
    } finally {
      setSaving(false);
    }
  }, [draft, problems.length]);

  const discard = useCallback(() => {
    setDraft(saved);
    setSaveError(null);
  }, [saved]);

  // Ctrl/Cmd+S saves; closing or reloading the tab with changes asks first.
  useEffect(() => {
    if (dirty.length === 0) return;
    const key = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "s") {
        e.preventDefault();
        if (serverAdmin && !saving) void save();
      }
    };
    const unload = (e: BeforeUnloadEvent) => {
      e.preventDefault();
    };
    window.addEventListener("keydown", key);
    window.addEventListener("beforeunload", unload);
    return () => {
      window.removeEventListener("keydown", key);
      window.removeEventListener("beforeunload", unload);
    };
  }, [dirty.length, save, saving, serverAdmin]);

  // Leaving the page through its own links, with changes: ask.
  const [leaving, setLeaving] = useState<string | null>(null);
  const leave = (to: string) => {
    if (dirty.length > 0 && serverAdmin) setLeaving(to);
    else navigate(to);
  };

  const [query, setQuery] = useState("");
  const shown = filterSections(sections, query);

  // Escape backs out: a section on a phone goes back to the list of sections, otherwise back
  // to the torrents (asking first when there are unsaved changes). An open dialog takes its
  // own Escape, and a focused box just lets go of the focus first.
  const escape = useRef<() => void>(() => {});
  escape.current = () => {
    if (narrow && current) navigate("/settings");
    else leave("/");
  };
  useEffect(() => {
    const key = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || e.defaultPrevented || e.ctrlKey || e.metaKey || e.altKey || e.shiftKey) return;
      if (document.querySelector("dialog[open]")) return;
      const t = e.target;
      if (t instanceof HTMLInputElement || t instanceof HTMLTextAreaElement || t instanceof HTMLSelectElement) {
        // A search box with text clears it on Escape by itself; anything else just loses focus.
        if (!(t instanceof HTMLInputElement && t.type === "search" && t.value !== "")) t.blur();
        return;
      }
      escape.current();
    };
    window.addEventListener("keydown", key);
    return () => window.removeEventListener("keydown", key);
  }, []);

  const showNav = !narrow || !current;
  const showMain = current !== undefined;

  return (
    <div className="account settings-page">
      <header className="toolbar account-toolbar">
        <h1 className="brand">apitorrent</h1>
        <a className="button-link" href={withPrefix(location.pathname, "/")} onClick={(e) => onAppLink(e, () => leave("/"))}>
          ← Torrents
        </a>
        <span className="spacer" />
      </header>

      <div className={`s-layout${narrow ? " narrow" : ""}`}>
        {showNav && (
          <nav className="s-nav" aria-label="Settings sections">
            <h2 className="s-nav-title">Settings</h2>
            <input
              type="search"
              className="s-search"
              placeholder="Find a setting"
              aria-label="Find a setting"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && shown[0]) {
                  navigate(settingsPath(shown[0].id));
                  if (narrow) setQuery("");
                }
              }}
            />
            {shown.length === 0 && <p className="s-nav-none muted">Nothing matches “{query}”.</p>}
            <NavGroups
              sections={shown}
              current={current?.id}
              dirty={dirty}
              problems={problems.map((p) => p.section)}
              spaceName={space?.current.name}
              detailed={narrow}
            />
            {(administersASpace || usersPage) && query === "" && (
              <div className="s-nav-group">
                <h3>Administration</h3>
                <ul>
                  {administersASpace && (
                    <li>
                      <a href={withPrefix(location.pathname, "/spaces")} onClick={(e) => onAppLink(e, () => leave("/spaces"))}>
                        <span className="s-nav-label">Spaces</span>
                        <span className="s-nav-out" aria-hidden="true">
                          ↗
                        </span>
                        {narrow && <span className="s-nav-summary">Spaces, their members and invitations</span>}
                      </a>
                    </li>
                  )}
                  {usersPage && (
                    <li>
                      <a href={withPrefix(location.pathname, "/users")} onClick={(e) => onAppLink(e, () => leave("/users"))}>
                        <span className="s-nav-label">Users</span>
                        <span className="s-nav-out" aria-hidden="true">
                          ↗
                        </span>
                        {narrow && <span className="s-nav-summary">Everyone who can sign in to this server</span>}
                      </a>
                    </li>
                  )}
                </ul>
              </div>
            )}
          </nav>
        )}

        {showMain && current && (
          <main className="s-main" aria-labelledby="s-title">
            {narrow && (
              <a className="button-link s-back" href={withPrefix(location.pathname, "/settings")} onClick={(e) => onAppLink(e, () => navigate("/settings"))}>
                ← Settings
              </a>
            )}
            <header className="s-head">
              <p className="s-scope">{scopeText(current.group, space?.current.name)}</p>
              <h2 id="s-title">{current.title}</h2>
              <p className="muted">{current.summary}.</p>
            </header>
            {current.group === "server" && !serverAdmin && (
              <div className="s-note s-note-info">These settings apply to every space on this server. Only a server admin can change them.</div>
            )}
            <SectionBody
              section={current}
              draft={draft}
              saved={saved}
              loadError={loadError}
              update={update}
              serverAdmin={serverAdmin}
              onImport={() => showDelugeImport()}
            />
          </main>
        )}
      </div>

      {serverAdmin && (dirty.length > 0 || justSaved || saveError) && (
        <SaveBar
          dirty={dirty}
          problems={problems}
          saving={saving}
          error={saveError}
          justSaved={justSaved && dirty.length === 0}
          onSave={() => void save()}
          onDiscard={discard}
          onGo={(id) => navigate(settingsPath(id))}
        />
      )}

      {leaving && (
        <LeaveDialog
          dirty={dirty}
          canSave={problems.length === 0}
          saving={saving}
          onStay={() => setLeaving(null)}
          onDiscard={() => {
            const to = leaving;
            kept = null;
            setLeaving(null);
            setDraft(saved);
            navigate(to);
          }}
          onSave={async () => {
            const to = leaving;
            if (await save()) {
              setLeaving(null);
              navigate(to);
            } else {
              setLeaving(null);
            }
          }}
        />
      )}
    </div>
  );
}

function scopeText(group: SectionGroup, spaceName: string | undefined): string {
  if (group === "server") return "Server · every space";
  if (group === "space") return spaceName ? `This space · ${spaceName}` : "This space";
  return "This device";
}

const GROUPS: readonly { id: SectionGroup; title: (space?: string) => string }[] = [
  { id: "server", title: () => "Server" },
  { id: "space", title: (space) => (space ? `This space: ${space}` : "This space") },
  { id: "device", title: () => "This device" },
];

function NavGroups({
  sections,
  current,
  dirty,
  problems,
  spaceName,
  detailed,
}: {
  sections: readonly Section[];
  current: SectionId | undefined;
  dirty: readonly SectionId[];
  problems: readonly SectionId[];
  spaceName: string | undefined;
  /** Each item's summary under its title (the phone's nav screen). */
  detailed: boolean;
}) {
  return (
    <>
      {GROUPS.map((g) => {
        const items = sections.filter((s) => s.group === g.id);
        if (items.length === 0) return null;
        return (
          <div className="s-nav-group" key={g.id}>
            <h3>{g.title(spaceName)}</h3>
            <ul>
              {items.map((s) => {
                const bad = problems.includes(s.id);
                const changed = dirty.includes(s.id);
                return (
                  <li key={s.id}>
                    <a
                      href={withPrefix(location.pathname, settingsPath(s.id))}
                      aria-current={s.id === current ? "page" : undefined}
                      onClick={(e) => onAppLink(e, () => navigate(settingsPath(s.id)))}
                    >
                      <span className="s-nav-label">{s.title}</span>
                      {(bad || changed) && (
                        <span className={`s-dot${bad ? " bad" : ""}`}>
                          <span className="visually-hidden">{bad ? " (needs fixing)" : " (unsaved changes)"}</span>
                        </span>
                      )}
                      {detailed && <span className="s-nav-summary">{s.summary}</span>}
                    </a>
                  </li>
                );
              })}
            </ul>
          </div>
        );
      })}
    </>
  );
}

function SectionBody({
  section,
  draft,
  saved,
  loadError,
  update,
  serverAdmin,
  onImport,
}: {
  section: Section;
  draft: Settings | null;
  saved: Settings | null;
  loadError: string | null;
  update: (patch: Partial<Settings>) => void;
  serverAdmin: boolean;
  onImport: () => void;
}) {
  // Sections that don't need the instance's settings.
  switch (section.id) {
    case "bans":
      return <BansSection mayChange={serverAdmin} />;
    case "https":
      return <TlsSection />;
    case "updates":
      return <UpdatesSection />;
    case "passkeys":
      return <TrackerPasskeysSection />;
    case "deluge":
      return <DelugeSection onImport={onImport} />;
    case "appearance":
      return <AppearanceSection />;
    default:
      break;
  }
  if (loadError) return <p className="error-text">{loadError}</p>;
  if (!draft || !saved) return <p className="muted">Loading…</p>;
  const props: DraftProps = { draft, saved, update, serverAdmin };
  // A disabled fieldset makes every control read-only for those who can't change them.
  return (
    <fieldset className="s-fields" disabled={!serverAdmin}>
      {section.id === "transfers" && <TransfersSection {...props} />}
      {section.id === "seeding" && <SeedingSection {...props} />}
      {section.id === "groups" && <GroupsSection {...props} />}
      {section.id === "network" && <NetworkSection {...props} />}
      {section.id === "identity" && <ClientIdentitySection value={draft.identity} onChange={(identity) => update({ identity })} />}
      {section.id === "files" && <FilesSection {...props} />}
      {section.id === "library" && <LibrarySection {...props} />}
    </fieldset>
  );
}

function SaveBar({
  dirty,
  problems,
  saving,
  error,
  justSaved,
  onSave,
  onDiscard,
  onGo,
}: {
  dirty: readonly SectionId[];
  problems: readonly { section: SectionId; message: string }[];
  saving: boolean;
  error: string | null;
  justSaved: boolean;
  onSave: () => void;
  onDiscard: () => void;
  onGo: (id: SectionId) => void;
}) {
  const first = problems[0];
  return (
    <div className={`s-savebar${first || error ? " bad" : ""}${justSaved ? " done" : ""}`} role="region" aria-label="Unsaved changes">
      <div className="s-savebar-inner">
        <div className="s-savebar-text" role="status">
          {justSaved ? (
            <strong>Saved.</strong>
          ) : first ? (
            <>
              <strong>Fix before saving:</strong> {first.message}
              {problems.length > 1 && <span className="muted"> (and {problems.length - 1} more)</span>}{" "}
              <button type="button" className="link" onClick={() => onGo(first.section)}>
                Go to {sectionById(first.section)?.title}
              </button>
            </>
          ) : error ? (
            <>
              <strong>Not saved:</strong> {error}
            </>
          ) : (
            <>
              <strong>Unsaved changes</strong> <span className="muted">in {listTitles(dirty)}</span>
            </>
          )}
        </div>
        {dirty.length > 0 && (
          <div className="s-savebar-actions">
            <button type="button" disabled={saving} onClick={onDiscard}>
              Discard
            </button>
            <button type="button" className="primary" disabled={saving || problems.length > 0} onClick={onSave}>
              {saving ? "Saving…" : "Save changes"}
            </button>
          </div>
        )}
      </div>
    </div>
  );
}

function LeaveDialog({
  dirty,
  canSave,
  saving,
  onStay,
  onDiscard,
  onSave,
}: {
  dirty: readonly SectionId[];
  canSave: boolean;
  saving: boolean;
  onStay: () => void;
  onDiscard: () => void;
  onSave: () => void;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => ref.current?.showModal(), []);
  return (
    <dialog ref={ref} className="dialog" onClose={onStay} aria-labelledby="s-leave-title">
      <h2 id="s-leave-title">Save your changes?</h2>
      <p>
        You changed {listTitles(dirty)} and haven&apos;t saved.
        {!canSave && " Something there needs fixing before it can be saved."}
      </p>
      <div className="actions">
        <button type="button" className="actions-start" onClick={() => ref.current?.close()}>
          Keep editing
        </button>
        <button type="button" onClick={onDiscard}>
          Discard
        </button>
        {canSave && (
          <button type="button" className="primary" disabled={saving} onClick={onSave}>
            {saving ? "Saving…" : "Save and leave"}
          </button>
        )}
      </div>
    </dialog>
  );
}

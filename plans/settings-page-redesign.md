# Settings page redesign

## Goal

The Settings dialog had grown into one long modal: about fifteen sections stacked with no
navigation, paragraphs of grey hint text between every field, a single Save that covered
only some sections while others (bans, storage roots, updates, library folders, passkeys,
Deluge take-over) acted the moment you clicked, and the admin pages (Spaces, Users,
Import from Deluge) hidden as footer links. The user asked for a major makeover and
overhaul ("kinda confusing and poorly designed").

## Environment / context

- Code: `web/src/settings/` (new), section components in `web/src/components/*Section.tsx`,
  `Updates.tsx`, `LibrarySection.tsx`; routes in `web/src/auth/AuthGate.tsx`; toolbar in
  `web/src/App.tsx`; styles in `web/src/styles.css` plus `web/src/settings/settings.css`.
- Checks: in `web/`, `bun run typecheck`, `bun test`, `bun run build`.
- Screenshots: Playwright (Python) headless against `web/dist` with the API mocked, as in
  `plans/auth-pages-redesign.md`. Script in this session's scratchpad.

## Decisions already made (don't re-ask)

- Settings is a **page**, `/settings/<section>`, like Rules and Account: deep-linkable,
  back button works, room to breathe. The toolbar button navigates there.
- **Two panes**: a grouped nav (Server / This space / This device / Administration) with a
  "Find a setting" filter, and one section at a time. On a phone the nav is its own screen
  and a section has "← Settings".
- **One clear save model**: fields of the instance `Settings` object are a draft; a sticky
  bar appears only when something changed, naming the sections, with Discard and Save
  (Ctrl/Cmd+S too). Nav items show a dot for unsaved sections. Leaving with unsaved changes
  asks first (in-app) or warns (reload/close). Everything else (bans, folders, passkeys,
  updates, storage roots, theme) acts immediately with its own buttons, as before.
- Validation runs over the whole draft (group names, client identity) and the save bar
  links to the section with the problem, since hidden sections can't use HTML `required`.
- Setting rows: label and one-line description on the left, control on the right; stacked
  on a phone. Long explanations shortened, and moved under the control they explain.
- Rate inputs take KiB/s or MiB/s (unit picker); empty means unlimited/off, shown as the
  placeholder ("Unlimited", "Automatic", "Off").
- Enumerations with consequences (encryption, file names, dedupe, theme) are radio cards
  with a description each, not selects with long option text.
- Theme moves into Settings → Appearance too (the toolbar picker stays).
- No `title=` tooltips (user rule).

## Plan / steps

All done (2026-11-14). Layout of the code:

- `web/src/settings/model.ts`: section registry (id, group, summary, search keywords, which
  `Settings` fields it owns, who sees it), `dirtySections`, `draftProblems` (mirrors the
  daemon's `settings::validate`), nav filter, rate units, label parsing.
- `web/src/settings/controls.tsx`: `SettingCard`, `SettingRow`, `AmountInput`, `RateInput`,
  `ToggleRow`, `ChoiceCards`, `Note`, `Empty`.
- `web/src/settings/ServerSections.tsx`: Transfers, Seeding, Groups, Network, Files, Library.
- `web/src/settings/OtherSections.tsx`: Banned peers, Deluge, Appearance.
- `web/src/settings/SettingsPage.tsx`: layout, routing, save bar, leave dialog, kept draft.
- Existing section components (`ClientIdentitySection`, `TlsSection`, `Updates`,
  `TrackerPasskeysSection`, `DelugeTakeOverSection`, `StorageRootsSection`,
  `LibrarySection`) now render settings cards instead of fieldsets.

## Findings / gotchas

- The daemon rejects a label in two groups (`settings::validate`), so there is no "first
  group wins"; the page says so and checks it before saving, along with blank or repeated
  group names and peer port = DHT port.
- Plain substring search made "port" match "import"; the nav filter matches word starts.
- `useUpdateStatus` returned null both while loading and when the daemon has no updater;
  a module flag now tells them apart, so Updates says which.
- The phone layout first stacked every row (label, description, control): far too tall.
  Label and control now share a line, and the description wraps under both.
- Placeholders that look like values ("TV", a list of share paths) read as saved data next
  to an empty field; they now say what goes there.
- Bug in the old Groups editor: the labels input re-joined its parsed value on every
  keystroke, so typing a comma was swallowed (`"a,"` → `["a"]` → `"a"`). The new labels
  input keeps its own text.
- A desktop build for local use needs `cargo build --release -p apitorrent-desktop
  --features tauri/custom-protocol`. Without it the window points at the Vite dev server,
  and a release build shows a black window. That happened with the 2:40 AM PST build.
  Copy the exe (don't link it) to `target/desktop-app/run/`, which is what runs locally.
- Number boxes have one fixed width (12.5rem, 10rem on a phone), units included, and
  left-aligned text, so their edges line up from row to row (asked for after the first look).

## Progress log

- [x] Plan
- [x] Controls, registry, logic, tests (`web/src/settings/settings.test.tsx`)
- [x] Page, routing, save bar, guard
- [x] Sections ported
- [x] Wiring, dialog removed, README and architecture references updated
- [x] Checks (typecheck, 94 tests, build); 37 headless shots (desk/phone, light/dark,
  admin/non-admin) with no overflow, clipping or console errors; 25 scripted behaviour
  checks pass (save, discard, Ctrl+S, validation, kept draft, leave dialog, phone nav,
  read-only). Scripts: `shoot_settings.py`, `behave_settings.py`, `mocks.py` in the
  session scratchpad (API mocked, served from `web/dist`).
- [x] The user's first look in the app: "much better". Asked for left-aligned inputs (`5c0234a`).
- [x] Local app rebuilt at `5c0234a` with `custom-protocol` and running from
  `target/desktop-app/run/` since 2:11 PM PST (22:11Z). The page loaded with no errors.
- [x] Pushed `master` to `91ad588` and deployed it to pokol with the user's yes
  (2026-11-14 4:49 PM PST, 00:49Z). Built on sentinel from a clean worktree at that
  commit (`rust:1-bookworm`, `embedded-ui`), so other threads' uncommitted work stayed out.
  pokol serves the new CSS, and `https://apitorrent.isozilla.com/settings` returns 200.

## Open questions for the user

(none yet)

## Things not to do

- Don't put immediate-action sections behind the Save button: they call their own APIs.
- Don't rely on HTML `required` for fields in sections that aren't rendered.

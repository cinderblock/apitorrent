# Bulk select and edit

## Goal

From the user (2026-11-20): "Do we also have a bulk select/edit mode? probably worth a
thread just for that..."

Let someone select many torrents and change their settings together (label, transfer
limits, seed limits, trackers, …), with mixed values shown honestly, and make selection
itself discoverable with a mouse, a keyboard and a finger.

## Environment / context

- Web UI: `web/` (React + Vite, Bun). Selection state lives in `web/src/App.tsx`
  (`selected`, `focus`); the wide list is `components/TorrentTable.tsx`, the phone list is
  `components/TorrentCards.tsx`.
- Per-torrent editors are in `components/DetailPane.tsx`: `LabelEditor`, `LimitsEditor`,
  `SeedLimitsEditor` (with `SeedLimitsFields.tsx`), `TrackersEditor`, the files tab.
- Server routes: `crates/apitorrent-server/src/lib.rs` (~line 229). All torrent writes are
  per id: `PUT /torrents/{id}/label|limits|seed-limits|trackers|unwanted-policy`,
  `POST /torrents/{id}/pause|resume|queue`, `DELETE /torrents/{id}`.
- Space-wide tracker passkey replace already exists: `POST /tracker-passkeys/replace`.

## What exists today (master @ 9adb71a)

- Wide table: click, ctrl/cmd-click, shift-click range, Ctrl+A, Escape / click below rows
  clears. No checkbox column: nothing on screen says multi-select exists.
- Phone cards: a checkbox per card, a bottom selection bar.
- Bulk actions: Pause/Resume, Remove, queue moves. Each fires one request per torrent with
  `void`, so a failure on any of them is silent.
- `TorrentSummary` (what the list holds) carries `label` but not limits or seed limits;
  those are only in `TorrentDetail`. So mixed limit values can't be shown from list data.
- **The server has no recheck and no move-storage at all** (no route, nothing in the
  engine). They aren't a bulk question; they're missing features.

## Proposal

### 1. Edit dialog for the selection

An **Edit…** button next to Pause/Resume and Remove (and in the phone selection bar)
opens a dialog titled "Edit 37 torrents". One section per setting:

| Section | Control | Mixed state |
| --- | --- | --- |
| Label | text input with the existing labels as suggestions | placeholder "Mixed (5 labels)"; untouched = keep |
| Transfer limits | down / up KiB/s, max connections | each field "Mixed" placeholder; only fields you type in are applied |
| Seed limits | Keep / Follow group or instance / Own limits (SeedLimitsFields) | "Mixed" when torrents differ |
| Trackers | Add URL(s) to every torrent; remove a URL from every torrent that has it | list of URLs in the selection with "in 12 of 37" counts |
| Unwanted files | the unwanted-policy choice | "Mixed" |

Each section shows a "changed" marker once touched, and **Apply** only sends touched
fields. A field's reset (↺) returns it to "keep". After applying, the dialog shows a
result: "Updated 37" or "Updated 35, 2 failed: <name>: <error>", with failed rows
reselectable.

Not in bulk edit: file priorities (files differ per torrent), queue (already there).

### 2. Server: one bulk endpoint (recommended) vs. a client loop

Recommended: **`PATCH /api/v1/torrents`** with `{ ids, label?, limits?, seedLimits?,
trackers?: { add, remove }, unwantedPolicy? }` where every field is optional and limits
are per-field (`downloadRate?`, `uploadRate?`, `maxConnections?`). Answers with a per-id
result list. Plus **`POST /api/v1/torrents/settings`** `{ ids }` returning each torrent's
editable settings (own limits, own seed limits, trackers, unwanted policy), so the dialog
can show mixed values in one request.

Why not loop over the per-id API from the client: 4–5 requests × hundreds of torrents,
each a separate permission check and sync broadcast; partial-field limit edits would need
a read-modify-write per torrent from the browser (racy); and plugins / the REST API would
have no way to do the same thing. A loop would be fine for a first cut but gets thrown
away.

Pause/resume/remove/queue keep their per-id routes for now, but get the same result
reporting (no more silent `void`).

### 3. Discoverable selection

- Wide table: a narrow **checkbox column** at the left for anyone who may write. Clicking
  a box toggles that row without touching the rest (like ctrl-click); the header box
  selects / clears all visible rows and shows indeterminate when some are selected.
  Row click behaviour stays as it is.
- A selection count in the toolbar ("37 selected · Clear") so the state is always visible.
- Phone: the cards already have checkboxes and a bar; add Edit… to the bar, and a
  "Select all" next to "Clear N selected".

## Decisions already made (don't re-ask)

User answers, 2026-11-20:

- **Dialog** for the edit surface (matches Add and Remove).
- **Server bulk endpoints**, not a client loop.
- v1 scope: label, transfer limits, seed limits, **trackers add/remove**, **tracker
  find/replace**, **unwanted-files policy**, and **recheck + move storage** (both new
  engine features; built in this thread, as later phases).

## Open questions for the user

1. Checkbox style for the wide table: (A) always-on column (recommended), (B) select-mode
   toggle, (C) no checkboxes, only a visible count. User asked for a visual comparison;
   mockup shown in the thread 2026-11-20, waiting on the pick.

## Plan / steps

1. [x] User answers the first four questions
2. [ ] Server: types in `apitorrent-types`, `PATCH /torrents`, `POST /torrents/settings`,
       tests; regenerate TS bindings
3. [ ] Web: `api.bulkEdit`, `api.bulkSettings`; `BulkEdit.tsx` dialog with mixed-value
       logic in a tested pure module
4. [ ] Web: checkbox column + header select-all; toolbar count; phone bar Edit… / Select all
5. [ ] Result reporting for bulk pause/resume/remove/queue
6. [ ] Typecheck, tests, cargo test/clippy, headless browser check (wide + phone)
7. [ ] Recheck: engine + `POST /torrents/{id}/recheck` + bulk; UI button
8. [ ] Move storage: engine + endpoint + bulk; UI (destination picker within storage roots)
9. [ ] README / docs for the new endpoints

## Findings / gotchas

- Bulk actions today swallow errors (`void api.pause(id)` in `App.tsx`).

## Things not to do

- No `title=` tooltips (use `Tip`); browser checks headless only.

## Progress log

- [x] 2026-11-20: surveyed UI and API, wrote this proposal

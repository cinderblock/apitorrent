# Torrent details: closable panel, separate windows, column drag

## Goal

From the user's report (2026-11-20):

1. The bottom details panel can't be closed, and a selected torrent can't be deselected.
2. A per-device option to show details in their own window instead of the bottom panel.
   Click selects, double-click opens a window. Several torrent windows at once is fine,
   and a torrent window outlives the main window.
3. The queue position ("#") column is noise for finished torrents: hidden by default.
4. Drag table headers to reorder columns.
5. Bulk select/edit: its own thread (multi-select with pause/resume/remove/queue exists;
   no bulk editing of label, limits, etc.).

## Decisions

- Details mode is a per-device preference (localStorage, like theme and columns), set in
  Settings → This device → Appearance, with Panel the default.
- Double-click always opens a window, whatever the mode. The mode only decides whether the
  bottom panel shows for the focused selection.
- Window route is `/torrent/<id>` (space-prefixed like every route). Browser: `window.open`
  with a per-torrent name, so a second double-click focuses the same window. Desktop: a
  Tauri command builds a `torrent-<id>` webview window (focus it if it exists).
- Main desktop window already hides to the tray on close, so torrent windows stay open.
- Queue position matters for seeding torrents only when a seed or total active limit is
  set, so the column stays available but is hidden by default. Saved column prefs are
  migrated once (v1 → v2) so the column gets hidden for existing browsers too.

## Progress

- [ ] Panel: close button, Escape and empty-area click clear selection; panel shows only
      for a focused torrent that is still selected
- [ ] Details preference + Appearance card
- [ ] `/torrent/<id>` page
- [ ] Desktop: `torrent_window_open` command, capability for `torrent-*` windows
- [ ] "#" hidden by default, prefs migration
- [ ] Header drag reorder (`placeColumn` + tests)
- [ ] typecheck, tests, cargo check/clippy for desktop
- [ ] Bulk edit thread launched

## Findings

- Desktop main window: `on_window_event` hides `main` on close (tray app), so nothing
  needed for "close main, keep torrent window".

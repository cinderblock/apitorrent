# Deluge import: leftover .torrent files

## Goal

Some torrents imported from pokol's Deluge showed today's date as "Added". They aren't
torrents Deluge has: they're `.torrent` files left in `state/` with no entry in
`torrents.state` or `torrents.fastresume`. The import reads every `.torrent` there, and
with no resume data it fell back to "now".

Fix: give such torrents the `.torrent` file's modified time as their added time, import
them ticked but paused, and show where they came from (in the wizard and on the imported
torrent). Correct the 8 already imported on pokol.

## Environment / context

- pokol: `/var/lib/deluged/state` has 672 `.torrent` files, Deluge tracks 666 (664 with
  resume data). 8 leftovers, all *Suits* seasons S02–S09, files dated
  2026-05-14 12:43 AM PDT (07:43Z). `/var/lib/delugex`: none.
- pokol's apitorrent: `/var/lib/apitorrent/apitorrent.db` (schema 15), service
  `apitorrentd`, runs as `debian-deluged`. No `sqlite3` CLI; use `python3 -I` + `sqlite3`.
  The 8 are in space 1, paused, `imported.taken_over_at` NULL, `added_at` ≈
  2026-11-20 11:31 AM PST.
- Leftover ids and file mtimes (seconds):
  `04af0270… 1778744594`, `19aa5ee2… 1778744584`, `203cb9cf… 1778744616`,
  `45964f89… 1778744616`, `515e11e2… 1778744602`, `6e3192d1… 1778744618`,
  `7a8d47ce… 1778744601`, `a872fc2c… 1778744618` (full ids in the DB query below).

## Decisions already made (don't re-ask)

- Added time for a torrent without resume data: the `.torrent` file's mtime.
- Leftovers: ticked by default, imported paused (user, 2026-11-20).
- Provenance must be visible in the UI for torrents imported this way.
- Fix the 8 already on pokol: yes.

## Plan / steps

1. Deluge reader: `leftover` flag + mtime fallback for `added_at`.
2. Import: leftovers paused, recorded as leftover (`imported.leftover`, migration 16),
   nothing to take over (Deluge never had them).
3. Types/UI: preview shows "Not in Deluge's list"; detail pane shows the provenance.
4. Tests, commit.
5. ✅ pokol `added_at` fixed (2026-11-20 1:40 PM PST, 21:40Z): stopped `apitorrentd`,
   backed up the DB to `/var/lib/apitorrent/backup-2026-11-20/apitorrent.db`, set the 8
   rows' `added_at` to their file mtimes, restarted (672 restored).
6. ✅ Leftover marking (2026-11-20 5:19 PM PST, 01:19Z Nov 21): another thread had
   deployed `105a06d` (schema 16) to pokol. Stopped `apitorrentd`, backed up the DB to
   `/var/lib/apitorrent/backup-2026-11-20/apitorrent-before-leftover.db`, set
   `imported.leftover = 1` and `taken_over_at` on the 8, restarted (672 restored).

## Findings / gotchas

- `DelugeClient::pause` skips ids Deluge doesn't have, so take-over of a leftover is
  harmless; but leaving `taken_over_at` NULL shows a misleading "Still in Deluge" banner.
- Editing pokol's DB needs the daemon stopped (it holds rows in memory).

## Progress log

- [x] Diagnosis (2026-11-20)
- [x] Code: reader, import, migration 16, wizard chip + Tip, list chip, detail rows,
  tests (Rust unit + lifecycle, web). Landed inside `d658044` (another thread's commit
  swept up the staged files; the message doesn't mention it).
- [x] pokol `added_at` fix
- [x] pokol leftover marking (2026-11-20 5:19 PM PST)

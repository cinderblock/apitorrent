# Web UI review in a headless browser

## Goal

Most of the web UI had never been looked at in a browser, including the newest pages
(Spaces, Users, invitations, enrollment, the space switcher, the Plugins page for space
admins, the update button). Screenshot every page and dialog headless, at three sizes, in
both themes, as three kinds of user; fix what's wrong in `web/`.

## Environment / context

- Worktree: `C:\Users\camer\.t3\worktrees\apitorrent\t3-1148e5fc`, branch `t3/1148e5fc`
  (from master `e2bbdc1`).
- Daemon: on `127.0.0.1:8811`, peer port `127.0.0.1:16881`, `--no-dht --no-lsd --no-utp`,
  `--passwords fallback`, data and downloads in the scratchpad.
- Screenshots: Playwright for Python, headless Chromium, 1× scale.
- Checks: in `web/`, `bun run typecheck`, `bun test`, `bun run build`.

Scratchpad: `C:\Users\camer\AppData\Local\Temp\claude\0f0f91baa1a22449\eca01308-68d2-4fcd-bdd7-73eece82eac5\scratchpad`

- `scripts\mktorrents.py` makes five small private `.torrent` files (tracker
  `127.0.0.1:1`) and writes the content of two so they check complete and seed.
- `scripts\seed.py` sets up a fresh daemon: password admin `admin` ("Avery Admin"), spaces
  `tv` and `photos` (plus `default`), `sam` (admin in tv, viewer in photos), `vic` (instance
  viewer, photos), two pending invitations, torrents in each space. Writes `creds.json`.
- `scripts\shoot.py creds.json OUTDIR` shoots every scene for roles admin, sam, vic and anon
  (`--setup` for the setup page) at desk 1280×800, wide 1920×1080 and phone 390×844, light
  and dark, and writes `report.json` (sideways scroll, clipped text, wrapped buttons,
  off-screen elements, console errors). The update button is mocked as "0.2.0 available".
  `--dist DIR` serves pages and assets from a local build while the API stays on the
  daemon; `--only REGEX` limits scenes.
- `before\` (198 shots + 6 setup), `after\` (same scenes), `check*\` (quick checks).
- `web-master\web\dist`: the untouched master UI build the before set was served from.
- `bin\apitorrentd.exe`: copy of the main checkout's debug daemon (see gotchas).

## Decisions already made (don't re-ask)

- Headless only; nothing in the foreground.
- No public swarms: only small local `.torrent` files.
- Fix UI issues in `web/`; bigger non-UI problems go to `ISSUES.md`.
- The toolbar shrinks the search field first (ISSUES.md entry, now removed).
- No `title=` tooltips anywhere (user's tooltips rule): information goes inline.

## Plan / steps

1. ~~Daemon on a free loopback port, seeded~~
2. ~~Screenshot script~~
3. ~~Before set, review~~
4. ~~Fixes~~ (commits below)
5. ~~After set from the daemon serving `web/dist`~~ (204 shots + 6 setup, no probe flags)
6. ~~Final checks, stop daemon and browsers~~. Done 2026-11-10 about 4:50 PM PST (00:50Z).

## Findings and fixes

Key before/after pairs (same file name in `before\` and `after\`):

- `admin-main-desk-light.png`: toolbar 1477 px wide at 1280 → one row, no sideways scroll.
- `sam-tv-main-desk-light.png`: same for a space admin.
- `admin-add-phone-light.png`, `admin-settings-phone-light.png`: phone dialogs overflowed.
- `admin-plugins-desk-light.png`: empty state outside the column.
- `admin-users-invite-desk-dark.png`: full-width role select, loose checkbox rows.
- `admin-rules-new-desk-dark.png`: native grey selects in dark mode.
- `sam-photos-detail-desk-light.png`, `vic-settings-desk-dark.png`: viewer saw editors.
- `admin-spaces-edit-phone-light.png`: Spaces row buttons wrapped their labels.

Fixed:

1. **Toolbar at 1280 px** overflowed ~200 px; labels wrapped. Labels never wrap now; the
   search box shrinks first (16rem → 7rem); between 721 and 1500 px the total rates sit in
   the status bar; the update button reads "Update". Worst case (admin, three spaces, update
   pending) fits one row; the rates/status/account group wraps as a unit only as a fallback.
   `1b5047c`, `de3d13a`.
2. **Phone dialogs and forms overflowed** (`.field` grid items kept their inputs' natural
   width; fieldsets grew to their content; dialog footers squeezed five buttons). `1b5047c`.
3. **Native selects** now match the inputs; **links** use the accent; **`code`** breaks only
   where it must; **Account inputs** fit their placeholders; **Plugins empty state** sits in
   the column. `1b5047c`.
4. **Hover-only `title=`** on times, the theme picker, the space switcher and the plugin
   badge: UTC is now inline as elsewhere. `df5b617`.
5. **Wording**: details showed raw "downloading"; table said "Metadata…", cards "Fetching
   metadata"; a 3-day invitation read "expires in 2 days". `2ab4b89`.
6. **Viewers** saw Add, Pause/Resume, Remove, queue arrows, every details editor and an
   editable server Settings form (all refused by the daemon). Now a "Read only" badge,
   values as text, Settings read-only with a note for non-server-admins, Import only for
   space admins. `5d01157`.
7. **Invitation form** role selects 930 px wide; checkbox rows 43 px apart. `d2b85e8`.
9. **Search box floated mid-toolbar at 1920 px** (the spare width split between two auto
   margins). It now lives in the right-hand group, whose flex basis is its smallest size,
   so the search still shrinks before anything wraps. `060fce9`.
8. **Regression from fix 1, caught in the after set**: the toolbar's no-wrap rule reached the
   update dialog (rendered inside the toolbar), so it scrolled sideways. `af2276a`. The
   probe now also reports `dialogOverflow`.

Looked at and fine: sign-in, setup, enrollment (all sizes and themes), Rules (empty and new
rule), Import, Remove (turns red with "delete files"), update dialog, space switcher, Spaces
page as server admin and as a space admin, Account, Users. No console errors or page errors
in any shot.

Judgement calls, settled 2026-11-11 (the user said to go ahead):

- On phones the toolbar took three rows. Settings, Rules, Plugins and the theme picker now
  sit in a "⋯" (More) menu (`web/src/components/ToolbarMore.tsx`), so it takes two.
- Release notes in the update dialog are rendered from Markdown by `web/src/markdown.tsx`,
  which builds React elements only (no HTML strings) and follows only http(s) links.
  Tests: `web/src/markdown.test.tsx`.
- The Users page role select has a visible "Server role" label again, in an inline
  label that keeps the row aligned.
- Checked with `shoot.py` (copied to this session's scratchpad, plus a `main-more`
  scene): no sideways scroll, clipping, wrapping or console errors at phone, desk and
  wide sizes. The first try opened the menu off the left edge (anchored right); it is
  anchored left now.

## Gotchas

- `/` is the user's *last used* space, server-side: use explicit `/s/default/` in scripts.
- `networkidle` never settles (sync WebSocket); wait for a selector instead.
- **Rebuilding `web/dist` while the daemon serves it swaps the UI under a running shoot.**
  Happened twice; both runs were discarded and redone. Use `--dist` for checks.
- Git Bash rewrites `/s/default/` arguments into `S:/default/`; use `MSYS_NO_PATHCONV=1`.
- The daemon's CSP blocks `page.add_style_tag`; use `bypass_csp=True` for experiments.
- `TaskStop` on `sh daemon.sh` killed the shell but left `apitorrentd.exe` running; start
  the daemon with `exec` (or stop it by PID).
- The T3 server restarted mid-run (2026-11-10 about 3:20 PM PST (23:20Z)), killing the
  daemon, the queued cargo build and the shoot; the data dir survived.
- The worktree's own daemon build sat queued behind other srm jobs and died in the restart.
  Only `web/` changed, so the shots use a copy of the main checkout's
  `target/debug/apitorrentd.exe` (built 2:31 PM PST, after the last `crates/` commit
  `a0e7491`), run from the scratchpad so no other build is blocked.
- Colour fringes on the queue arrows in 1× shots are subpixel antialiasing; at 4× they are
  plain text colour.

## Progress log

- [x] Daemon running with UI, test data seeded
- [x] Before set (198 + 6 setup), reviewed
- [x] Fixes: `1b5047c`, `df5b617`, `2ab4b89`, `de3d13a`, `5d01157`, `d2b85e8`, `af2276a`
- [x] Fixes, last: `060fce9` (search beside the rates)
- [x] After set reviewed: 204 + 6 shots, 0 sideways scroll / wrapped / clipped / off-screen /
  dialog overflow / console errors (before: 26 sideways scroll, 30 wrapped, 14 off-screen)
- [x] `bun run typecheck`, `bun test` (56 pass), `bun run build` pass; daemon and browsers
  stopped

## Open questions for the user

None blocking. The three "Not changed" items above are the user's call.

## Things not to do

- Don't take the foreground (no headed browsers).
- Don't join public swarms; don't push; don't touch servers.
- No `title=` tooltips: put the information inline.
- Don't rebuild `web/dist` during a full shoot.

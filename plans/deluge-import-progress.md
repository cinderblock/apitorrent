# Deluge import: never block the UI, show progress

## Goal

Importing from Deluge took ~60 s and the Import dialog sat on "Importing…" the whole
time, as a modal that blocks the rest of the app. The UI must never block like that:
long work runs in the background on the server, the UI shows a real progress bar, and
the user can keep using the app (close the dialog, come back to it) while it runs.

## Decisions already made (don't re-ask)

- Long Deluge work (import, move-over/take-over) becomes a **server-side job**: the
  POST starts it and returns at once; the job reports progress; the UI polls it.
  Server-side so it survives closing the dialog or reloading the page.
- Keep the existing blocking endpoints working for API clients and tests; the web UI
  moves to the job endpoints.
- Also make the import itself faster (it was doing avoidable per-torrent work).

## Environment / context

- Work happens in the worktree `C:/Users/camer/.t3/worktrees/apitorrent/deluge-import-progress`
  (branch `deluge-import-progress`, from `master` at `f98ce37`): other threads had
  uncommitted edits to `apitorrent-core/src/lib.rs` and `apitorrent-types/src/lib.rs` in
  the shared checkout.
- Local Windows builds share the main checkout's target dir
  (`CARGO_TARGET_DIR=C:/Users/camer/git/apitorrent/target`); clippy and the full
  core/server tests run on sentinel via `rbuild`.

## Findings

- Per imported torrent, `Core::import_deluge` (crates/apitorrent-core/src/lib.rs) did,
  serially: a `held_path_for` probe (2× `create_dir_all`, write, rename, remove a file:
  slow on Windows with Defender), a DB insert, a seed-state save and an imported-row
  save — 3 separate SQLite commits with `synchronous=FULL`, each its own fsync.
- `deluge_import::plan` (parses every .torrent) ran directly on the async runtime.
- `take_over` has the same shape: per torrent `mark_taken_over` + `resume`, each with
  its own DB commit.
- The web dialog is `showModal()`, so while it waits the whole app is blocked.
- Measured on Windows (`crates/apitorrent-core/tests/deluge_import_many.rs`, 300 small
  torrents, paused): **6.06 s before, 0.45 s after** (batching 64 rows per transaction +
  one held-dir probe per save path + planning off the runtime). So the reported 60 s is
  roughly 3,000 torrents, and should now be ~5 s.
- Unrelated bug found on the way: `f98ce37` put literal tab characters in the
  "which is now" tooltip (`D:	orrents	v` → `D:<tab>orrents<tab>v`). Fixed in its
  own commit.

## Plan / steps

1. Measure: a test importing a few hundred synthetic torrents, timed.
2. Speed: probe the held path once per distinct save path; write all rows in one
   transaction; `plan` on a blocking thread.
3. Jobs: `Core` keeps a job registry (per space, id, kind, progress
   `{phase, done, total, current}`, outcome). Import and take-over report progress
   into it. Routes: start (202 + job), get one, list the space's.
4. Web: Import/Move-over start a job; the dialog shows a progress bar with "Keep
   working" (closes the dialog, job continues); a status-bar indicator shows running
   jobs with a bar and reopens the dialog; finished job shows the report.
5. Tests (core, server routes, web), README/docs, commit, deploy note.

## Progress log

- [x] 1. Measure (6.06 s / 300 torrents on Windows)
- [x] 2. Speed (0.45 s / 300)
- [x] 3. Jobs: `crates/apitorrent-core/src/jobs.rs`, `Core::start_import_deluge`,
      `start_take_over`, `job(s)`; routes `POST …/import/deluge/jobs`,
      `POST …/import/deluge/take-over/jobs`, `GET …/jobs`, `GET …/jobs/{id}`;
      `CoreError::Busy` → 409
- [x] 4. Web: `web/src/jobs.ts` (poller), `components/Jobs.tsx` (tray + global dialog),
      `components/JobProgress.tsx`, `components/importDialog.ts`; wizard and Settings →
      Deluge take-over use jobs
- [x] 5. Clippy + core/server/types tests on sentinel pass; web typecheck, tests, build
      pass. Committed on branch `deluge-import-progress`, rebased onto master `d87361f` (`91612c7`,
      `0401a82`, then the web commit). After the rebase the new "In Deluge" indicator needed
      setting in the batched path too (fixed; `deluge_take_over` caught it). All green again.
- [x] 6. Landed on local master (fast-forward) after rebasing onto `2d6feff`, the
      import-options commit: the job's planner takes `req.options` (cloned into the
      blocking task) and `stop_at_ratio` in place of `importSettings`. Clippy and tests
      on sentinel, web typecheck/121 tests/build all green. Commits `583798d`, `4de591c`,
      `25dae41`. Not pushed.
- [ ] 7. Try it by hand against a real Deluge (user), then deploy to pokol

## Things not to do

- Don't hold the import's DB transaction across engine calls (`spawn_entry`).

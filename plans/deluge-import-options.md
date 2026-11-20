# Deluge import: show every option that comes over, and let the user drop or convert it

## Goal

Asked 2026-11-20: during an import from Deluge, the user should see every option the import
carries over, with a way to leave some out or convert them, so it is clear what is coming
over. Today the wizard shows a torrent table, one "also use Deluge's limits" box and one
"add paused" box; labels, totals, limits, file priorities, edited trackers, stop-at-ratio
and the client-identity rules all come over silently, and what can't come over is only in
the CLI report.

## Environment / context

- Repo `C:\Users\camer\git\apitorrent`, `master`. Builds on `plans/deluge-coexistence.md`
  (step 5 wizard, step 7 identity rules).
- Server: `crates/apitorrent-core/src/deluge_import.rs` (`plan`, `report`, `run_offline`),
  `Core::preview_deluge` / `Core::import_deluge` in `crates/apitorrent-core/src/lib.rs`.
  Types in `crates/apitorrent-types/src/lib.rs` (ts-rs → `web/src/api/generated`).
  CLI `crates/apitorrentd/src/import.rs`.
- Web: `web/src/components/ImportDeluge.tsx` (wizard), `delugeImport.ts` (tested logic).

## Decisions (made here; the user can overrule)

- **One options model, `DelugeImportOptions`, for API and CLI**, on `ImportDelugeRequest`
  with `serde(default)`; its default is today's behaviour, so old callers don't change.
- Per-torrent options, each on by default: labels (keep, rename or drop each one), queue
  order, added dates, totals and seeding time, skipped files, Deluge's edited tracker
  lists, per-torrent speed/connection limits, per-torrent stop-at-ratio.
- Client identity for private trackers: Deluge (default, versions from the daemon),
  apitorrent (add no rules), qBittorrent or Transmission; hosts can be unticked one by one.
- Deluge's global settings: one tick per setting (download rate, upload rate, connections,
  active slots, slow torrents, stop at ratio), replacing `importSettings: bool`. CLI
  `--settings` still takes them all; `--setting NAME` takes one.
- Always carried, shown but not optional: files where Deluge left them, progress, renamed
  files (they are how the files are found).
- What can't come over (move completed, sequential, super-seeding, upload slots, label
  options…) is listed in the wizard, grouped with counts.
- Counts in the wizard follow the ticked torrents.

## Plan / steps

1. [x] Types: `DelugeImportOptions`, `DelugeSettingsPick`, `ImportIdentity`,
       `LabelRename`; preview additions (per torrent: private, limits, stop-at-ratio,
       skipped/renamed files, totals, seeding time, edited trackers, not carried; preview:
       Deluge settings, label options, Deluge identity, hosts already with a rule).
2. [x] Core: apply options in `plan`, settings, identity; `report` gets an OPTIONS section.
3. [x] CLI flags.
4. [x] Web: "What comes over" section in the wizard; logic + tests in `delugeImport.ts`.
5. [x] Tests, clippy, typecheck; docs (architecture Deluge section, README if it lists flags).
6. [ ] ← Click through the wizard against a real Deluge dir (with the rest of
       `plans/deluge-coexistence.md` step 11, on pokol).

## Findings / gotchas

- The preview asks Deluge's daemon for its versions (for the identity choice's label),
  capped at 3 s so a hung daemon doesn't stall the preview; the import asks again.
- `importSettings: bool` is gone from `ImportDelugeRequest`, replaced by
  `options.settings` (an old client sending it gets no settings taken).
- Deluge copies its global stop-at-ratio into every torrent; `plan` gives a torrent its
  own seed limits only when it differs (or always, in a non-default space taking the
  setting, since spaces have no seed-limit setting).

## Progress log

- [x] 2026-11-20: plan written.
- [x] 2026-11-20: built. Rust unit tests (`deluge_import`: defaults, every option off,
      label renames, settings one by one), offline and lifecycle Deluge tests, clippy,
      web typecheck and `delugeImport.test.ts` pass. The options panel was rendered to
      static HTML with sample data and looked at; not yet clicked through in the running
      app.

## Open questions for the user

(none yet)

## Things not to do

- Don't make renamed files optional: dropping them would point torrents at missing files.

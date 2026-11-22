# Files in order, sequential files, and per-file move when complete

## Goal

Asked 2026-11-21. Three related features:

1. **Files in order** (layer 1): a strong, not strict, preference for downloading a
   torrent's files in a chosen order. Later files still get data (from peers that lack the
   earlier files' pieces, or when the earlier files have nothing left to start), but the
   first unfinished file in the order wins whenever it can.
2. **Sequential file** (layer 2): one file downloaded strictly front to back, so a video can
   be watched while it downloads.
3. **Move each file when complete**: Deluge's "move completed", but per file. A finished
   file moves (for example to a public directory) as soon as it's done, before its
   neighbours are.

How "first" is defined is its own problem: by name usually, maybe by size, ideally by
arbitrary rules.

## Environment / context

- Picker: `crates/apitorrent-engine/src/picker.rs`. Priorities 0 to 7 per piece (from file
  priorities: a piece takes the highest priority of the files it overlaps,
  `piece_priorities` in `torrent.rs`). New pieces come from `order`, bucketed by
  `(priority, availability)`. A `sequential` flag (whole torrent, index order) exists in the
  picker but nothing sets it: no API, no UI, no persistence.
- File placement: `crates/apitorrent-engine/src/storage.rs`, `Placement::{Save, Held,
  Parts}`, moved on a disk thread by `TorrentActor::place_files` / `start_moves` /
  `on_moved` (`torrent.rs`). Per-file paths under the save path come from
  `AddTorrent::file_paths`.
- No whole-torrent "move storage" exists yet.
- File priorities: `TorrentFile.priority`, `SetFilePrioritiesRequest` in
  `crates/apitorrent-types/src/lib.rs`; persisted in `torrents.file_priorities` (core
  `db.rs`); UI in `web/src/components/DetailPane.tsx`.
- Deluge import lists "move completed" and "sequential download" as not carried over
  (`crates/apitorrent-deluge/src/lib.rs`). These features should let both come over.
- A swarm can be held by several entries (spaces), sharing one storage (`plans/spaces.md`).

## Decisions already made (don't re-ask)

- **Explicit priority beats file order.** File order only breaks ties between pieces of the
  same priority; a file set to High still goes before an earlier Normal one. *(mine)*
- **Order rules are a sort key, pluggable.** Built in: natural name order (default), size
  ascending, size descending, the torrent's own order, and a manual order. Arbitrary rules
  (plugins) can supply a key later without engine changes: the engine only ever gets a
  rank per file. *(mine; the user called the rule question a separate problem)*
- **Engine primitives, core policy**, as for the content library: the engine gets a rank
  per file, a sequential flag per file, a file-completed event and a move-one-file command;
  the core decides order, destinations and when to move. *(mine)*
- **A moved file keeps seeding from its new place.** Rename on one volume, copy + delete
  across volumes. *(user, 2026-11-21)*
- **Both timings:** "each file as it finishes" (default) and Deluge's "whole torrent when
  everything is done". Deluge's move-completed imports onto the latter. *(user)*
- **Destination layering: Settings default → label → torrent, plus per-file pattern
  rules** (`*.mkv` → `/public/video`). Each file keeps its relative path under the
  destination. *(user)*
- **Streaming: a player reading the file on disk, for now.** Browser playback with seeking
  (playhead steers the picker) is wanted later ("would be really sweet"): keep the engine
  API ready for a moving "needed from here" position, but don't build the endpoint yet.
  *(user)*
- **Several spaces holding one swarm:** the first entry's move settings win; other entries
  see the file wherever it is. *(mine; not asked separately)*

## Design (as built)

- **Layer 1:** the picker's key is `(priority, tier, availability)`; a piece's tier is its
  earliest file's place among the unfinished files of the order (8 places, the rest share
  the last; files move up as earlier ones finish). Rarest first within a file.
- **Layer 2:** a stream fetches its file's first 1 MiB and last 0.5% (2–16 MiB), then from
  the playhead to the end, then the start up to the playhead. The piece the reader waits
  for goes first among pieces in progress. Not built: requesting that piece from a second
  peer when the first is slow (needs request ages in the picker).
- **Move:** `Session::file_events` (after the last piece is written and flushed) drives the
  core's mover; `Storage::relocate` renames (or copies, flushes, deletes across volumes)
  under the file's write lock, so neighbouring boundary pieces wait. Moved paths are kept
  per entry in `torrents.file_options` (JSON) and handed back as
  `AddTorrent::file_locations`.

## Plan / steps

1. ✅ Settle the open questions with the user.
2. ✅ Picker (`d452bdd`).
3. ✅ Engine (`cf0b7d0`): relocate, file events, order and streams on the handle.
4. ✅ Types, DB (migration 17), core policy, server routes (`7391548`).
5. ✅ Deluge import mapping (`dc20978`): move completed → whole-torrent move; sequential →
   order "torrent" + every file sequential. Both are import options with wizard rows.
6. ✅ Web UI (`84d56f4`, by a delegated child).
7. ✅ README (`4ff80d6`).
8. Later (wanted, not started): browser playback with seeking (`set_stream(file, from)` and
   `SequentialFile.from` are the hooks); duplicate requests for the waited-for piece;
   add-time options (order / sequential in the Add dialog); editing a manual order in the
   UI; plugin-supplied order rules.

## Findings / gotchas

- On loopback every piece is requested almost at once, so files finish in "roughly" the
  order: small.txt (2 pieces) sometimes beat tail.bin (7) though tail was first. The
  loopback test checks only that the last file in the order finishes last; exact order is
  the picker tests' job.
- `tracing` macros bring a `display` function into scope: a local named `display`
  can't be indexed inside `tracing::warn!(… %display[i] …)`.
- Windows: `/srv/done` is not an absolute path; tests that need one use `C:\…` under
  `cfg(windows)`.
- A per-torrent destination must be inside the space's storage roots
  (`check_save_path`), else any member could make the daemon write anywhere. Instance and
  group settings are admin-only and not checked.
- The first pokol build failed: `apitorrentd`'s import CLI builds `DelugeImportOptions`
  too, and only the library crates had been checked. Fixed in `93993c4`. Check with
  plain `cargo clippy --all-targets` (default members), not just `-p` the touched crates.
- `discover::tests::{config_arguments, a_daemon_without_config_uses_its_users_home}` fail
  on Windows before this work too (ISSUES.md).

## Progress log

- [x] Survey picker, placement, types, DB, UI
- [x] Open questions answered
- [x] Picker
- [x] Engine move + events
- [x] Types/API/DB
- [x] Core policy + Deluge mapping
- [x] Web UI (`84d56f4`; manual order not editable in the UI yet)
- [x] Tests + README
- [x] Deployed `93993c4` to pokol (2026-11-21 9:01 PM PST, 05:01Z Nov 22), asked for by
  the user. Built on sentinel from a clean worktree (`embedded-ui,first-party-plugins`),
  installed with `install.sh --binary`. Database backed up first to
  `/var/lib/apitorrent/backup-2026-11-21/`; migration 17 ran, 672 torrents restored,
  `https://apitorrent.isozilla.com/` answers 200.
- [x] Desktop app rebuilt (`24db1b5`, `tauri/custom-protocol`) and copied to
  `target/desktop-app/run/apitorrent-desktop.exe` (9:00 PM PST); the old one is
  `.bak-20261121-2059`. The running copy needs a restart.
- [ ] Seen working in a browser against a running daemon (nobody has yet)

## Open questions for the user

(none)

## Things not to do

- Don't make file order strict: a peer that only has later files' pieces must still be used.
- Don't let a torrent's own move destination leave its space's storage roots.

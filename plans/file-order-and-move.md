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

## Design sketch

- **Layer 1:** the picker's key becomes `(priority, file rank, availability)`. Within one
  file, rarest first still spreads peers over the swarm. A piece spanning two files takes
  the better (lower) rank.
- **Layer 2:** a sequential file's pieces are picked in index order, after its first and
  last few pieces (container metadata: MP4 `moov`, MKV cues often sit at the end). The next
  needed piece may be requested from a second peer when the first is slow (time-critical,
  like endgame for one piece), so one slow peer can't stall playback.
- **Move:** the engine emits "file N complete" once every piece touching it is verified. The
  core moves it with a new placement (a file at its own path outside the save directory).
  Rename on one volume; copy, fsync, then delete across volumes. The file's I/O lock is held
  while it moves, so a neighbour's boundary piece waits instead of writing to the old place.
  Placement and the new path are persisted, so a restart finds it.

## Plan / steps

1. ✅ Settle the open questions with the user.
2. ✅ Picker (`d452bdd`): `set_file_order(Option<Vec<usize>>)`, `set_stream(file,
   Option<from_byte>)`, `take_completed_files()`. Key is `(priority, tier, availability)`,
   `TIERS = 8`. Streams: first 1 MiB, last 0.5% (2–16 MiB), playhead→end, start→playhead;
   the waited-for piece is first among partials.
3. ✅ Engine (`cf0b7d0`): `Storage::relocate` / `file_path`, `TorrentHandle::relocate_file`,
   `file_paths`, `set_file_order`, `set_stream`; `AddTorrent::{file_locations,
   file_order, streams}`; `Session::file_events()` (flushed before sending).
4. ⏳ **Current:** types + DB + core policy + server routes.
   - Layering like seed limits: `Settings` → `GroupSettings` (groups hold labels: that's
     the "label" layer) → torrent. Same for file order and move-when-complete.
   - Core keeps `Entry.locations` (relocated paths) and applies them in `entry_paths` and
     the library snapshot, so the content library, other spaces and remove-with-data see
     moved files where they are. Priority-0 files are left out of the order sent to the
     engine (else they hold an order place for ever); resync on priority change.
   - Mover: event-driven from `file_events`, plus a sweep at start, on settings change and
     on a lagged receiver. Rules: glob on the file's path relative to the save path, first
     match wins; a rule with an empty destination keeps matching files in place.
5. Deluge import: `move_completed(_path)` → torrent move setting, "whole torrent";
   `sequential_download` → file order "torrent" + every file sequential.
6. Web UI: details pane (order, per-file sequential toggle, location / move error, move
   setting), Settings and group defaults.
7. README.
8. Later (wanted): browser playback with seeking; the engine's `set_stream(file, from)` is
   the hook. Add-time options (order/sequential in the Add dialog). Plugin order rules.

## Findings / gotchas

- (none yet)

## Progress log

- [x] Survey picker, placement, types, DB, UI
- [ ] Open questions answered
- [ ] Picker
- [ ] Engine move + events
- [ ] Types/API/DB
- [ ] Core policy + Deluge mapping
- [ ] Web UI
- [ ] Tests + README

## Open questions for the user

1. Moved file: keep seeding from its new place (recommended), link/copy into the public dir
   and leave the original, or move it and stop seeding it?
2. Offer Deluge's "whole torrent when everything is done" as well as per file?
   (Recommended: both, per file the default.)
3. Where the destination is set: global default, then label, then torrent (recommended);
   and/or per-file pattern rules (`*.mkv` → `/public/video`)?
4. Watching while downloading: a player reading the file on disk only (recommended first),
   or also playback in the web UI with seeking, where the playhead steers the picker?
5. Several spaces holding one swarm share one copy of each file: whose move setting wins?
   (Recommended: the first entry's; others see the file where it is.)

## Things not to do

- Don't make file order strict: a peer that only has later files' pieces must still be used.

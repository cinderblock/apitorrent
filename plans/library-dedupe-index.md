# Content library: retroactive dedupe and library folders

Content-library gaps 3 and 4 from `plans/apitorrent-bootstrap.md` ("Content library gaps
(found 2026-11-11)"). Started 2026-11-11 (US Pacific).

## Goal

3. **Retroactive dedupe.** Identical complete files that different torrents downloaded
   separately are merged into hard links, reclaiming the space. A background,
   rate-limited pass; setting off / report only / on (default report only); reclaimable and
   reclaimed bytes in the API and the web UI (Settings → Library); an admin action
   "Deduplicate now".
4. **Files outside torrents.** Admin-chosen "library folders" are scanned (size, mtime;
   lazily v2 root + SHA-256) and their files become match candidates when a torrent is
   added. They are read-only sources. A torrent removed with "keep files" leaves its
   complete files as candidates when they are under a library folder.

## Environment / context

- Worktree `C:\Users\camer\.t3\worktrees\apitorrent\t3-53031008`, branch `t3/53031008`.
  Commit here, never push. The parent merges.
- Build only through `~/.srm/bin/srmctl run --slots 4 --low --label apitorrent-dedupe --
  cargo …` (Git Bash). C: is tight (64 GB free at start).
- Another agent is doing gaps 1, 2, 5 and 6 at the same time. Their work touches
  `library.rs`, `link_from_library`, and adds a DB migration. Expect a migration-number
  clash on merge (mine is 12).

## Decisions already made (don't re-ask)

- Dedupe only merges files of torrents (primary entries' files, complete, not checking).
  Library-folder files are never modified, renamed or replaced, so they're never a dedupe
  target, and never a keeper either (a later in-place edit of the folder file would change
  the torrent's file through the link).
- The keeper of a group is the copy with the most hard links already, then the oldest
  path. Every copy (keeper included) is marked `linked`, so the engine's copy-on-write
  protects both sides.
- A copy is only replaced when every one of its links is a known torrent file
  (link count == paths we know). Otherwise replacing it frees nothing (another space's
  placed file, or a user's own link, still holds it), so it's reported as not reclaimable
  and left alone.
- "Deduplicate now" applies merges in both "report only" and "on" modes (an explicit
  admin action). With dedupe "off" it's refused.
- Library folders and dedupe status are server-admin only (they show paths). The dedupe
  numbers are aggregates; no file names leave the server through them.
- Folder files are found by DB query at match time (indexed by size and root), not held in
  memory, so a large media folder costs no RAM.

## Plan / steps

1. Engine: `fileid` (volume, file index, link count; Windows via
   `GetFileInformationByHandle`), `Storage::replace_file` / `TorrentHandle::replace_file`
   and `mark_linked`.
2. Types: `DedupeMode`, `LibrarySettings.dedupe`, `LibrarySettings.folder_rescan_hours`,
   `LibraryStatus`, `DedupeStatus`, `LibraryFolder`, `AddLibraryFolderRequest`.
3. Core `dedupe` module: grouping/planning (pure), `swap` (verify then link + rename),
   failure-injection hook for crash tests.
4. Core `folders` module: walk, diff against DB, lazy hashing, matching for
   `link_from_library`.
5. DB migration 12 (`library_folders`, `library_files`) in `db/library.rs`.
6. Core wiring (`library_index.rs`): background worker, `dedupe_now`, folder API,
   keep-files retention on remove.
7. Server routes; web UI section; web tests.
8. Docs (`docs/architecture.md`), bootstrap plan ticks, full checks.

## Findings / gotchas

- Rust's `std::fs::File::open` on Windows shares read/write/delete, and `fs::rename` uses
  POSIX-semantics replace, so renaming a fresh hard link over a file the engine has open
  works. The engine's own `write_linked` already relies on this. `replace_file` still
  closes the engine's handle before and after, so later reads open the new file.
- A hard link shares the inode's mtime, so after a swap the replaced path's stamp is the
  keeper's; its fingerprint is rewritten with that stamp.
- The engine's writes to a file not marked linked don't take the per-file unshare lock,
  but every read and write takes the placement lock for reading, so `replace_file` takes
  the placement lock for writing (then the unshare lock: the order `write_linked` uses).
- The slow full-file comparison runs *outside* the engine lock (inside, it would stall
  seeding that file); the swap re-checks size and time inside it. The engine never writes
  a complete file, so what's left is an outside writer that keeps both size and time,
  which no comparison can rule out anyway.
- Test race: adding a folder wakes the walker, which wakes the hasher, so a test calling
  `hash_folders_now` may find the file already done (`left: 0, right: 1`). Tests assert
  the folder's `hashed_files` instead.
- `routes::Failure` has a private field: outside `routes.rs` use `Failure::from(e)`
  (`E0423 cannot initialize a tuple struct which contains private fields`).
- A 202 with no body breaks the web client's `request()` (it parses JSON for anything but
  204), so "rescan" answers 204.
- The build directory is shared (`~/.cargo/build/…`), not `target/` in the worktree. A
  cold engine test build took 4m41s through srmctl.

## Progress log

- [x] 1. Engine: `fileid`, `replace_file`, `mark_linked` (`0468d78`)
- [x] 2. Types: `library` module, `LibrarySettings.dedupe` / `folder_rescan_hours`
- [x] 3. Dedupe module: plan, verify, `link_over`, crash tests
- [x] 4. Folders module: walk, diff, matching
- [x] 5. DB migration 12 (`db/library.rs`)
- [x] 6. Core wiring (`library_index.rs`), keep-files retention, folder candidates in
  `link_from_library` (2–6 in `5e28d54`)
- [x] 7. Server routes, web UI, tests (`2088dce`)
- [x] 8. Docs: architecture, README, bootstrap plan ticks
- [x] Full checks 2026-11-11 (US Pacific): fmt, clippy `-D warnings`, `cargo test --locked --no-fail-fast` (0 failures), web typecheck, `bun test` (67 pass), build

## Not done (and why)

- Dedupe only merges files of torrents' primary entries. Other spaces' placed copies are
  usually hard links already; where they are copies (another volume) they can't be linked.
- Library-folder files are never deduplicated (against each other or torrents): they are
  read-only by design.
- The reflink fallback in a swap is used only where hard links fail, and isn't tested on
  its own beyond `reflink-copy` (CI's reflink volume covers the other reflink paths).

## Things not to do

- Don't run cargo directly; always through `srmctl`.
- Don't touch gaps 1, 2, 5, 6 (re-matching, in-progress sharing, bookends, "reused from").
- Never delete or write a library-folder file. Never replace a file without re-verifying
  it right before the swap.

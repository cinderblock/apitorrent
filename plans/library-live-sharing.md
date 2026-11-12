# Content library: live sharing, bookends, provenance

## Goal

Close content-library gaps 1, 2, 5 and 6 from `plans/apitorrent-bootstrap.md` ("Content
library gaps (found 2026-11-11)"):

1. **Re-match when a source completes.** A file completing in one torrent (or getting a
   fingerprint) is offered to other torrents that still need a file like it.
2. **Piece sharing between in-progress torrents.** A piece one torrent verifies fills the
   pieces of other torrents that hold the same bytes, without downloading them.
5. **Bookend pieces.** A v1 piece straddling a reused file and a file still downloading takes
   the reused file's bytes from disk and downloads only the rest.
6. **Visibility.** Per file: where it came from, how, and bytes reused. Per torrent and
   instance totals of bytes not downloaded thanks to the library.

Gaps 3 and 4 (retroactive dedupe; folders outside torrents) belong to another agent working
concurrently. Don't implement them; keep shared-struct changes additive.

## Environment / context

- Worktree: `C:\Users\camer\.t3\worktrees\apitorrent\t3-6786e3a1`, branch `t3/6786e3a1`,
  based on `master` at `d7d7f8d`. The parent merges the commits; never push.
- Builds through `~/.srm/bin/srmctl run --slots 4 --low --label apitorrent-library -- cargo …`
  (Git Bash). Never cargo directly. C: had 65 GB free at the start (2026-11-11).
- Generated TS types: `cargo test -p apitorrent-types` writes `web/src/api/generated`
  (`TS_RS_EXPORT_DIR` in `.cargo/config.toml`).

## Decisions already made (don't re-ask)

- **Engine primitives, core policy.** The engine gets three small, policy-free primitives;
  the core decides what to share:
  - `TorrentHandle::offer_piece(piece, bytes)`: verify against this torrent's hash and write
    through the normal storage path (so linked files are still copy-on-write). Only verified
    data is ever written, so a wrong guess costs a hash, never corruption.
  - `TorrentHandle::adopt_file(file, from, mode)`: replace a file the torrent has no piece of
    with a link, clone or copy of another file, then re-hash just the pieces touching it.
    Refused if any piece touching the file is had or in progress (never clobbers verified
    data, no race with an in-flight write).
  - `TorrentHandle::set_reused(files)`: files whose bytes on disk came from the library.
    Bookend pieces read those files' blocks from disk and download only the rest.
  - `Session::piece_events()`: a broadcast of verified pieces, so the core reacts instead of
    polling bitfields. A lagged receiver rescans.
- **Live sharing lives in a new core module (`share.rs`)**, not in `library.rs`, to keep
  merges with the gaps 3/4 agent easy. `library.rs` only gains additive helpers.
- **Adoption over piece sharing when possible**: a needing file with nothing verified is
  linked (hard link for full matches, clone/copy for partial), like matching at add time.
  Otherwise its pieces are shared one by one.
- **Pairs for piece sharing**: v2 same `pieces root` and length (trusted, no I/O); v1 same
  length (same name preferred) is a *candidate*, confirmed by the first piece of the needing
  torrent that verifies from the source's bytes. A candidate whose first probe fails is
  dropped. A confirmed pair that fails several pieces is dropped (a repack).
- **Bounded**: one share worker, one piece in flight, reads in `spawn_blocking`, at most
  `library.share_rate` bytes/s (default 64 MiB/s; 0 turns live sharing off). Bookend reads
  are limited by the engine's memory budget (prefill only when a new piece would fit).
- **Provenance is per torrent id and file** (the swarm's files, whatever spaces hold it), in
  a new table `file_reuse` (migration 13 after the merge; 12 on the branch). Rows go when the torrent's last entry goes.

## Plan / steps

1. Engine: piece events, `offer_piece`, `adopt_file`, `set_reused` + bookend prefill. Unit
   tests in the engine.
2. Core `share.rs`: pair index, re-match on completion/fingerprint (adopt), piece sharing
   worker. Hook into add/remove/background.
3. Provenance: migration, DB functions, recording at add time (after the check), adopt,
   piece sharing and bookends. Types + detail + session stats.
4. Web UI: Files tab provenance, detail pane total, Settings → Library total and the new
   rate setting.
5. Core tests (lifecycle style), docs, bootstrap plan ticks, full checks.

## Findings / gotchas

- `library::find_matches` skips targets that exist, so it can't be reused for a running
  torrent whose file is a sparse partial. Re-matching uses its own per-file candidate test.
- The picker has no notion of blocks already present; bookend prefill creates the piece's
  partial with those blocks marked received (`Picker::prefill`), and the piece then ranks
  first ("in progress, most complete first").
- Migration numbering: the gaps 3/4 agent may also add migration 12. Whoever merges second
  renumbers.
- **Bookend race.** Prefilling only on the actor's tick let peers start the boundary piece
  first, so it was downloaded whole. Bookend pieces are now *held* in the picker (like v2
  pieces waiting for their layer) until their library blocks are in, and the core sends
  `set_reused` right after spawning, before the check ends. With that, the bookend test's
  download is exact: episode 2 plus 2,544 bytes, not plus a whole 32 KiB piece.
- **Provenance race.** A bookend piece can verify before the background refresh records
  the add-time match (`settle_reuse`), so `record_bookend` also looks at the pending
  matches.
- **Random piece order delays sharing.** A needing piece is offered only once *every*
  source piece under it is had, and sources download rarest-first in random order. A
  64 KiB needing piece over five 16 KiB source pieces typically becomes coverable only
  past half of the source's progress. The test rate-limits its seeder (192 KiB/s for
  2 MiB) so sharing visibly starts while the source is still downloading.
- First run of `a_file_finished_later_is_linked_into_a_waiting_torrent` failed with
  `left: 0, right: 9` on `pieces_have`: the file appears (rename) before the re-hash
  finishes and before the 1 s summary refresh. The test now waits for the piece count.
- Not built, deliberately: the "mark the candidate as a different version" half of the
  architecture doc's bookend design (copy-on-write already protects the source), and
  supersession from live adoption (a pack still supersedes singles only at add time).

## Progress log

- [x] Engine primitives (`6b04d10`): `offer_piece`, `adopt_file`, `set_reused` + bookend
  prefill and holds, `Session::piece_events`. Tests: `picker::prefilled_pieces_request_only_missing_blocks`,
  `storage::adopting_a_file_links_or_copies_it`.
- [x] Core live sharing and provenance (`b383af6`): `share.rs`, migration 12 (13 on master), API types.
  Tests: `share::tests::pieces_map_across_alignments`, `db::reuse_records_add_up`, and
  `tests/library_live.rs` (five end-to-end tests).
- [x] Web UI (`8b930c2`): Files tab provenance, General tab total, status bar total,
  Settings > Library share rate. Test: `web/src/library.test.ts`.
- [x] Docs: architecture "Live sharing between running torrents" and bookends; README;
  bootstrap plan gaps ticked.
- [x] Full checks, 2026-11-11 evening PST: `cargo fmt --check` clean; `cargo clippy
  --workspace --all-targets --locked -D warnings` clean; `cargo test --locked
  --no-fail-fast` 770 passed, 0 failed; web typecheck, `bun test` (64 pass) and build;
  plugin SDK and plugins typecheck and tests (33 pass).

## Left for later

- Migration 12 may collide with the gaps 3/4 agent's migration; renumber on merge.
- Bookend "different version" marking and supersession from live adoption (see Findings).
- Pair discovery asks each candidate torrent's actor for its bitfield every 2 to 15 s
  while something downloads; fine at today's scale, worth an index if many torrents share
  file lengths.

## Open questions for the user

(none)

## Things not to do

- Don't write unverified bytes into any torrent's files, and never write through a hard link
  (the storage layer's copy-on-write handles this as long as writes go through `Storage`).
- Don't read files in the core's async tasks or the actor loop; use `spawn_blocking`.
- Don't run cargo outside `srmctl`.

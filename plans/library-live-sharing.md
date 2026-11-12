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
  a new table `file_reuse` (migration 12). Rows go when the torrent's last entry goes.

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

## Progress log

- [ ] Engine primitives
- [ ] Core live sharing
- [ ] Provenance (DB, API)
- [ ] Web UI
- [ ] Tests, docs, full checks

## Open questions for the user

(none)

## Things not to do

- Don't write unverified bytes into any torrent's files, and never write through a hard link
  (the storage layer's copy-on-write handles this as long as writes go through `Storage`).
- Don't read files in the core's async tasks or the actor loop; use `spawn_blocking`.
- Don't run cargo outside `srmctl`.

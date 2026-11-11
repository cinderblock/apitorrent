# Network fixes: uTP reset, uTP-under-load engine test, reflink CI

## Goal

Three items handed to a sub-agent on 2026-11-10:

1. `ISSUES.md` "uTP test `transfer_through_a_lossy_reordering_proxy` reset once on Linux
   CI": find the root cause, fix it, add a regression test, remove the entry.
2. `ISSUES.md` "Engine test `default_links_are_encrypted_utp` fails under load": make the
   test deterministic under load without weakening it, remove the entry.
3. Reflinks are untested on a cloning filesystem: add a separate CI job on ubuntu-latest
   that loop-mounts Btrfs and XFS images and runs the content library's reflink tests
   against them, pointed there by an environment variable.

## Environment / context

- Worktree: `C:\Users\camer\.t3\worktrees\apitorrent\t3-686b51cf`, branch `t3/686b51cf`,
  from master `e2bbdc1`.
- Cargo through `~/.srm/bin/srmctl run --slots 4 --low --label apitorrent-net -- cargo …`.
- No push, no servers, nothing left running.

## Decisions already made (don't re-ask)

- One commit per fix; the ISSUES entry is removed in the fix's commit.

## Plan / steps

1. ~~uTP reset~~ (done, `10391b8`)
2. ~~Engine test under load~~ (done, `d279c62`)
3. ~~Reflink CI job~~ (done, `d8a2da6`); not yet run on GitHub (no push from here)
4. ~~Final checks~~ (fmt, workspace clippy with `web/dist` built, `cargo test --locked`: all pass)

## Findings / gotchas

- **uTP reset, confirmed:** the regression test failed with the exact CI error
  (`Custom { kind: ConnectionReset, error: Reset }`) before the fix and passes after;
  the lossy-proxy test then ran 80 times clean (8 at once, machine at 100% CPU).
  Original analysis: the CI panic was at
  `tests/socket.rs:49`, the writer's `w.shutdown()`, not the read. `Driver::service`
  releases a connection at once when `dropped && is_write_closed()`, skipping the linger
  that a cleanly `Closed` connection otherwise keeps (`max(2·RTO, 1 s)`). If the side that
  finishes first loses its final ACKs for the peer's last data, the peer retransmits,
  the forgotten connection answers with a RESET, and the peer's `fail()` only treats a
  reset as a clean close when nothing but its FIN is unacked. So the peer's shutdown
  fails with `ConnectionReset` although every byte arrived.
  Regression test `a_dropped_stream_lingers_for_lost_final_acks` drives exactly that
  with a proxy that drops the server's `State` packets until the client retransmits.
- **Engine test, confirmed:** 6 copies of the whole loopback suite at idle priority on
  the busy machine: 1 of 6 failed, in `obfuscation_only_when_both_prefer_plaintext` (same
  `link_between` helper), `left: Some(((false, false), (false, false)))`. The log had
  `dial attempt failed … transport="uTP" error=connect failed: the connection timed out`:
  the 5 s uTP connect timed out under starvation, and the dialer fell back to TCP. The
  test alone, 64 runs (16 at once), never failed.
  Fix: `SessionConfig::utp_connect_timeout` (default 5 s, as before) replaces the
  `UTP_CONNECT_TIMEOUT` constant; the dialer no longer wraps uTP connects in the 10 s
  TCP timeout (the socket has its own deadline). `link_between` gives the leecher the
  test's patience when the seeder has uTP; the TCP-fallback test keeps the default.
- Engine test: dial errors for a transport that failed before the fallback succeeded are
  not logged anywhere (`Dialer::dial` collects them only for the all-failed case).
- Reflinks: no test asserted `Linked::Reflink` before. Added
  `partial_matches_are_reflinked_where_the_volume_clones` (core) and
  `unsharing_a_linked_file_reflinks_where_the_volume_clones` (engine), driven by
  `APITORRENT_REFLINK_DIR`. On macOS CI (APFS) the core one already clones.
- `actionlint` is at `~/go/bin/actionlint`.
- Reproduction harness for load flakes: several copies of the test binary started with
  `Start-Process … -NoNewWindow`, `PriorityClass = 'Idle'`, on the already busy machine.
  The single test alone never failed; whole suites in parallel did.
- The slot limiter is heavily contended (6+ queued jobs); builds wait a long time.

## Progress log

- [x] uTP reset: root cause, fix, regression test (`10391b8`)
- [x] `default_links_are_encrypted_utp` deterministic (`d279c62`)
- [x] Reflink CI job, actionlint clean (`d8a2da6`); it first runs when the branch is pushed
- [x] Side issue logged: other loopback tests time out under heavy load (`ebd9a3d`)
- [x] Final checks: fmt, clippy `-D warnings`, `cargo test --locked` (82 test binaries) all pass

## Open questions for the user

- None. Still to happen outside this thread: merge the branch, and watch the new `reflink`
  CI job's first run on GitHub (it has never run).

## Things not to do

- Don't push or touch any server.

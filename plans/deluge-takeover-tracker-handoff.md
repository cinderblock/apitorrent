# Deluge take-over: a clean tracker hand-off

## Goal

When apitorrent takes a torrent over from Deluge (at import, the take-over job, or resuming
an imported torrent), the tracker must see what it sees for an identity switch (see
`plans/identity-switch-hygiene.md`): Deluge's peer sends `stopped`, *then* apitorrent's
peer sends `started`, and no byte is ever reported twice. Ratios are sacred.

## Environment / context

- Take-over code: `crates/apitorrent-core/src/takeover.rs` (`pause_in_deluge`), called from
  `deluge_import.rs::take_over_running`, `lib.rs::ensure_taken_over` and the take-over job
  in `lib.rs` ("Pausing them in Deluge" / "Starting them here").
- Deluge RPC client: `crates/apitorrent-deluge/src/rpc/client.rs` (`pause`,
  `wait_until_stopped`, polling `state` for `Paused`/`Error`).
- Real-daemon test recipe: `plans/deluge-coexistence.md` ("How it was tested against a
  real daemon"): Deluge 2.1.1 + libtorrent 2.1.1 in a venv, port 58899.

## Decisions already made (don't re-ask)

- Stats are per tracker session; a `stopped` must precede the next `started`, or time out
  (20 s, as `announce::STOP_WAIT`). From the identity-switch thread.

## Findings

- **Stats already safe.** Imported Deluge totals (`downloaded_total`/`uploaded_total`) go
  into core's `downloaded_base`/`uploaded_base` only, for display and ratio. The engine's
  counters start at zero each run and announces report bytes since that tracker's
  `started`, so Deluge's bytes never reach a tracker from apitorrent. The first `started`
  after a take-over reports 0.
- **Ordering not guaranteed.** `wait_until_stopped` returns when Deluge's state reads
  `Paused`, which libtorrent sets at once; its `stopped` announce goes out asynchronously.
  apitorrent then resumes immediately, so its `started` can reach the tracker before (or
  race) Deluge's `stopped`: two peers from one IP at once.

- **Measured on a real Deluge 2.1.1 / libtorrent 2.1.1** (2026-11-20, this dev machine,
  scratch venv per `plans/deluge-coexistence.md`, a Python tracker holding `stopped` 5 s):
  `stopped` sent 4 ms after `core.pause_torrent`; state `Paused` at +0.3 s while the
  tracker still held it; `tracker_status` read `Announce Sent` until the reply (+5.1 s),
  then `Announce OK`. Only tier 0 got a `stopped` (libtorrent's `announce_to_all_tiers`
  is off by default). Deluge's `trackers` field carries libtorrent's announce entries,
  but `updating`/`endpoints` come back empty, so it can't tell an announce in flight.
  An unreachable tracker (`tracker.invalid`) also reads `Announce Sent` until it fails.
- **Signal used:** `tracker_status` (`DelugeClient::wait_until_announced`). Done once a
  torrent has shown `Announce Sent` and then something else, or hasn't shown it for a 2 s
  settle (answered before the first poll, no tracker, paused long ago). Capped at 20 s,
  then the take-over goes ahead with a warning: a silent tracker only delays it.

## Plan / steps

1. Measure on a real Deluge 2.1.1 with a local logging tracker: time from
   `pause` to `Paused`, to the `stopped` request, and what Deluge's status exposes
   (`tracker_status`, …) once the `stopped` is answered.
2. Make the take-over wait for Deluge's `stopped` to be answered (signal from step 1), or
   20 s, before apitorrent's `started`. One wait per batch, not per torrent.
3. Test, README, commit.

## Progress log

- [x] Confirmed imported totals never reach a tracker report.
- [x] Real-daemon measurement.
- [x] `pause_in_deluge` waits for Deluge's `stopped` to be answered (≤ 20 s), once per batch.
- [x] Mock-daemon unit tests; real-daemon test `waits_for_the_trackers_answer_to_the_stop`
  in `crates/apitorrent-deluge/tests/deluge_daemon.rs` passes on Deluge 2.1.1.
- [x] README, commit.

## Things not to do

- Don't seed the engine's announce counters with imported totals.
- Don't fail a take-over because Deluge's tracker never answers its `stopped`.

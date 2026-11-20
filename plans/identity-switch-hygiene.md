# Identity switch: clean stop, clean start, honest stats

## Goal

When a torrent's client identity changes (a per-tracker rule edited), the tracker should
see one client stop and an unrelated client start, never the two overlapping. Stats must
never be double-counted or invented. Ratios are sacred.

## Decisions already made (don't re-ask)

- The `started` from the new identity goes out only after the old identity's `stopped`
  has been answered, or after a timeout.
- Every unique id is regenerated on a switch, apitorrent's own included: peer id,
  announce `key`, `trackerid`, UDP connection id, and the HTTP/TLS connection.
- uploaded/downloaded are reported per tracker session (BEP 3: since `started`), not since
  the torrent was loaded.

## Findings

- Before: on a switch, `stopped` and the new `started` were spawned together and could
  arrive in either order (`torrent.rs` `refresh_identity`).
- Before: announces sent the entry's totals since load. After a pause/resume or a switch,
  the new `started` carried the previous session's bytes. On trackers that credit a new
  peer's first announce (UNIT3D-style `uploaded - 0`), that is a double count. This hit
  plain pause/resume too, not only switches.
- Before: switching back to apitorrent reused the session's peer id and key. `trackerid`
  survived a stop. The UDP connection-id cache was keyed by tracker address only, and the
  HTTP client pooled keep-alive connections and resumed TLS sessions, so a new identity
  could reuse the old one's connection.
- Not fixed here, logged in ISSUES.md: the magnet metadata fetch announces `started` under
  its own peer id and never sends `stopped` for it. Hybrid torrents announce the same bytes
  under both the v1 and v2 hashes.

## Plan / progress

- [x] announce.rs: per-tier baseline counters; a stop holds the tier until its answer
      (or `STOP_WAIT`); stop clears `trackerid`; jobs carry the stats to report
- [x] torrent.rs: pass the totals in, report stops, regenerate the native identity on a switch
- [x] tracker: UDP connection ids scoped by announce key; no HTTP pooling, no TLS resumption
- [x] tests: announcer unit tests; tracker integration test checks ordering and fresh ids
- [x] ISSUES.md entries for the two side issues

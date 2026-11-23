# Peer wire: lazy uploads, extensions, wire fingerprints

## Goal

Asked 2026-11-22 ("build them all. improve performance"):

1. Uploads read from disk lazily: accept a deep request queue (`reqq` 2000, libtorrent's)
   without holding a 16 KiB buffer per queued request. Faster uploads.
2. Client profiles carry their wire fingerprint (reserved bits, `reqq`), editable at runtime
   for custom identities.
3. `upload_only` extension message (libtorrent's, mid-connection seed status).
4. `lt_donthave` (a piece we no longer have).
5. `ut_holepunch` (BEP 55): NAT traversal through a shared peer.

Not in scope (user): request-pattern mimicry, `m` id mimicry ("meh"), `share_mode`.

## Decisions already made

- Build all five. `share_mode` is out.
- Request patterns stay ours.

## Design

### Lazy upload reads (step 1)

Before: each accepted request immediately ran `spawn_blocking` with its own 16 KiB buffer
(`on_request`, `torrent.rs`), so `reqq` 256 = up to 4 MiB per peer.

After:
- `Peer.upload_queue: VecDeque<BlockRef>`: accepted, not yet read.
- Reads are issued while (bytes being read + bytes queued in the writer) < the peer's send
  budget: about one second of its upload rate, clamped 512 KiB–4 MiB.
- Contiguous queued blocks of one piece are read in one `spawn_blocking` into one buffer and
  split with `Bytes::slice` (fewer syscalls and blocking-pool hops).
- The write loop reports piece bytes written (`PeerEvent::Sent`), which frees budget.
- Choking drops the queue (Reject each one for Fast peers); Cancel removes a queued request
  and, for Fast peers, answers with Reject (BEP 6 requires a reply to every request).
- A throttled peer now also stops disk reads, since unsent bytes count against its budget.

- Downloads: requests outstanding per peer follow its rate (3 s of it, libtorrent's
  `request_queue_time`), at least `request_queue` (64), at most 500 or the peer's `reqq`.
  The outstanding set is a `HashSet` (was a `Vec` searched per block).

Measured with `crates/apitorrent-engine/tests/throughput.rs` (ignored; release; sentinel).

### Wire style (step 2)

`WireStyle` on `ClientProfile` (engine): `dht_bit` (Always / WhenRunning /
PublicWhenRunning), `v2_bit` (hybrids; v2-only always sets it), `request_queue`,
`libtorrent_extensions`. Presets: `APITORRENT`, `LIBTORRENT`, `TRANSMISSION`. Settings:
`PeerWire` on `ClientIdentity::Custom` (serde default = libtorrent's), edited in Settings →
Client identity → Custom → "What peers see".

### Extensions (steps 3–5)

- Our ids follow libtorrent's: `upload_only` 3, `ut_holepunch` 4, `lt_donthave` 7 (ours
  for `ut_metadata` 1 and `ut_pex` 2 stay).
- `upload_only` means "finished what we want, not super-seeding" (BEP 21 partial seed),
  in the handshake and as a message when it changes (checked each tick and when a piece
  lands). Two peers that both want nothing drop each other.
- `lt_donthave` goes out when a deselected file's pieces are dropped; received, it
  cancels our requests for that piece and makes a seed a non-seed.
- `ut_holepunch`: introducers from PEX flag `0x08`; rendezvous after a failed dial, once
  per address; relay with BEP 55 errors; `UtpHint::Only` dial on connect; duplicate
  connections resolved by peer id (lower id's outgoing connection stays). Public torrents
  only.

## Progress

- [x] Plan
- [x] Throughput test + baseline (sentinel)
- [x] 1. Lazy upload reads, `reqq` 2000, adaptive request depth
- [x] 2. Profile wire style + settings/UI
- [x] 3. `upload_only` message
- [x] 4. `lt_donthave`
- [x] 5. `ut_holepunch`
- [x] Docs: `identity.rs` header, `docs/architecture.md`, `plans/deluge-coexistence.md`
- [x] Full test + clippy run on sentinel (clean), wire tests on Windows, final benchmark
- [x] Commit: pushed as `2341d91`, `80c92ec` (rebased by another thread; identical)
- [x] Push; CI run 45121256881 passed; the local desktop app (restarted 2026-11-22 10:14 PM PST) includes it

## Findings

- `docs/architecture.md` lists BEP 55 (holepunch) under Transport, but the engine doesn't
  implement it; only the wire crate names `ut_holepunch` and the PEX `0x08` flag.
- `TorrentBuilder`: a single-file torrent needs `FileSource::from_bytes(&[], …)`. A path
  makes a multi-file layout, and a seeder pointed at the bare file then waits forever (the
  first benchmark run hung at 0% CPU).

### From the sources (libtorrent 2.0.10, Transmission 4.0.6, BEP 55), read 2026-11-22

Reserved bits:
- libtorrent: extension (byte 5 `0x10`), Fast (byte 7 `0x04`) always; DHT (byte 7 `0x01`)
  whenever built with DHT, even if the DHT isn't running; v2 (byte 7 `0x10`) when the
  torrent has v2 hashes.
- Transmission: extension and Fast always; DHT when `allows_dht()`; never v2.
- Ours: extension and Fast; DHT when our DHT runs; v2 for v2/hybrid.

Extension handshake:
- libtorrent: `p` only on outgoing connections; `v`, `yourip`, `reqq` 2000
  (`max_allowed_in_request_queue`), `complete_ago` always; `upload_only` only when 1;
  `metadata_size`. `m`: `ut_pex` 1, `ut_metadata` 2 (both left out for private torrents),
  `upload_only` 3, `ut_holepunch` 4, `lt_donthave` 7, `share_mode` 8 when enabled. No `e`.
- Transmission: `e`, `ipv6`, `metadata_size` (public), `p`, `reqq` 512, `yourip`, `v`,
  `upload_only` (0 or 1) always. `m`: `ut_pex` 1, `ut_metadata` 3 (public only). No
  holepunch, no `upload_only` or `lt_donthave` messages.

Messages:
- `upload_only`: extended message, one byte 0/1. libtorrent sends it only if
  `close_redundant_connections` (default on).
- `lt_donthave`: extended message, 4-byte big-endian piece index. libtorrent sends it (with
  a reject) when a disk read for a request fails.
- `ut_holepunch`: type (0 rendezvous, 1 connect, 2 error/failed), addr type (0 IPv4, 1
  IPv6), addr, port, then a 4-byte error code. BEP 55 says the code is always present (0
  when not an error); libtorrent writes it only for errors. Errors: 1 NoSuchPeer, 2
  NotConnected, 3 NoSupport, 4 NoSelf. A relay answers rendezvous with connect to both
  sides (each gets the other's endpoint, libtorrent uses the connection's `remote()`); a
  target dials the endpoint over uTP. libtorrent sends rendezvous through an introducer
  (the peer that PEX'd the target) after a failed **TCP** connect to a peer flagged as
  holepunch-capable; a uTP failure first retries over TCP.

### Benchmarks (sentinel, 1 GiB, 1 MiB pieces, plaintext, median of 3)

- Baseline (HEAD `a2507d4`): TCP 1 leecher 1520 MiB/s, 4 leechers 2283; uTP 215 / 219.
- Lazy reads + adaptive depth (Vec requests): 1386 / 2206; uTP unchanged.
- Same with the depth fixed at 64: 1468 / 2423. With the budget fixed at 4 MiB: 1414 / 2205.
  So the deeper download queue cost the time, through the `Vec` search per arriving
  block, not the lazy reads. Fixed with a `HashSet`.
- Final (all five steps, `HashSet`): TCP 1883 / 3274 MiB/s (+24% / +43%), uTP 240 / 231
  (+11% / +5%).
- Loopback has no latency, so it can't show what the deeper queue is for: at 100 ms RTT
  64 requests cap a peer at 10 MiB/s.
- uTP is flat at ~218 MiB/s whatever the engine does: its own limit (not this work).

## Things not to do

- Don't mimic request patterns or `m` ids.

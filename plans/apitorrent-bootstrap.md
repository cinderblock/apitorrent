# apitorrent: bootstrap plan

## Goal

Build a complete replacement for Deluge with:
- a fast, cross-platform core: our own Rust BitTorrent engine;
- a native Tauri desktop app that can also serve the web UI, or act as a thin client to a
  headless daemon (with path mappings to open remote folders over OS file sharing);
- a full web UI with passkey login;
- full plugin support (TypeScript);
- first-class file sharing between torrents ("packs");
- strict TLS: never accept an unverified certificate.

The project will eventually be public, MIT-licensed, with GitHub CI builds. For now it stays
**local only**.

The target design is in `docs/architecture.md`.

## Environment / context

- Repo: `C:\Users\camer\git\apitorrent`, git initialized 2026-11-06, branch `master`, no
  remote.
- Toolchain: rustc/cargo 1.99, bun 1.4.2, tauri-cli 2.10.1, VS 2022 Community (MSVC),
  CMake, Ninja, LLVM clang.
- Cargo is capped at `jobs = 4` machine-wide. Heavy builds go through
  `~/.srm/bin/srmctl run --slots 4 --low --label apitorrent -- cargo …`.
- `rbuild`/sentinel is **not** approved for this repo.

## Decisions already made (don't re-ask)

- **License**: MIT. Local only until the user says to publish. *(user)*
- **Engine**: our own pure-Rust engine, sans-IO protocol core, depending on rqbit's
  separable leaf crates (DHT, uTP, UPnP, LSD; Apache-2.0) where each passes evaluation.
  Don't fork `librqbit`'s session. libtorrent is dropped as a backend and kept only as an
  interop test peer. *(user 2026-11-06: "wouldn't be against a full rust core … or fork the
  core from rqbit"; I chose own engine plus leaf deps)*
- **Plugins**: TypeScript on embedded V8 (`deno_core`), one isolate per plugin,
  capability-checked ops, a curated set of globals. Bun is the authoring toolchain
  (bundle and test), not the runtime, because it has no permission system. WebAssembly was
  dropped. *(user 2026-11-06: "webassembly scares me … why not TypeScript + Bun?")*
- **qBittorrent WebAPI compatibility**: yes, as a first-party plugin (`qbittorrent-api`),
  off by default. *(user agreed; plugin rather than core is my recommendation; see the
  reasons in the architecture doc)*
- **Stack**: Rust core; web UI in TypeScript and React, built with Vite and Bun; desktop on
  Tauri 2.
- **TLS**: expect a reverse proxy. Built-in rustls supports bring-your-own certificates and
  ACME HTTP-01. DNS-01 runs through a `dns-provider` plugin.

- **Scope: everything.** The user's answer to "which milestone next" (2026-11-06) was "do it
  all. boil the ocean". Work runs in parallel streams (see Plan / steps). *(user)*
- **Real-world swarm test approved** (2026-11-06): join the public Ubuntu swarm, download
  about 50 MB, stop and delete. *(user)*
- **Unwanted files never sit in the save directory.** Boundary bytes go to a part store;
  deselecting a file with data moves it to a managed held area (default) or deletes it.
  *(user 2026-11-06)*
- **Dedupe goes past size matching**: bookend pieces, piece-level reuse, v2 roots, and idle
  fingerprinting. *(user asked; design in `docs/architecture.md`, "Going further")*
- **Pack switchover is automated**: superseded-by links, retirement on "unregistered", and
  a tracker-adapter plugin hook for tracker-specific behaviour. *(user)*
- **Limits are hierarchical** (session, group, torrent, peer) for bandwidth, memory,
  connections, half-open attempts, connection attempts per second, and queueing. *(user
  asked for app-wide and per-torrent/group limits, plus connection and connect-rate limits)*
- **Banning works on evidence**: smart ban, parole, trust points, escalating global IP bans.
  A single unattributable failure is not a ban. *(user asked for a sophisticated system;
  my design)*
- **Naming policy per storage root**: portable by default, native as an option, forced
  portable on filesystems that need it. *(the user asked whether portable-everywhere was
  best; this is my answer)*

## Prior art (checked 2026-11-06)

No existing client has the whole combination:

- **qBittorrent**: libtorrent, mature, web UI. No thin-client desktop, Python search
  plugins only, no passkeys, no content library.
- **Porla**: a libtorrent daemon, web UI, Lua plugins. Early stage, C++.
- **rqbit**: Rust, web UI, Tauri wrapper. Weak engine. **rustTorrent** is a fork.
- **BiglyBT**: swarm merging, size-only matching, Java.
- **cross-seed / qui**: external file reuse through links. They warn that partial overlap
  corrupts seeded files. Our staged boundary-piece writes address this.
- **Transmission**, **Flood**: the daemon-plus-web-UI pattern.

## Plan / steps

M0 to M3.1 are done (see the progress log). Everything else is in scope. It runs as
parallel streams. Self-contained crates go to background agents working in worktrees; the
engine core stays in the main thread, because its pieces interlock.

- **Main thread (engine core), in order:**
  1. ~~Real-world interop test against the Ubuntu swarm~~ (done, two bugs fixed)
  2. ~~Part store, unwanted-file handling, held area~~ (done)
  3. ~~Memory budget (session and torrent)~~ (done; group limits come with step 5)
  4. ~~Bad-peer system~~ (done; v2 block proofs come with hash requests in step 7)
  5. ~~Rate limits, connection limits, half-open limits, connect-rate limits, groups, queueing~~ (done)
  6. ~~Magnets (BEP 9), PEX (BEP 11), LSD (BEP 14), then wiring in DHT, uTP and MSE~~ (done;
     real-world uTP and encryption tests passed)
  7. ~~v2 hash requests~~, ~~upload `cancel`~~, ~~webseeds (BEP 19)~~, ~~naming policy~~ (done)
  8. Content library: ~~matching, linking, copy-on-write, pack switchover~~ (done); still
     to do: piece-level reuse of partial matches, idle fingerprinting (v2 roots and SHA-256
     of v1 files), reflinks, tracker-adapter plugin hook ← current
- **Delegated streams:**
  - `apitorrent-dht`: BEP 5, 32 and 42 ✅ done (own implementation, see Findings)
  - `apitorrent-utp` (BEP 29) and `apitorrent-mse` (MSE/PE)
  - ~~M4 auth: passkeys, sessions, API tokens, so the API can leave loopback~~ done
    (`plans/m4-auth.md`)
- **Delegated streams, started 2026-11-08 (background agents in worktrees, cherry-picked
  when done):**
  - M8 plugin host (`apitorrent-plugins`, plugin SDK, rss and ubuntu-releases plugins,
    Plugins page). Plan: `plans/m8-plugins.md`
  - ~~Built-in TLS: bring-your-own certificates, ACME HTTP-01 and TLS-ALPN-01, DNS-01 hook,
    self-signed with a pairing code~~ done, merged. Plan: `plans/tls.md`
  - ~~M7 Tauri desktop app~~ done, merged. Plan: `plans/m7-desktop.md`
- **Main thread meanwhile:** ~~M3.2 rest (embed `web/dist`, column chooser, slow-torrent
  queue option)~~ done; now content library step 8.
- **Later (after the streams above land):** M3.2 UI depth, M7 Tauri desktop, M8 plugin
  host and first plugins (qbittorrent-api, RSS, Ubuntu, Raspberry Pi, tracker adapters),
  M9 Deluge importer, rules and channels, TLS (bring-your-own certificates, ACME), M10 CI.

## Findings / gotchas

- libtorrent's fixture expectations are platform-dependent (Windows strips trailing spaces,
  others don't). We apply one portable rule set everywhere, so our expectations differ in a
  few documented cases (`crates/apitorrent-metainfo/tests/libtorrent_fixtures.rs`). A
  Deluge import must therefore carry libtorrent-style paths as per-file overrides where
  they differ.
- Real `.torrent` files can have trailing bytes after the root dictionary, and keys out of
  order outside the v2 file tree. Both are accepted. The v2 file tree must be in order.
- BitComet's legacy pad files are recognised by the `_____padding_file_` name prefix.

- No maintained Rust bindings for libtorrent exist. `libtorrent-sys` was last updated in
  2022 and `lt-rs` is 0.1. This was moot once we chose our own engine.
- rqbit 9.1 (October 2026) lacks MSE/PE encryption, rarest-first picking and full
  BEP 52 support. It is Apache-2.0, so its leaf crates can be MIT-compatible
  dependencies.
- Bun has no permission model, so it isn't safe as an in-process or subprocess plugin
  runtime.

- Windows reserves TCP port ranges (Hyper-V/WinNAT), so binding can fail with
  `PermissionDenied`. On this machine 17691–19290 (and more) are excluded, while 8742 is
  free. `serve` reports this case with an explicit message.
- Tracker TLS uses rustls with the **ring** provider (not aws-lc-rs) so Windows builds
  need no CMake or NASM. reqwest gets a preconfigured rustls config; if reqwest's rustls
  version ever diverges from ours, the client fails to build (a test catches it), rather
  than silently changing TLS backend.
- Tracker proxy environment variables are ignored on purpose (`no_proxy()`). Proxy
  support will be an explicit setting.
- **First real-world test, 2026-11-06 9:47–9:55 PM PST (05:47–05:55Z)**, against the
  Ubuntu 26.10 desktop ISO swarm (HTTPS tracker, 6.6 GB, 256 KiB pieces), with outbound
  connections only. The peer listener was bound to loopback to avoid a Windows Firewall
  prompt. Results:
  - It interoperates: 13–14 real peers (11–12 of them seeds), 12–16 MB/s, 338 pieces
    verified, no hash failures.
  - **Bug: a save path with `/` failed every write on Windows.** The `\\?\` prefix turns
    off normalisation, so `/` became part of the file name. Fixed: `long_path` now calls
    `std::path::absolute` first. A regression test covers it.
  - **Bug: a disk error didn't stop the torrent.** It downloaded about 500 MB that could
    never be stored. Fixed: `fail()` now stops the torrent and disconnects peers, and
    resume retries. A loopback test covers it.
  - **Finding: in-flight memory.** 46 MB arrived before the first piece completed,
    because requests were spread over many pieces at once. The memory budget and piece
    affinity (step 3) must fix this.
  - **Overshoot: the cap was 50 MB, but about 600 MB was downloaded in total** (500 MB in
    the buggy first run, 110 MB in the second). 1-second polling can't stop at
    15 MB/s with no rate limit. Next time, use a rate limit or a byte cap in the engine.
- **Peers dropped by a pause or a clean close were never redialled** (found while
  testing re-selection). The `known` set also blocked trackers from re-adding them.
  Fixed: outbound peers are requeued with a retry time (10 s after a normal close, 60 s
  after a failed connect, at once after a pause). Seeds are skipped while we have
  everything, and become eligible again when files are reselected.
- Held area placement: `data_dir/held/<id>` when a test rename from the save path
  succeeds (same volume), otherwise `<save path>/.apitorrent-held/<id>`. The choice is
  stored per torrent, so it never changes under existing held files. Part stores live in
  `data_dir/parts/<id>.parts`.
- **No `title=` tooltips in the web UI** (user rule from the `tooltips` skill: invisible
  on touch devices). Information goes inline. Existing uses were removed.
- Bash heredocs in this environment fail on some content ("unexpected EOF"). Write
  scripts and test snippets to the scratchpad with the Write tool, then run or append
  them.
- **NTFS zero-fill stall.** On Windows, the first write past the end of a non-sparse
  file makes NTFS zero-fill the gap first. With rarest-first writes into a 6.6 GB file,
  that stalled writes for seconds: 46 MB arrived with no completed piece. Fixed by marking
  new files sparse (`FSCTL_SET_SPARSE`; the one `unsafe` block, with a scoped allow).
  Second Ubuntu run, 2026-11-06 about 10:40 PM PST (06:40Z): first piece after 16 MB,
  and at the pause 223 pieces (56 MiB) verified out of 60 MiB downloaded; no hash
  failures; data deleted. It overshot the cap again (60 MiB against 50); 100 ms polling
  plus pause isn't a hard cap.
- **Trust design trap:** docking every contributor of a failed mixed piece (as a naive
  libtorrent-style rule does) can ban an honest seed. Several mixed pieces are in flight
  before parole takes effect, so an honest peer collects many penalties. Now a single
  mixed failure only means parole, and smart ban settles it. A unit test simulates 50
  mixed failures with one liar.
- Loopback tests can give each session its own IP (127.0.0.2, 127.0.0.3), which works on
  Windows. That's how the ban tests tell the liar from the honest seeder.
- **The daemon turns LSD on by default, which binds UDP 0.0.0.0:6771.** On Windows that
  can raise a firewall prompt, so smoke tests pass `--no-lsd` (and bind the peer port to
  loopback).
- Daemon command-line limits (`--memory-limit-mib` aside) only seed the *first* settings;
  after that the stored settings win. Memory isn't a live setting yet (the budget is
  fixed at session start).
- **BEP 52 hash requests** (`hashreq.rs`): v2-only magnet links fetch each multi-piece
  file's piece layer from peers in chunks of up to 512 with uncle proofs. The spec is
  ambiguous by one about which proof layers are omitted, so the verifier folds whatever
  uncles arrive and accepts only an exact root match (also trying with one redundant
  leading uncle dropped). We answer piece-layer requests and reject leaf-layer ones.
  **Interop with libtorrent is untested**: check it in the interop matrix (M10).
- **Pieces waiting for their layer must stay *wanted*.** The first version held them at
  priority 0, and a v2 magnet torrent then reported itself finished before downloading
  anything. The picker now has a separate "held" state: wanted, but not started.
- A magnet link keeps the id it was added under (its v1 hash) even when the metadata
  turns out to be hybrid, whose own id would be the v2 hash. Core passes ids explicitly
  for this reason.
- **Windows: `File::open(..).sync_all()` fails with "Access is denied"**, because
  `FlushFileBuffers` needs write access. That broke the cross-volume move fallback and
  unsharing linked files. Open with `.write(true)` to flush.
- The Deluge importer is tested on synthesized state, not a real install: none on this
  machine, and the user's Deluge is presumably on a server I must not touch. Ask for a
  copy of a real `~/.config/deluge` (it can be from a stopped Deluge) to test before
  relying on it.
- **Merged the three agent branches** (2026-11-07 ~1 AM PST) by cherry-picking onto
  master: DHT (2 commits), uTP/MSE (3), auth (5). Conflicts were only in `Cargo.toml`
  (union, one duplicate `socket2` removed), the lockfile (regenerated), the plan, the
  README, `client.ts` and `styles.css` (unions). My bans API test was rewritten on the
  auth test helpers. Backup branch `backup/pre-merge-20261107` holds the old tip.
- **uTP close race** (found as a flaky socket test after the merge, 2 of 3 full runs): if
  the peer forgets a finished connection before our FIN retransmission reaches it, we got
  a reset or timeout although every byte was delivered. Fixed: with all data acked and
  the peer's FIN received, a missing FIN ack is a clean close. A deterministic sim test
  covers it (verified to fail without the fix).
- The auth UI had `title=` tooltips; replaced with visible text per the tooltip rule.
- **MSE handshake could drop its final message** (flaky `a_tiny_pipe...` test, about 1 in
  6 runs): when the machine finished while an older output buffer was still being
  written, the driver returned without taking the output of the last feed, so the
  initiator timed out waiting. Fixed in `drive` (take output again before finishing; also
  re-feed partially consumed input). 0 failures in 80 runs after.
- **DHT nodes with sparse tables never re-bootstrapped** (only empty ones did), so a node
  that joined a busy network with 1–3 neighbours stayed poorly connected for ~15 min.
  Now tables under 8 nodes re-bootstrap every 30 s. Found as a flaky loopback test under
  CPU contention.
- After the fixes, three full workspace runs in a row were clean.
- Known engine gaps: none blocking real-world use now that hash requests and cancels exist.
- **uTP and MSE wiring** (done): `SessionConfig.utp` (default on) binds a `UtpSocket` on
  the peer port's UDP side; failing to bind only logs and leaves TCP. Outgoing connections
  go through `peer::Dialer`: uTP first when `prefer_utp` (default, like libtorrent), TCP as
  the fallback, each attempt covering connect, MSE and the BitTorrent handshake. uTP is
  skipped for addresses its socket can't reach (other address family, or loopback-bound
  socket to a non-loopback peer). `SessionConfig.encryption` is an `apitorrent_mse::Policy`
  (default `Enabled(Rc4)`): outgoing tries MSE and retries in plaintext on a fresh
  connection when the MSE handshake fails; incoming runs `MseStream::accept` (which passes
  plain BitTorrent through) with a `SkeyTable` kept beside the torrent map, and rejects a
  handshake naming a different torrent from the MSE SKEY. Peers carry a `Link` (uTP,
  encrypted), shown as flags `U` and `E`. Daemon flags `--no-utp`, `--prefer-tcp`,
  `--encryption disabled|enabled|prefer-plaintext|required`. Seven loopback tests cover
  the combinations; every other loopback test now runs over encrypted uTP by default.
- **Gotcha: an encrypting `MseStream` must be flushed.** It encrypts a write into its own
  buffer and accepts it whole, sending what the socket takes; the rest goes out on the
  next write or flush. The engine's writer never flushed (TCP and uTP have nothing to
  flush), so over TCP the tail of a burst sat in the buffer and transfers stalled. uTP
  accepts whole writes, which hid it. The writer and both handshake writes now flush.
- **Gotcha: with port 0, the TCP port's UDP twin may be taken.** Under the full test suite
  (many UDP sockets from DHT and uTP tests), a session sometimes got an ephemeral TCP port
  whose UDP side was in use, so it ran without uTP and peers fell back to TCP (4–5 of the
  new transport tests failed in 2 of 3 runs). `bind_peer_port` now retries a fresh TCP
  port, up to 20 times, until UDP binds too; a fixed port with its UDP side taken runs
  without uTP and logs a warning. Three full runs clean afterwards.
- **Real-world encryption test** (2026-11-07 1:49–2:00 AM PST (09:49–10:00Z), Ubuntu 26.10
  desktop swarm, peer port bound to 127.0.0.1 so uTP was skipped for internet peers):
  encryption `enabled` reached 28–29 of 32–34 peers encrypted (Transmission 4.1.x,
  qBittorrent 4.5 to 5.3, libtorrent/rtorrent 0.13.8, Freebox), 53–58 MiB in about 5 s,
  no verification failures. Plaintext baseline: 44 MiB in 3.6 s from 33 peers.
  **Unexplained:** the very first encrypted run got only 2 peers and 1 MiB in 90 s; two
  later encrypted runs were normal. The info log showed no errors and the run didn't
  capture tracker state. If it recurs, capture `trackers` from the detail endpoint and
  run with `apitorrent_engine=debug`.
- **Real-world uTP test** (2026-11-07 ~2:15 AM PST (~10:15Z), peer port on 0.0.0.0 under a
  temporary Windows Firewall rule the user approved for testing, removed afterwards): 12
  of 20 peers over uTP, 18 of 20 encrypted (Deluge 2.2 / libtorrent 2.0.10, Transmission
  4.0 to 4.1, qBittorrent 5.x, rtorrent), 45 MiB in 3.6 s, no verification failures.
- **Encryption is a live setting** (`Settings.encryption`, the settings dialog's
  Connections section; `Session::set_encryption`). New connections use a change, open ones
  keep theirs. The daemon's `--encryption` is only the starting value. The UI says plainly
  what MSE is worth: it hides traffic from passive inspection and throttling but doesn't
  authenticate peers, so it doesn't stop a man in the middle; integrity comes from the
  piece hashes either way.
- **DHT wiring** (done): `SessionConfig.dht` starts an `apitorrent_dht::Dht` on its own UDP
  port (daemon default 6882, `--dht-port`, `--no-dht`), leaving the peer port's UDP side
  for uTP. Handshakes set the DHT bit; peers with it get a PORT message (not on private
  torrents), and PORT from peers adds them as DHT nodes. A 5 s announcer announces each
  public torrent every 15 min, but only once the routing table has nodes: an announce made
  on an empty table finds nothing and would otherwise wait 15 min to retry. Magnet fetches
  also run a DHT `get_peers`. State is saved in the meta table (`dht_state`, base64) and
  restored at start. `/stats` reports `dhtNodes` (null when off), shown in the status bar.
  Smoke scripts must pass `--no-dht` too (it binds 0.0.0.0 and can raise a firewall prompt).
- **Naming policy** (done): `sanitize::Naming` is Portable, Native or Verbatim, and
  `Info::paths(naming)` re-parses the info dictionary under the chosen rules. Core picks a
  policy for each torrent when it is added. The `naming` setting is portable by default;
  with native, a probe of the save directory (can it create `probe:name.`, and does it
  fold case?) decides. The result is stored per torrent (DB migration 5) and never
  changes, because the files on disk depend on it. On NTFS native falls back to portable.
  Web seeds use verbatim names for their URLs.
- `apitorrent_tracker::strict_http_client` is the one way to make HTTP requests (verified
  TLS, no environment proxies, no Referer, no HTTPS-to-HTTP redirects). Reuse it for
  feeds and plugins.

- **DHT: rqbit's `librqbit-dht` 9.1 evaluated and rejected; we wrote our own**
  (`crates/apitorrent-dht`, 2026-11-06). Reasons:
  - No BEP 42 (no secure IDs, no `ip` key handling), no BEP 43 read-only mode, no BEP 33
    scrape, and `implied_port` is always sent as 0.
  - IPv6 shares one dual-stack socket (`librqbit-dualstack-sockets`) instead of BEP 32's
    separate socket and table.
  - Write tokens are random values kept in a 1000-entry queue, not rotating secrets, so
    under load a valid token is evicted before it's used, and every `get_peers` costs
    memory.
  - It panics on input it doesn't expect: `.unwrap()` on decoded fields in
    `bprotocol.rs`, and an `expect` on a malformed `DHT_QUERIES_PER_SECOND` env var.
  - Dependency weight: `librqbit-core`, `anyhow`, `chrono`, `dashmap`, `serde_json`,
    `backon`, `leaky-bucket`, `parking_lot`, `tokio-util`, and `rand` 0.10 (we use 0.9, so
    two copies). Persistence is JSON files, where we want opaque bytes.
  - Its API (`Arc<DhtState>`, a `RequestPeersStream` that announces when given a port)
    is close to what we want, but wrapping it would still leave the gaps above.
  Ours is about 4,400 lines (unit tests included) with five runtime deps we already have (bencode, sha1, rand,
  tokio, futures-util) plus `socket2` for `IPV6_V6ONLY`.
- DHT design: a sans-IO `Node` (datagrams and timers in, transmits and events out), one
  routing table per family, a tokio `Dht` driver. Deterministic simulations of up to 200
  nodes (with 10% packet loss) run in `cargo test`, alongside a 20-node loopback network.
- DHT gotcha: BEP 42 ID changes break everyone's routing entries for that node (they hold
  the old ID). Fix: a verified answer from an address we hold under another ID replaces
  the entry, and a lookup still harvests nodes and peers from an answer whose ID doesn't
  match. Without this, simulations lost most announces.
- DHT gotcha: if every node enforces BEP 42 from the start, no table admits anyone (all
  IDs begin random), so nobody gathers the votes to fix its ID. Enforcement is off by
  default, as in libtorrent; the simulation runs half the nodes enforcing.
- DHT gotcha: BEP 33's estimate formula clamps the zero-bit count to `m - 1`, so an empty
  filter reads as 0.5. We return 0 for an empty filter. BEP 33 also counts IPs, not
  ports: two peers on one IP are one.
- DHT gotcha: a lookup started while the routing table is still empty (right after a
  restart) must fall back to the saved nodes, not just the bootstrap routers.
- DHT gotcha (Windows): an ICMP port-unreachable from an earlier send surfaces as a
  `recv_from` error (`WSAECONNRESET`). The driver logs and ignores receive errors.
  Disabling it with `SIO_UDP_CONNRESET` needs `unsafe`, which the workspace denies.
- DHT limits worth knowing for wiring: `stats()` is a snapshot refreshed every 250 ms;
  dropping a `get_peers` stream cancels its lookup, dropping an `announce` stream doesn't.
  Responses (up to ~1 KB for a scrape) are capped at 500 a second to limit amplification.
- **uTP: rqbit's `librqbit-utp` 0.7.0 rejected; we wrote `apitorrent-utp`.** Evaluated
  2026-11-06 (Apache-2.0, 12k lines, smoltcp-derived). Against it: the only congestion
  controller is CUBIC (`lib.rs`: "TODO: LEDBAT congestion control"), and LEDBAT is the
  point of uTP; its own TODO lists "connect: need to retry SYN packets" as open; it pulls
  in libc, socket2, dontfrag, librqbit-dualstack-sockets, bitvec, ringbuf, parking_lot,
  lazy_static and tokio-util; one tokio task per connection. For it: SACK, RFC 6298 RTO,
  MTU probing with the don't-fragment bit, a `Transport` trait. The API would have fit.
- uTP gotcha: an acceptor's STATE carries its *next* sequence number, which the initiator
  takes as where the stream starts. If the acceptor sends data first, a later STATE (say,
  answering a retransmitted SYN) points past that data and the initiator silently skips
  it. Our acceptor sends no data until the initiator's first post-SYN packet, and the
  initiator ACKs as soon as it connects. Found by the simulation: 4 packets lost under
  reordering.
- uTP gotcha: RFC 6817's linear decrease (at most about 1 MSS per RTT) can't drain the
  queue slow start builds; the mean queuing delay sat at 157 ms against a 100 ms target. A
  LEDBAT++-style multiplicative decrease (halve per RTT at twice the target) gives a mean
  of 89 ms and 95% link use in the simulated bottleneck test.
- uTP: no path-MTU probing. Without the don't-fragment bit, an oversized probe is
  fragmented and still arrives, which looks like success; tokio doesn't expose DF, and
  setting it needs unsafe FFI (denied workspace-wide). Packets are 1400 bytes by default,
  configurable from 1232 to 1472.
- Windows: `recv_from` on a UDP socket returns `ConnectionReset` after an ICMP port
  unreachable for an earlier send. The uTP receive loop ignores it; any other UDP loop
  (the DHT's, if it owns the port) must too.
- uTP and DHT on one port: `UtpSocket::bind` hands non-uTP datagrams (a bencoded `d...`
  never parses as uTP) to `take_unrecognized()` and sends with `send_to()`; or the DHT
  owns the socket and feeds uTP datagrams to `UtpSocket::handle_datagram()` over
  `with_transport()`.
- MSE: chose `crypto-bigint` 0.7 (constant-time, fixed-width Montgomery modpow) over
  `num-bigint` (already in the tree, but variable-time). RustCrypto `rc4`'s cipher is not
  `Clone`, so the initiator derives its VC sync marker from a second cipher instance. The
  known-answer transcript comes from an independent Python reference whose RC4 matches
  the classic `Key`/`Plaintext` vector; no published MSE test vectors were found.
- MSE gotcha: a handshake driver that writes, then reads, deadlocks when both sides' writes
  block (a 13-byte pipe in tests; can't happen with real TCP buffers, but still a bug).
  `MseStream` reads and writes concurrently.
- MSE policy for the engine: `Policy::initiates_mse()` decides whether to open with MSE;
  under `Enabled` a failed MSE attempt may be retried as plain BitTorrent
  (`retry_plaintext()`), which is the engine's job. Pass our BitTorrent handshake as the
  initial payload to save a round trip. An accepted plaintext stream replays its first
  bytes, so the engine reads the BitTorrent handshake from the `MseStream` either way.
- Auth (M4): webauthn-rs 0.6 dropped OpenSSL for RustCrypto, so passkeys need no C
  toolchain; 0.5 and `webauthn-authenticator-rs` still pull OpenSSL in. Passkeys only work
  on `https://` or `http://localhost`, never on an IP address, so the UI points
  `127.0.0.1` users at `localhost`. Plain-HTTP cookies drop the `__Host-` prefix because
  Safari refuses `Secure` cookies on `http://localhost`.

### Embedded web UI (2026-11-08)

- **Cargo reruns a build script every build when a `rerun-if-changed` path doesn't
  exist**, and recompiles everything downstream of it. Checked with a throwaway crate: with
  `missing/dist` absent, every `cargo build` printed `Compiling`. So embedding can't be "on
  by default, skipped when `web/dist` is missing". It is a feature (`embedded-ui`) that
  fails the build with instructions when `web/dist` is missing.
- Source maps (`*.map`, 1.4 MB) stay out of the binary. The 327 KB script ships as 84 KB of
  brotli.
- Port 18742 is inside a Windows reserved range on this machine. The daemon's error says
  so. Use 8799 or similar for smoke tests.

### Piece-level reuse (2026-11-08)

- **Sampling can't prove a file identical.** The first version classed a repack with
  piece 9 changed as a full match: 8 samples over 20 pieces read pieces 0, 2, 5, 8, 10,
  13, 16 and 19. That's safe for the data (the file is hard-linked, and the engine
  unshares it before writing), but it would have marked the old torrent superseded, and
  retiring it could follow. Supersession now waits for the new torrent's check.
- A finished engine session doesn't dial peers, so `seed.add_peers(core)` never
  connected in a test. Use a tracker that hands out the seeder's address
  (`tracker_with_peer` in the core tests).
- Supersession candidates are kept in memory only. If the daemon restarts during the
  new torrent's first check, the old torrent is simply not marked. Harmless, since it
  keeps seeding.

### Disk full (2026-11-09)

- At about 12:05 AM PST (08:05Z) C: ran out of space (392 MB free), and a write of
  `web/src/styles.css` left it empty. It was rebuilt from HEAD plus the session's two
  edits, and nothing else was damaged.
- The cause was build output. `~/.cargo/build` (the machine-wide cargo build dir, shared
  with other projects) was 219 GB. apitorrent's master build dir was 60 GB, half of it
  stale incremental sessions (1,060 of them). The finished agents' worktrees held about
  20 GB, and the live plugin and desktop agents 11 and 22 GB.
- Fixed with `cargo clean` in the four finished worktrees and by deleting master's
  `debug/incremental`, which brought it to 50 GB free. Watch free space while the agents
  build V8 and Tauri, and clean finished worktrees after merging them.

## Progress log

- [x] Research the engine options and prior art
- [x] `docs/architecture.md`, revised for our own engine and TypeScript plugins
- [x] User's go-ahead on direction (2026-11-06)
- [x] M0.1 workspace, MIT licence, README (`1f9a837`)
- [x] M0.2 bencode: strict codec with property tests (`1f9a837`)
- [x] M0.3 metainfo: v1, v2 and hybrid, magnets, sanitising; libtorrent fixtures pass (`04e7c70`)
- [x] M0.4 wire: handshake, all messages, BEP 9/10/11 payloads, decoder (`c7fb73c`)
- [x] M1.1 torrent creator, v1/v2/hybrid (`642c534`)
- [x] M1.2 engine core: bitfield, layout, storage, verify, picker (`0e4c4c8`)
- [x] M1.3 networking: session/listener, torrent actor, peer tasks, choker (`fc61d2f`)
- [x] M1.4 loopback tests: v1/v2/hybrid transfers, 4-node swarm, skipped files (`fc61d2f`, `478d482`)
- [x] M1.5 trackers: `apitorrent-tracker` (HTTP/HTTPS/UDP, strict TLS via
  rustls-platform-verifier, built by a background agent, cherry-picked as `d25c643`),
  announce scheduler with BEP 12 tiers wired into the actor (`7153af1`)
- [x] M1.6 resume state, file-size validation of resume data, check on add (`c9cd733`)
- [x] M2.1 `apitorrent-types` with TS bindings → `web/src/api/generated` (`e5f9595`)
- [x] M2.2 `apitorrent-core`: SQLite (WAL, FULL sync, migrations), registry, store (`e5f9595`)
- [x] M2.3 `apitorrent-server`: REST + sync WebSocket, Host/Origin guards (`1220f9f`)
- [x] M2.4 `apitorrentd` daemon, default port 8742 (`1220f9f`)
- [x] M3.1 web UI: live table, filters, details, file priorities, add/remove, drag-drop
  (`d185833`); daemon serves it with CSP and other security headers (`f459ad6`)
- [ ] M3.2 UI: per-tracker status (engine already reports it), peers tab, label
  editing, column chooser (the table is tight at 1280 px), embedding `web/dist` into
  release binaries, bans list, session stats (memory, connections), "slow torrents
  don't count" queue option like Deluge's
- [x] Real-world Ubuntu swarm test: interop works; two bugs fixed (see Findings)
- [x] Part store and unwanted files: `parts.rs` (per-torrent slot file), per-file
  placements (save, held, parts) in `FileStorage`, hold or delete on deselect, release
  held, data deletion, placements persisted (DB migration 2), API and Files tab
- [x] Memory budget: session-wide byte budget (default 256 MiB, `--memory-limit-mib`) and
  optional per-torrent limit; the picker starts new pieces only when there is room.
  Sparse files on Windows
- [x] Bad-peer system: `reputation.rs` (whole-piece proof, smart ban by block digests,
  parole with exclusive pieces, trust only lost on repeated failures), `ban.rs`
  (escalating session-wide IP bans; P2P, DAT and CIDR blocklists), `--blocklist`,
  `GET/POST /bans`, `DELETE /bans/{ip}`
- [x] Limits and queueing: `limits.rs` (GCRA buckets per session/group/torrent, applied
  in the peer I/O tasks; connection, half-open and connect-rate counters), wanted-piece
  progress and `finished` in stats, core settings (stored in `meta`), groups chosen by
  label, per-torrent limits, a queue with download/seed/total limits and moves (DB
  migration 3), API (`/settings`, `/torrents/{id}/limits|label|queue`), UI (Settings
  dialog, queue column and buttons, label and limits editing)
- [x] Magnets and PEX: `metadata.rs` fetch (BEP 9, verified against the info-hash,
  bad senders dropped), torrents serve `ut_metadata`, PEX (BEP 11, not for private
  torrents), core pending entries ("Fetching metadata" state, DB migration 4, `so` file
  selection, survives restarts, pause and remove)
- [x] LSD (BEP 14): `lsd.rs` codec and multicast service, public torrents only, on in the
  daemon (`--no-lsd` to turn off), off in the engine default (it binds UDP 6771, which
  can trigger a firewall prompt)
- [x] Step 7: BEP 52 hash requests, upload cancel, web seeds, naming policy
- [x] DHT: `apitorrent-dht` crate, own implementation (BEP 5/32/33/42/43), built by a
  background agent (`t3/mainline-dht-crate`, cherry-picked). Wired into the engine,
  core and daemon (announces, PORT, magnet lookups, saved state, `/stats`)
- [x] uTP and MSE: `apitorrent-mse` and `apitorrent-utp`, built by a background agent
  (`t3/mse-and-utp-transport-crates`, cherry-picked); engine wiring is step 6. See
  `plans/transport-mse-utp.md`.
- [x] M4 auth: `apitorrent-auth` (own `auth.db`), passkeys via webauthn-rs 0.6, optional
  argon2id passwords, setup token, sessions, scoped API tokens, rate limits; auth on every
  route and the sync socket; non-loopback listening behind a trusted proxy; sign-in, setup
  and account pages (background agent, `t3/add-m4-authentication`, cherry-picked; details
  in `plans/m4-auth.md`)
- [x] Content library and pack switchover: `library.rs` (v2 root and v1 sampled matching,
  hard link or copy, superseded detection, "unregistered" patterns), engine copy-on-write
  for linked files, DB migration 6 (`linked`, `superseded` table), retirement every 60 s,
  settings and UI. A season pack replacing two single episodes finished with nothing
  downloaded (core test `a_pack_reuses_and_supersedes_single_episodes`)
- [x] Content library: piece-level reuse (8-piece sampling; full matches hard-linked,
  partial ones cloned or copied; the check keeps what verifies), reflinks (`reflink-copy`,
  also for the engine's unshare step), supersession decided after the new torrent's check
  (`settle_supersedes`). Setting `partialReuse`, on by default. Core test
  `a_repack_downloads_only_what_changed` covers a change the sampling misses and one it
  catches: about one piece downloaded each time, the old file untouched, nothing
  superseded
- [x] Idle fingerprints: `fingerprint.rs` (v2 root and SHA-256 in one read, rate-limited,
  gives up if the file changes mid-read), DB migration 8 `fingerprints`, a background
  pass every 5 minutes (skipped while checking), setting `fingerprintRate`, matches
  tagged `MatchedBy::{Root, Fingerprint, Pieces, Name}`. Core test
  `fingerprints_let_v2_torrents_reuse_v1_files`
- [ ] Content library, next: tracker-adapter hook (after M8 lands)
- [ ] Reflinks are untested on a filesystem that clones: this machine has only NTFS (C:,
  28 GB free on 2026-11-08). Test in CI on a loop-mounted Btrfs or XFS
- [x] M9 (part): Deluge importer. `apitorrent-deluge` reads `state/*.torrent`,
  `torrents.fastresume`, the `torrents.state` pickle (safe unpickler; fixture made by
  Python), `label.conf` and `core.conf`. `Core::import_deluge` keeps files where they are
  (path-prefix remapping, renamed files, libtorrent-layout detection; DB migration 7
  `file_paths`), uses resume progress without rehashing, and keeps labels, queue order,
  limits, totals and edited trackers. `POST /import/deluge`, and a UI dialog under
  Settings
- [x] M9 (part): seed limits (`seeding.rs`, DB migration 9 `seeding`, settings, group and
  per-torrent limits, `PUT /torrents/{id}/seed-limits`, UI in Settings, groups and the
  details, Deluge import). Core test `seed_limits_pause_then_remove`
- [ ] M9 rest: rules over source candidates (after M8 lands, since sources produce the
  candidates), stable "latest" aliases, distribution channels
- [x] M3.2 (part): Peers tab (client, progress, rates, flags with a legend), Trackers tab
  with live status, status bar (`GET /stats`), bans list in Settings, wanted-piece
  progress
- [x] M3.2: `web/dist` embedded in release binaries (`apitorrent-webui`, daemon feature
  `embedded-ui`): brotli and gzip copies made at build time, an entity tag per encoding,
  immutable caching for hashed assets, `no-cache` for the page, a real 404 for missing
  assets. Off by default, so a fresh clone builds without Bun
- [x] M3.2: "slow torrents don't count" queue option, with libtorrent's rules and defaults
  (below 2 KiB/s for 60 s; slow torrents skip the download and seed limits but count
  toward the total, and are placed first so they don't flap). Imported from Deluge's
  `dont_count_slow_torrents`
- [x] M3.2: column chooser (show, hide and move columns; right-click the header; saved
  per browser; new columns arrive in their default place). New columns: ETA (now
  sortable), Downloaded, Uploaded, Label, Save path. Fixed a NaN in the ratio sort when
  both torrents have an infinite ratio
- [x] Phone layout (720 px and below): filters drawer, wrapping toolbar, a card list with
  a sort control and checkboxes, a selection action bar, details as a full-screen sheet.
  Checked headless at 390×844 (no horizontal scroll, no console errors) and at 1280×720.
  The column chooser's button is now pinned outside the column grid; at 1280 px the
  columns overflowed and pushed it out of sight
- [x] TLS: built-in rustls (ring) with bring-your-own certificates (reloaded), a
  self-signed certificate with pairing codes, and ACME (instant-acme) over TLS-ALPN-01
  and HTTP-01, with a DNS-01 plugin trait; tested against a local Pebble. See
  `plans/tls.md`. Cherry-picked from `t3/ae0674d1` on 2026-11-08. Settings shows the
  certificate, its pairing code and ACME progress (`GET /tls`); checked against a
  `--tls-self-signed` daemon, the code matching its console banner
- [x] M7 desktop app: Tauri 2 (`apps/desktop`, not a default workspace member),
  `apitorrent-client` (strict TLS: OS, private CA or a strict pin), storage-root markers
  and `GET /roots`, in-process API for Local mode, path mappings, tray, notifications,
  single instance, link handling. Cherry-picked from `t3/m7-tauri-desktop-app` on
  2026-11-09. The LAN listener serves the embedded UI (`apitorrent-webui`). Pairing uses
  the daemon's short code. See `plans/m7-desktop.md`
- [ ] M7 follow-ups: run the GUI once (needs your OK to use the screen, or a hidden
  desktop), build installers and test link registration, a folder picker, share locators
  on the daemon. Local mode's LAN listener now serves HTTPS with a self-signed certificate
  and shows its pairing code (on by default; plain HTTP still needs a proxy or consent).
  Not yet run end to end in the app
- [x] M10 (written, not yet run): `.github/workflows/ci.yml` (web; Rust on Linux, Windows
  and macOS with fmt, clippy, tests and an embedded-UI daemon build; the desktop app on
  all three; ACME against Pebble v2.10.1), `release.yml` (on a `v*` tag: daemons for
  x86_64 and aarch64 Linux, Windows and Apple silicon with the UI embedded, desktop
  installers through the Tauri CLI, provenance attestations, a **draft** release for a
  person to publish), `dependabot.yml`. Actions pinned by commit. `actionlint` passes.
  They run only once the repo is on GitHub, which waits on you
- [ ] Code signing for the desktop installers (Windows Authenticode, Apple Developer ID):
  needs certificates as repository secrets
- [ ] M8

## Open questions for the user

*(none right now)*

## How to run it

```sh
cd web && bun install && bun run build && cd ..
cargo run -p apitorrentd -- --web-root web/dist
# then open the /setup#token=… link the daemon prints, at http://localhost:8742
```

For UI development, run `bun run dev` in `web/` (Vite proxies `/api` to the daemon on
8742) and open http://localhost:5173.

## Things not to do

- Don't add an "ignore certificate errors" option anywhere.
- Don't use `rbuild`/sentinel for this repo until it's approved.
- Don't push or create a remote until the user asks.
- Don't fork `librqbit`'s session or storage. Depend on leaf crates instead.

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
  6. Magnets (BEP 9), PEX (BEP 11), LSD (BEP 14), then wiring in DHT, uTP and MSE ← current
  7. ~~v2 hash requests~~, ~~upload `cancel`~~, ~~webseeds (BEP 19)~~, ~~naming policy~~ (done)
  8. Content library: matching, bookends, piece reuse, fingerprints, pack switchover ← current
- **Delegated streams:**
  - `apitorrent-dht`: BEP 5, 32 and 42 (evaluate rqbit's crate first)
  - `apitorrent-utp` (BEP 29) and `apitorrent-mse` (MSE/PE)
  - M4 auth: passkeys, sessions, API tokens, so the API can leave loopback
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
- Known engine gaps: none blocking real-world use now that hash requests and cancels exist.
  Still missing: DHT/uTP/MSE wiring.
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
- [ ] DHT (delegated)
- [ ] uTP and MSE (delegated)
- [ ] M4 auth (delegated)
- [ ] Content library and pack switchover
- [ ] M3.2, M7, M8, M9, TLS, M10

## Open questions for the user

*(none right now)*

## How to run it

```sh
cd web && bun install && bun run build && cd ..
cargo run -p apitorrentd -- --web-root web/dist
# then open http://localhost:8742
```

For UI development, run `bun run dev` in `web/` (Vite proxies `/api` to the daemon on
8742) and open http://localhost:5173.

## Things not to do

- Don't add an "ignore certificate errors" option anywhere.
- Don't use `rbuild`/sentinel for this repo until it's approved.
- Don't push or create a remote until the user asks.
- Don't fork `librqbit`'s session or storage. Depend on leaf crates instead.

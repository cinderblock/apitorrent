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

- **M0: foundations** ← current
  1. Cargo workspace, licence, README, CI-ready layout (no remote).
  2. `apitorrent-bencode`: a strict codec, depth-limited, with canonical-encoding checks
     and a fuzz target.
  3. `apitorrent-metainfo`: v1, v2 and hybrid parsing, info-hashes (SHA-1 and SHA-256),
     file trees, BEP 47 pad files, merkle roots, magnet URIs, path sanitising.
  4. `apitorrent-wire`: sans-IO handshake and message codec, extension handshake.
- **M1: engine MVP** (design notes: per-torrent actor task owns the picker and
  `PieceHashes`; each peer connection is a tokio task doing socket I/O only and exchanging
  messages with the actor over channels; blocks are assembled in memory per piece, hashed
  on a blocking pool and written only once verified; a memory cap on in-flight pieces is
  still to do): TCP peers, a picker, storage (placement-based), HTTP and UDP
  trackers, resume state. Download from a local seeder in an integration test.
- **M2: core and server**: registry with SQLite, axum API, WebSocket sync, daemon binary.
- **M3: web UI**: torrent list (virtualised), details, add torrent, settings.
- **M4: auth**: bootstrap, passkeys, sessions, API tokens, CSRF.
- **M5: engine depth**: DHT, PEX, LSD, uTP, MSE/PE, magnet metadata, v2 merkle
  verification, webseeds, choker, rate limits, NAT.
- **M6: content library**: matching, staging, materialising.
- **M7: desktop**: Tauri app with local and remote modes, path mappings, TLS pinning.
- **M8: plugin host**: deno_core, the SDK, first plugins (qbittorrent-api, RSS, Ubuntu,
  Raspberry Pi).
- **M9: Deluge importer**, rules and retention, distribution channels.
- **M10: CI**: GitHub Actions when the repo goes public, including the interop matrix.

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
- [ ] M1.3 networking: peer connection task, torrent actor, session/listener ← current
- [ ] M1.4 loopback integration test: seeder session → leecher session, all versions
- [ ] M1.5 trackers: HTTP (strict TLS) and UDP (BEP 15)
- [ ] M1.6 resume state (bitfield and file sizes) and recheck

## Open questions for the user

*(none right now)*

## Things not to do

- Don't add an "ignore certificate errors" option anywhere.
- Don't use `rbuild`/sentinel for this repo until it's approved.
- Don't push or create a remote until the user asks.
- Don't fork `librqbit`'s session or storage. Depend on leaf crates instead.

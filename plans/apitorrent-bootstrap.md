# apitorrent: bootstrap plan

## Goal

Build a complete replacement for Deluge with:
- a fast, cross-platform core;
- a native Tauri desktop app that can also serve the web UI, or act as a thin client to a
  headless daemon (with path mappings to open remote folders over OS file sharing);
- a full web UI with passkey login;
- full plugin support;
- first-class file sharing between torrents ("packs");
- strict TLS: never accept an unverified certificate.

The project will eventually be public, MIT-licensed, with GitHub CI builds. For now it stays
**local only**.

The target design is in `docs/architecture.md`.

## Environment / context

- Repo: `C:\Users\camer\git\apitorrent`, git initialized 2026-11-06, branch `master`, no
  remote.
- Toolchain: rustc/cargo 1.99, bun 1.4.2, tauri-cli 2.10.1, VS 2022 Community (MSVC),
  CMake, Ninja, LLVM clang. No vcpkg and no Boost installed.
- Cargo is capped at `jobs = 4` machine-wide. Heavy builds go through
  `~/.srm/bin/srmctl run --slots 4 --low`.
- `rbuild`/sentinel is **not** approved for this repo.

## Decisions already made (don't re-ask)

- License: MIT. Local only until the user says to publish. *(user)*
- Language: Rust core; web UI in TypeScript and React, built with Vite and Bun; desktop on
  Tauri 2. *(user: Tauri; the rest is my choice)*
- Engine: libtorrent-rasterbar RC_2_0 behind our own `Engine` trait, with a mock engine for
  tests and UI work. Reasons are in `docs/architecture.md` → Engine. *(my choice; the
  user's brief allowed "libtorrent or a Rust equivalent")*
- TLS: expect a reverse proxy. Built-in rustls supports bring-your-own certificates and
  ACME HTTP-01. DNS-01 runs through a `dns-provider` plugin. *(user's brief; my plugin split)*

## Prior art (checked 2026-11-06, in answer to "are we recreating something that exists?")

No existing client has the whole combination, but many have parts of it:

- **qBittorrent**: libtorrent-based, mature, with a web UI. Its desktop app cannot act as a
  thin client. Its plugins are Python search plugins only. It has no passkeys and no
  content library. Its WebAPI is what the *arr, autobrr and cross-seed ecosystem speaks.
- **Deluge**: what we're replacing.
- **Porla**: a libtorrent daemon with a web UI, Lua plugins and JWT auth. Early stage, C++.
- **rqbit**: Rust, with a web UI and a Tauri wrapper. Its engine is weak: no encryption,
  sequential picking only, partial v2 support. **rustTorrent** is a fork of it.
- **Transmission**: its own C engine, a daemon and a web UI. Remote GUIs such as TrguiNG
  exist.
- **BiglyBT**: "swarm merging", which shares identical files across torrents. It matches by
  size only and is opportunistic. Java.
- **cross-seed / qui**: external tools that reuse existing files for new torrents through
  hardlinks or reflinks. Their docs warn that partial overlap can corrupt seeded files and
  recommend reflinks. This confirms the boundary-piece hazard in our design.
- **Flood**: a web UI on top of other clients.

## Plan / steps

1. **← current: write the architecture doc, then wait for the user's go-ahead after the
   prior-art question.**
2. M0: Cargo workspace, types, Engine trait, mock engine, core registry with SQLite, axum API
   and WebSocket sync, web UI skeleton.
3. M1: auth (bootstrap, passkeys, sessions, API tokens).
4. M2: libtorrent build spike on Windows (Boost headers, CMake, cxx shim), then the
   `engine-lt` backend.
5. M3: content library, plus the part-file / copy-on-write spike for boundary pieces.
6. M4: Tauri desktop (local and remote modes, path mappings).
7. M5: plugin host (wasmtime, WIT) and the first source plugins.
8. M6: Deluge importer and a qBittorrent-WebAPI compatibility shim (proposed).
9. M7: CI (GitHub Actions) when the repo goes public.

## Findings / gotchas

- No maintained Rust bindings for libtorrent exist. `libtorrent-sys` was last updated in
  2022 and `lt-rs` is 0.1, so we write our own cxx shim.
- rqbit 9.1 (October 2026) still lacks MSE/PE encryption, rarest-first picking and full
  BEP 52 support.

## Progress log

- [x] Research the engine options and prior art
- [x] `docs/architecture.md` first draft
- [ ] User's go-ahead on direction (see the open questions)

## Open questions for the user

1. Proceed with a new client, or build on something existing (an rqbit fork, a layer over
   qBittorrent, or contributing to Porla)? **Recommendation: build new, on libtorrent.**
2. Add a qBittorrent-WebAPI compatibility layer so Sonarr, Radarr, autobrr and cross-seed
   work unchanged? **Recommendation: yes, in M6.**

## Things not to do

- Don't add an "ignore certificate errors" option anywhere.
- Don't use `rbuild`/sentinel for this repo until it's approved.
- Don't push or create a remote until the user asks.

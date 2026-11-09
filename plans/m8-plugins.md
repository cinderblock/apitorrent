# M8: plugin host and first plugins

## Goal

Run third-party and first-party TypeScript plugins inside the daemon, sandboxed: each plugin
gets its own V8 isolate, can only do what its manifest declares and an admin approved, and
cannot touch the filesystem, processes, environment or sockets. Ship the SDK authors use, the
first `source` plugins (RSS/Atom/JSON feeds, Ubuntu releases), the management API and a
minimal Plugins page. The design was decided earlier: `docs/architecture.md`, "Plugins".

## Environment / context

- Worktree `C:\Users\camer\.t3\worktrees\apitorrent\t3-140ebf9a`, branch `t3/140ebf9a`,
  based on master `0f4d23b`. The main thread cherry-picks these commits onto master.
- Heavy cargo runs: `~/.srm/bin/srmctl run --slots 4 --low --label apitorrent-plugins -- cargo …`.
- Intermediate build artifacts live under `~/.cargo/build/<hash>/` (machine-wide
  `build-dir`), not `target/`.
- rustc 1.99, bun 1.4.2.

## Decisions already made (don't re-ask)

- **Runtime**: TypeScript on embedded V8 through `deno_core`, one isolate per plugin, ops
  checked against grants, curated globals; Bun only for authoring (build and test). *(user,
  2026-11-06; architecture doc)*
- **Globals are our own**, not Deno's extension crates. The architecture chose "embedded V8
  with our own globals". Checked on 2026-11-08: `deno_crypto` 0.274 pulls in `aws-lc-rs`
  (CMake and NASM on Windows, which the repo avoids by using ring); `deno_fetch` 0.284 has
  its own TLS stack with an "unsafely ignore certificate errors" option; `deno_web` 0.291
  brings BroadcastChannel, Blob URLs, Locks, CSS, geometry and a custom-libcxx V8 build, and
  still needs our own wiring of globals. Our JS sits on spec-grade Rust crates where it
  matters: `url` (WHATWG URL), `encoding_rs` (WHATWG encodings), `sha1`/`sha2`/`hmac`. *(mine)*
- **Crate split** (*mine*, to keep V8 out of most link steps):
  - `apitorrent-plugin-api`: manifest, capabilities, the settings-schema subset, and two
    traits: `AppApi` (what plugins may ask of the app; `apitorrent-core` implements it) and
    `PluginService` (what the server asks of the plugin host). No V8.
  - `apitorrent-plugins`: the V8 host; implements `PluginService`. Only it and
    `apitorrentd` link V8, so core and server tests stay fast.
  - The architecture doc called the host `apitorrent-plugin-host`; the task named it
    `apitorrent-plugins`, which wins. The doc is updated.
- **The API object is passed to `start(api)`**, not imported as a module: `export default
  definePlugin({ start(api) { … } })`. Bundles need no externals, and the Bun test harness
  just passes a mock `api`.
- **Management routes use a `/-/` segment** (GitLab's convention) so they can't collide with
  a plugin's own routes under `/api/v1/plugins/<id>/…`; plugin route paths may not start
  with `-`.
- **Upgrades that expand capabilities don't stop the running version.** The new version waits
  as `pending` until approved; the old one keeps running with its old grants.
- **Settings schemas are a JSON Schema subset** that the host validates and the UI renders;
  unsupported keywords are rejected at install, so the two never disagree. `writeOnly`
  properties (secrets) are never sent back by the API.
- **Network grants are host patterns**: `host`, `*.host`, `host:port`, `http://host…`.
  HTTPS-only unless `http://` is written. Redirects are followed by the host, one checked hop
  at a time. A host name that resolves to a loopback, private or link-local address is
  refused unless the grant names an IP literal or `localhost`, so a granted public name
  can't be pointed at the daemon or the LAN (DNS rebinding).
- **`networkFromSettings`** (added during the build): a manifest may name settings fields
  (`feeds[].url`, `downloadHosts[]`) whose values become grants: a URL grants its origin,
  anything else is read as a grant pattern. A feed reader can't list its feeds' hosts in a
  manifest; this keeps every host the plugin can reach one an admin typed in, and approval
  shows it. *(mine)*
- **Plugin responses are sandboxed**: the server forces `Content-Security-Policy: sandbox;
  default-src 'none'` and drops `Set-Cookie`, CSP and CORS headers from plugin routes, and
  plugins never see the caller's `Cookie` or `Authorization`. A plugin can't run script as
  the API's origin. *(mine)*
- **Plugin management is admin-only.** Install and approve also need a browser session
  with recent authentication (like creating an API token), because granting capabilities
  hands out privileges. Disable works with a full-scope token, for scripts and emergencies.

## Plan / steps

1. ✅ Check `deno_core` on Windows MSVC (see Findings).
2. ✅ `apitorrent-plugin-api`: manifest, capabilities and diffs, schema subset, traits; DTOs
   in `apitorrent-types::plugins` (`492a4e2`).
3. ✅ `apitorrent-plugins`: store, lifecycle, isolate threads, watchdog, heap and buffer
   limits, ops, JS globals, fetch (`73cde27`).
4. ✅ Tracker: `strict_http_client_builder` (`73cde27`).
5. ✅ Core `CorePluginApi`, server routes, daemon `--no-plugins` (`f760e52`).
6. ✅ `networkFromSettings` (`b4d20ed`).
7. ✅ `packages/plugin-sdk` (`7f320fd`).
8. ✅ Plugins: `rss` with the end-to-end test (`cd105cf`), `ubuntu-releases` (`556f31d`).
9. Web UI: `web/src/plugins/` ← current
10. Docs: README, architecture, this plan; daemon smoke test.

## Findings / gotchas

- **`deno_core` 0.413.0** (2026-10-21) is current. It depends on `deno_v8` 0.5, a new
  facade crate whose default feature selects `v8` (rusty_v8) **150.4.0**; an optional
  `v8x` backend (QuickJS) also exists. On this machine (Windows 11, MSVC, rustc 1.99) the
  `v8` build script downloads the prebuilt static library from
  `github.com/denoland/rusty_v8/releases` (`rusty_v8.lib`, 222 MB) and links without
  issues; a probe test evaluated `1 + 2` (2026-11-08 ~10:25 PM PST (06:25Z)). Mirrors:
  `RUSTY_V8_MIRROR` or `RUSTY_V8_ARCHIVE` for offline CI.
- deno_core API drift: `JsRuntime::handle_scope()` is gone; use `deno_core::scope!(scope,
  &mut runtime)`.
- **Linker warning LNK4098** (`defaultlib 'libcmt.lib' conflicts with use of other libs`)
  on every binary that links V8: the prebuilt `rusty_v8.lib` carries a static-CRT default
  lib while Rust links the dynamic CRT. rusty_v8's build script supports both CRTs and the
  binaries run fine. Deno itself builds with `+crt-static`; switching this whole workspace
  is a bigger decision, so it is left as a warning (it shows under `cargo test`, not
  clippy). If CI ever builds with `-D warnings` through `RUSTFLAGS`, allow
  `linker_messages` for the V8-linking crates or move to `+crt-static`.
- **The heap limit doesn't cover ArrayBuffers.** A probe allocated 4 GiB of `Uint8Array`s
  under a 32 MiB heap limit. Fixed with a per-isolate counting allocator (`alloc.rs`, the
  crate's only `unsafe`, via `v8::new_rust_allocator`); refused allocations are a
  `RangeError` the plugin can catch.
- **A refused ArrayBuffer allocation fires the near-heap-limit callback.** V8 answers the
  refusal with a last-resort GC that also calls the callback (seen with `current=44040192`,
  heap nowhere near full), which first killed the plugin as "out of memory". The budget now
  flags refusals and the callback leaves the limit alone for them. V8 also counts buffers
  toward its own limit, so the isolate's limit is heap + buffer budget.
- **WebAssembly is removed** from plugin globals: wasm memories bypass the ArrayBuffer
  allocator. `SharedArrayBuffer` and `Atomics` go too (no threads to share with).
- **Extension JS must be 7-bit ASCII** (`deno_core` panics at isolate creation otherwise:
  "Extension code must be 7-bit ASCII"). A unit test guards `src/js/`.
- deno_core puts `Deno`, `__bootstrap`, `__infra` and `__timers` on `globalThis`; the
  bootstrap deletes them. It has no exception hook for timers or microtasks: one uncaught
  throw ends the event loop, so the bootstrap wraps every callback and installs an
  unhandled-rejection handler that logs.
- `NotCapable` is already a registered error class in deno_core (Deno 2 uses it for
  permissions); ops throw it for denied calls. `core.registerErrorBuilder` gives
  `DOMException` classes (`DOMExceptionAbortError`, …).
- JSON strings, not `serde_v8`, carry structured data between ops and JS, so 64-bit sizes
  arrive as plain numbers (a test checks a 5 GB size).
- An isolate must be created inside a tokio runtime context, or V8's delayed tasks abort the
  process ("V8 posted a delayed task, but this isolate was created outside of a tokio
  runtime context").
- Nested axum routers see the URI without `/api/v1`; plugin routes use `OriginalUri` to
  give plugins the URL the client used.
- **Disk full, 2026-11-09 ~12:30 AM PST (08:30Z):** C: hit 0 bytes free mid-build
  (`~/.cargo/build` was 230 GB against the 150 GB cap the daily prune enforces). I removed
  only this worktree's incremental cache (3 GB) and built with `CARGO_INCREMENTAL=0`; later
  the disk was back to 49 GB free. Machine-wide; reported to the main thread.
- Bash heredocs in this environment break on some quoting; edits went through scripts in
  the scratchpad.

## Progress log

- [x] deno_core / V8 build check on Windows MSVC
- [x] Contract crate, types, auth `require_recent`/`audit`
- [x] Host: isolation, watchdog, limits, ops, globals, fetch; 11 host tests
- [x] Core, server, daemon wiring; server tests with a fake service
- [x] `networkFromSettings`
- [x] SDK (types, harness, build) with tests
- [x] `rss` (7 Bun tests, Rust end-to-end test), `ubuntu-releases` (2 Bun tests)
- [ ] Web UI Plugins page
- [ ] README, architecture doc, daemon smoke test

## Open questions for the user

*(none yet)*

## Things not to do

- Don't add an "ignore certificate errors" option, for plugins or anything else.
- Don't give plugins `Deno.core`: the bootstrap captures what it needs and deletes it.
- Don't run `AppApi` futures on a plugin's thread: core spawns tasks, and they would die with
  the plugin's runtime. They run on the daemon's runtime.

# M7: the Tauri 2 desktop app

## Goal

Build `apps/desktop`, the Tauri 2 desktop app from `docs/architecture.md` ("Process
model", "Client trust", "Remote file access"). Remote mode (a thin client to a headless
daemon on a seedbox) is the main production use and comes first; Local mode (the core
embedded in the app, optionally serving the web UI and API on the LAN) comes second.
Delegated stream from `plans/apitorrent-bootstrap.md`.

## Environment / context

- Worktree `C:\Users\camer\.t3\worktrees\apitorrent\t3-ce452977`, branch `t3/ce452977`,
  off `0f4d23b`. Never push, never add a remote, never touch `master`.
- The main thread cherry-picks these commits onto master, where TLS for the daemon and the
  plugin host are being built in parallel. Keep shared files (`apitorrentd/src/main.rs`,
  `apitorrent-server/src/lib.rs`, `web/src/**`, root `Cargo.toml`) minimally touched.
- Toolchain: cargo 1.99, tauri-cli 2.10.1, bun 1.4.2. Tauri crates: `tauri` 2.12,
  plugins single-instance 2.5, deep-link 2.6, notification 2.8, opener 2.7 (3.0 is alpha).
- Heavy builds: `~/.srm/bin/srmctl run --slots 4 --low --label apitorrent-desktop -- cargo …`.
- Never show the GUI on the user's screen. Build only; any run must be hidden.

## Decisions already made (don't re-ask)

- **Passkeys don't work for the desktop app; it signs in with a scoped API token kept in
  the OS keychain** (`keyring` crate). Investigation, 2026-11-08:
  - The webview's origin is the app's (`http://tauri.localhost` on Windows,
    `tauri://localhost` elsewhere). A passkey's RP ID must be the origin's host or a
    parent of it, so the bundled UI can never use the daemon's passkeys.
  - The only way around that is to load the daemon's own origin in the webview. Then:
    the webview's TLS stack, not ours, verifies the server, so a pinned self-signed key
    or a private CA fails (or would need "ignore certificate errors", which is banned);
    and desktop IPC would have to be granted to a remote origin.
  - Even then, WKWebView (macOS) allows WebAuthn only in entitled browsers or for
    domains in the app's signed Associated Domains entitlement, which can't name each
    user's own seedbox. WebKitGTK (Linux) has no WebAuthn at all. Only WebView2 (Windows)
    would work.
  - So: a token (`apt_…`, scope `read` or `full`) made on the daemon's Account page,
    pasted into the profile once, stored in the keychain, sent by Rust as
    `Authorization: Bearer`. The webview never sees it.
- **All remote traffic goes through Rust**, never the webview's network stack: HTTP
  through a Tauri command, the sync WebSocket through a Tauri channel. That is what makes
  pinning and private CAs possible and keeps the token out of JavaScript.
- **Trust is one of three kinds per profile**: OS verifier (`rustls-platform-verifier`),
  an imported private CA (webpki with only that CA as root), or a pinned SPKI SHA-256.
  The pinned verifier accepts the pinned key, or else a chain the OS verifier accepts;
  nothing else. Handshake signatures are always verified. No "ignore certificate errors".
- **The client lives in a new crate, `crates/apitorrent-client`** (the architecture's
  planned crate): trust, HTTP and sync over TLS. It has no Tauri dependency, so it is
  fast to build and test, and the CLI can reuse it.
- **Workspace layout**: `apps/desktop/src-tauri` is a workspace member, but not a default
  member (`default-members = ["crates/*"]`), so plain `cargo build`/`cargo test` at the
  root skip the slow Tauri build while `--workspace` and `-p apitorrent-desktop` include
  it. One lockfile and one target dir, so core and engine compile once for both.
- **Local mode talks to the embedded server in memory**, not over a socket: requests go
  through the axum router directly and the sync socket over an in-memory HTTP connection.
  Such requests carry an in-process marker that network requests can't forge, and are the
  local owner. A LAN listener, when enabled, uses the normal auth rules.
- **Profile switching reloads the webview**, so the web app's module-level clients pick
  up the new transport without a rewrite.

## Plan / steps

1. Plan doc ← (this)
2. `crates/apitorrent-client`: trust kinds, pin parsing, pinned verifier, HTTP, sync,
   certificate probe; unit and loopback TLS tests.
3. `.apitorrent-root` marker in core plus `GET /api/v1/roots` (separate commit).
4. Embedded server transport in `apitorrent-server` (in-process owner).
5. `apps/desktop/src-tauri`: profiles (JSON + keychain), path mappings, commands, Local
   mode runner, tray, notifications, single instance, magnet/.torrent handling.
6. `web/src/desktop/`: capability check, transport hooks, profile and mapping UI, reveal
   buttons in the details pane.
7. Builds: clippy, tests, `cargo build -p apitorrent-desktop`; bun typecheck and tests.
8. Docs: architecture, README, this plan.

## Findings / gotchas

*(filled in as work proceeds)*

## Progress log

- [ ] 1 plan
- [ ] 2 client crate
- [ ] 3 root marker
- [ ] 4 embedded transport
- [ ] 5 desktop crate
- [ ] 6 web
- [ ] 7 builds
- [ ] 8 docs

## Open questions for the user

*(none yet)*

## Things not to do

- Don't add an "ignore certificate errors" option, or any verifier that accepts
  unverified certificates.
- Don't load a remote daemon's origin in the webview, and don't give a remote origin IPC.
- Don't put tokens in JavaScript, URLs or the profile JSON: keychain only.
- Don't launch the GUI on the user's screen.

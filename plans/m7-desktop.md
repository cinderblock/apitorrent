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
  The pinned verifier accepts only the pinned key (decided 2026-11-09: strict, since an
  OS fallback would let a publicly issued certificate stand in for the paired server);
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

1. Plan doc
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

All eight are done. Follow-ups are under "Open questions" and "Not done".

## Layout (for the merge)

- `crates/apitorrent-client/` (new crate): `trust.rs` (Trust, SpkiHash, PinnedVerifier,
  probe recorder), `remote.rs` (Remote: reqwest + tokio-tungstenite over tokio-rustls).
- `crates/apitorrent-core/src/roots.rs` (new) plus 10 lines in core `lib.rs` (`mod roots`,
  an `Inner.roots` field, two lines in `start`, `storage_roots()`); `rand` dep.
- `crates/apitorrent-types`: `StorageRoot`, `RootMarker` (+ generated TS).
- `crates/apitorrent-server`: `embed.rs` (new), one `.route("/roots", …)` line and two
  `mod`/`pub use` lines in `lib.rs`, a `roots` handler and a `storage-roots` capability in
  `routes.rs`, the in-process check in `authn.rs`, `tower` promoted to a normal dep.
- `apps/desktop/src-tauri/` (new): `profiles.rs`, `mapping.rs`, `links.rs`, `finish.rs`
  (pure, unit-tested), `backend.rs`, `local.rs`, `state.rs`, `notify.rs`, `commands.rs`,
  `lib.rs`; `tauri.conf.json`, `capabilities/default.json`, `icons/` (from `icon.svg`).
- Root `Cargo.toml`: `members` gains `apps/desktop/src-tauri`, new `default-members`, a
  workspace dep for `apitorrent-client`.
- `web/`: `src/api/transport.ts` (new seam), `src/desktop/*` (new), and small hooks in
  `main.tsx`, `auth/AuthGate.tsx`, `App.tsx`, `components/AddTorrent.tsx` (an
  `initialMagnet` prop), `components/DetailPane.tsx` (path actions), `api/client.ts` and
  `api/sync.ts` (use the transport); `@tauri-apps/api` 2.12.4.

## Findings / gotchas

- **Passkeys in the webview: don't work**, see Decisions. Sources checked 2026-11-08:
  WebKit bug 250912 / Apple forum threads (WKWebView allows WebAuthn only for entitled
  browsers or matching Associated Domains); no WebAuthn in WebKitGTK.
- **WebSockets can't go through a Tauri custom protocol**, so the sync socket runs in Rust
  and its messages go to the page over a `tauri::ipc::Channel`. The web side wraps that in
  a `SocketLike` (`DesktopSocket`) and fires real `MessageEvent`/`CloseEvent`s, so
  `SyncClient` is unchanged; a 401 on the handshake becomes close code 4401.
- **axum 0.8's `serve::Listener` trait** makes an in-memory server easy: a listener that
  yields `tokio::io::duplex` ends from a channel. `ConnectInfo` can't be implemented for a
  custom listener (orphan rule), so `embed.rs` inserts `ConnectInfo` and the in-process
  marker with a `map_request` layer instead.
- **reqwest hides the cause** of a TLS failure behind "error sending request"; the pin
  mismatch travels as a `rustls::Error::InvalidCertificate(Other(..))` inside io errors.
  `trust::find_pin_mismatch` walks `source()` and io `get_ref()` to find it, so the UI can
  show the key the server actually presented.
- **`tauri::generate_context!` needs `web/dist` to exist at compile time** (it embeds the
  UI), so `cargo build -p apitorrent-desktop`, clippy and tests need `bun run build` in
  `web/` first. CI must build the web UI before any `--workspace` cargo command.
- **`beforeBuildCommand.cwd` is relative to the Tauri config's directory**
  (`apps/desktop/src-tauri`), not to `apps/desktop`: `"../../web"` failed with
  `The directory name is invalid. (os error 267)`; `"../../../web"` is right. (Verified
  with `cargo tauri build --debug --no-bundle`; see the progress log.)
- `cargo tauri icon` takes an SVG directly. Only the desktop sizes are committed (the
  Android/iOS/Windows Store sets were deleted).
- `keyring` 4.x: the `v1` default feature keeps the old `Entry::new(service, user)` API on
  top of `keyring-core`, with the native stores (Credential Manager, Keychain, Secret
  Service).
- Rust 2024 edition: a returned `impl Fn` captures every argument lifetime; a test helper
  needed `+ use<>`.
- Tauri passes `State` by value, so clippy's `needless_pass_by_value` fires on sync
  commands; allowed there with a comment.
- The bash tool turned a `\\n` inside a Python heredoc into a real newline in TSX once;
  check generated edits (or use the Edit tool).

## Progress log

Times are US Pacific (UTC alongside).

- [x] 1 plan (2026-11-08 ~10:25 PM PST, 06:25Z)
- [x] 2 client crate: 10 unit + 6 loopback TLS tests (pinned, wrong pin, private CA, other
  CA, OS refusing self-signed, probe) (`8a1258e`)
- [x] 3 root marker and `GET /roots`, with a server test (after autosquash: `2878061`)
- [x] 4 embedded transport, with a server test (`79ea41d`)
- [x] 5 desktop crate: 24 unit tests (profiles, mapping, links, finish watcher, navigation)
- [x] 6 web: transport seam, profiles and mapping UI, path actions, link handling; 5 new
  bun tests (24 total) with Tauri's `mockIPC`
- [x] 7 builds: `cargo build -p apitorrent-desktop` and clippy clean; `cargo tauri build
  --debug --no-bundle` (hook + embedding check)
- [x] 8 docs: architecture (desktop, client trust, remote file access), README (desktop
  app, development), this plan

## Not done (follow-ups)

- **Installers weren't built** (bundling is optional; no NSIS/WiX run). The bundle config
  registers `magnet:` (deep-link plugin) and `.torrent` (fileAssociations); that path is
  untested until someone runs `cargo tauri build`.
- ~~The GUI was never launched~~: first run 2026-11-09, with the user's OK, against a
  loopback daemon (Remote profile, pairing, sync, adds, deep link, keychain). See the
  bootstrap plan's progress log. Not yet run: Local mode, the tray, notifications.
  - **How to drive it**: build with `cargo build -p apitorrent-desktop --features
    tauri/custom-protocol` (a plain debug build loads the UI from Vite on :5173), start it
    with `APITORRENT_WEBVIEW_DEBUG_PORT=9333`, and attach Playwright with
    `connect_over_cdp("http://127.0.0.1:9333")`. A debug build registers itself as the
    `magnet:` handler under `HKCU\Software\Classes\magnet`, and keeps data in
    `%APPDATA%\apitorrent` and `%LOCALAPPDATA%\io.github.cinderblock.apitorrent`: remove
    all three afterwards.
- **No end-to-end test against a TLS daemon**: the daemon's built-in TLS is being built in
  parallel. The client is tested against rustls servers on 127.0.0.1 instead.
- No native folder picker (paths are typed); `tauri-plugin-dialog` would add one.
- Share locators (UNC/smb/nfs hints per root) aren't in the daemon yet; suggestions use
  the root's own path.
- Device credentials bound to the pinned key (architecture "Device pairing").

## Open questions for the user

*(none: the four below were settled by the main thread on 2026-11-09, at merge)*

1. **Pairing code format**: the daemon prints a short code (the first 100 bits of the SPKI
   SHA-256, base32, `ABCD-EFGH-IJKL-MNOP-QRST`). The client shows the same code for the
   key a server presents, accepts it (or the full hash) when pairing, and pins the full
   hash. 100 bits is far beyond what a key-grinding attacker can match.
2. **Pins are strict**: only the pinned key. An OS fallback would let any publicly issued
   certificate for the name stand in for the server the user paired with.
3. **Plain HTTP to a LAN daemon stays refused**; only loopback `http://`.
4. **Local mode on the LAN**: rather than passwords over plain HTTP, the LAN listener
   uses the built-in TLS: a self-signed certificate in the profile's data directory, its
   pairing code shown in the app (`LanSettings.https`, on by default). Done 2026-11-09.

## Things not to do

- Don't add an "ignore certificate errors" option, or any verifier that accepts
  unverified certificates.
- Don't load a remote daemon's origin in the webview, and don't give a remote origin IPC.
- Don't put tokens in JavaScript, URLs or the profile JSON: keychain only.
- Don't launch the GUI on the user's screen.

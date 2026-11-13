# Signed updates for the desktop app and the daemon

## Goal

Both the desktop app and the daemon can update themselves. They install only an update
signed with apitorrent's own key, so no paid code-signing certificate is needed.

## Decisions already made (don't re-ask)

From the user, 2026-11-09:

- No Authenticode or Apple Developer ID certificates for now.
- Both the desktop app **and the daemon** update themselves.
- The private signing key lives **only in GitHub Actions secrets**.

## Design

### The key

- One minisign (Ed25519) key pair. This is the format Tauri's updater already uses, and
  the daemon checks it with the `minisign-verify` crate.
- The public key is committed at `release/update-signing.pub` and compiled into both
  programs.
- While the public key file is missing, update installs are switched off at build time.
  The program can still say a new version exists.

### Creating the key

The key is created only once the GitHub remote exists:

- A script in the repo generates the key pair.
- It stores the private key with `gh secret set`.
- It deletes the local copy.

The user approves that step when it comes. Tests use a key made just for the test.

### Release workflow

- `release.yml` signs every artifact.
- It writes two signed manifests:
  - Tauri's `latest.json`
  - a daemon manifest, signed, listing each version, target, asset URL and SHA-256
- Secret names:
  - `TAURI_SIGNING_PRIVATE_KEY`
  - `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`
- The release stays a draft until a person publishes it. Programs only look at published
  releases.

### The daemon

- It checks GitHub's latest published release once a day (a setting, and it can be
  turned off).
- When an update exists, it tells server admins in the UI.
- An admin installs it with the API (`/api/v1/system/update`) or with
  `apitorrentd update`. The daemon never installs on its own unless a setting allows it.
- The install:
  - downloads the manifest and the binary
  - checks the signature, the SHA-256 and that the version is newer (no downgrades)
  - swaps the binary into place (Windows: move the running exe aside first)
  - restarts. Under systemd this is an exit that systemd restarts; otherwise it
    re-executes itself.

### The desktop app

- `tauri-plugin-updater` with the same public key.
- Local mode's bundled daemon updates along with the app.

## Implementation (2026-11-09, worktree branch `t3/86a72dbc`)

### Environment

- Worktree `C:\Users\camer\.t3\worktrees\apitorrent\t3-86a72dbc`, branch `t3/86a72dbc`,
  off `b6e6f8b`. No push, no remote, no real key.
- Builds: `~/.srm/bin/srmctl run --slots 4 --low --label apitorrent-update -- cargo …`.

### Layout

- **`crates/apitorrent-update`** (new): everything the daemon needs, with no server or
  Tauri dependency.
  - `key`: the public key, compiled in from `release/update-signing.pub` by `build.rs`
    when the file exists. Accepts Tauri's format (base64 of a minisign key file) and plain
    minisign files. Signatures likewise (`.sig` from `tauri signer sign`).
  - `manifest`: the signed daemon manifest, `apitorrentd-manifest.json` plus
    `apitorrentd-manifest.json.sig`.
  - `github`: the latest *published* release (`/releases/latest` never returns drafts or
    prereleases).
  - `install`: download, check size and SHA-256, swap the binary (Windows: rename the
    running exe to `<exe>.old`, which is removed on the next start).
  - `Updater`: state, settings (`<data dir>/update.json`), daily check, install.
- **`apitorrent-types::update`**: `UpdateStatus`, `UpdateSettings` (TS generated).
- **`apitorrent-server`**: an `UpdateService` trait (like `PluginService`), routes under
  `/api/v1/system/update`, admin only, audited. `ServerConfig.updates: Option<…>`.
- **`apitorrentd`**: `update [--check]` subcommand; wires the `Updater` into the server;
  on install, shuts down gracefully, then exits (systemd) or re-executes itself.
- **Web UI**: an admin toolbar button ("Update to x.y.z", opening the details and "Install
  and restart") when an update exists, and an "Updates" section in Settings. (A button,
  not a banner row: the app's grid has fixed rows.)
- **Desktop**: `tauri-plugin-updater` with the same key, given at runtime, so a build
  without the key file still compiles and only reports new versions. Local mode runs the
  core in the app's own process, so the app update *is* the daemon update; the in-process
  server gets no `UpdateService`.
- **Release**: every artifact gets a `.sig` from `tauri signer sign` with the secret; the
  desktop job sets `createUpdaterArtifacts` only when the secret exists; a final job
  writes `latest.json` and the daemon manifest and signs both. Bare daemon executables
  (`apitorrentd-<tag>-<target>[.exe]`) are uploaded beside the archives so the daemon
  needs no tar or zip code. `scripts/release-manifests.ts` writes the manifests and
  refuses a tag that differs from the workspace version.
- **Key script**: `scripts/update-signing-key.ts` (Bun, cross-platform).

### Manifest format

```json
{
  "product": "apitorrentd",
  "version": "1.2.3",
  "pubDate": "2026-11-09T20:54:00Z",
  "notesUrl": "https://github.com/cinderblock/apitorrent/releases/tag/v1.2.3",
  "assets": [
    { "target": "x86_64-unknown-linux-gnu",
      "url": "https://…/apitorrentd-v1.2.3-x86_64-unknown-linux-gnu",
      "sha256": "<hex>", "size": 12345678 }
  ]
}
```

The install trusts nothing outside the signed bytes: the version comes from the manifest
(not the tag), and so do the target, URL, size and hash.

## Findings / gotchas

- **Tauri CLI 2.10.1's signatures don't carry the version.** `tauri signer sign` writes
  the trusted comment `timestamp:…	file:<name>` only (probed with a throwaway key,
  2026-11-09). `tauri-plugin-updater` 2.14's `requireSignedVersion` needs a
  `version:` entry there, so it stays off: a forged `latest.json` could pair a higher
  version with an older signed installer (that needs control of the release assets).
  Turn it on once the CLI records versions, after re-signing what users can update from.
  The daemon's manifest is signed as a whole, so it has no such gap.
- **`tauri-plugin-updater`'s config requires `pubkey`** (not `serde(default)`), so
  `tauri.conf.json` has `"pubkey": ""`; `Builder::pubkey` sets the real one at runtime
  when the key was compiled in. An empty key verifies nothing.
- **On Windows, `update.install()` runs the installer and exits the process**, so the app
  saves a Local profile's core (`state::disconnect`) before calling it.
- Tauri's key files are base64 of the whole minisign file (`untrusted comment: …`); its
  `.sig` files likewise. `apitorrent-update` accepts both that and plain minisign. A real
  CLI-made public key and signature are a test fixture
  (`crates/apitorrent-update/tests/fixtures`, private half deleted).
- `cargo:rerun-if-changed` on a missing file reruns the build script every build, so
  `apitorrent-update/build.rs` watches the `release/` directory instead (which now always
  exists, with a README).
- Running-exe swap test: a copy of the test binary runs an ignored test that sleeps;
  Windows refuses `fs::write` over it, and the aside-rename swap succeeds. Passes on
  Windows here.
- There are no non-admin users yet (only admins can be created), so the server test
  checks the admin-only rule through a read-only token instead.
- Disk on C: was at 3–7 GB free throughout (other threads' builds). Built with
  `CARGO_INCREMENTAL=0` and deleted this worktree's test `.exe`/`.pdb` files between runs.

## Progress log

Times US Pacific.

- [x] Design written (2026-11-09)
- [x] Implementation (agent in worktree `t3/86a72dbc`, 2026-11-09)
  - [x] `apitorrent-update` crate: 15 unit tests (signatures good/bad/wrong key/legacy,
    manifest checks, downgrade and same version refused, digest, swaps, writable probe,
    systemd detection, daily schedule) and 10 integration tests against a loopback
    GitHub-shaped server (install, tampered binary, other key, altered manifest,
    downgrade, no key = installs off, settings persist, running-exe swap, Tauri CLI
    fixture) (`9062a2d`, `223904f`)
  - [x] types + server routes, 3 server tests (`9062a2d`)
  - [x] daemon wiring, `update [--check]` subcommand, restart (`9062a2d`)
  - [x] web UI toolbar button and Settings section (`cf0dca2`)
  - [x] desktop updater, plus a bridge test (`223904f`)
  - [x] release workflow, manifest script with 4 tests, key script (`626aa40`)
  - [x] docs: architecture "Updates", README "Updates", `release/README.md`
- [x] Key generated 2026-11-10 with `bun scripts/update-signing-key.ts`. The private key
  and its password are the repo's Actions secrets `TAURI_SIGNING_PRIVATE_KEY` and
  `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`. The public key, `release/update-signing.pub`
  (key ID 4EAD1BDC7E95B474), is committed
- [x] A real release run in Actions: `v0.1.2`, 2026-11-12 12:10 AM PST (08:10Z), run
  43178391333, every job green. The draft holds 18 files, each with a `.sig`, plus
  `latest.json` and `apitorrentd-manifest.json`, both signed. Checked by hand: the Windows
  daemon's SHA-256 matches the manifest; the manifest, `latest.json` and that binary
  verify against `release/update-signing.pub` (key ID 4EAD1BDC7E95B474, prehashed `ED`,
  trusted comment too), and a one-bit change fails.
  - `v0.1.0` failed: the bundler decodes `plugins.updater.pubkey`, empty in
    `tauri.conf.json` ("Missing comment in public key"). Fixed in `dc68522`: the workflow
    passes the committed key with `--config`.
  - `v0.1.1` failed at the draft job: on Linux and macOS the bare daemon executable has
    the archive folder's exact name and was copied into it. Fixed in `d0ab53a`. The
    version moved on rather than moving a pushed tag; the user later approved deleting
    both tags (2026-11-12).
- [x] Published `v0.1.2` (2026-11-12 1:41 PM PST, 21:41Z) and `v0.1.3` (2026-11-13
  10:59 AM PST, 18:59Z), both on the user's say-so. `v0.1.3` built first time (run
  43485678004).
- [x] Daemon updates tested end to end, 0.1.2 → 0.1.3 with the published binaries
  (2026-11-13 ~11 AM PST), all on loopback with `--insecure-no-auth-loopback`:
  - Windows, `apitorrentd update`: swapped in place, `.old` kept until the next start; the
    new exe's SHA-256 matches the manifest.
  - Windows, running daemon, `POST /system/update/install`: back as 0.1.3 within 2 s
    (re-exec spawns the new exe), the old binary removed on start, one process left.
  - Linux (WSL Ubuntu), CLI: same result. Running daemon: `exec` in place, same PID.
  - Linux under systemd (`systemd-run --user -p Restart=on-failure`): exits 75, systemd
    restarts it (`NRestarts=1`), new PID, 0.1.3.
- [x] Desktop update manifest for 0.1.3: `latest.json` lists every platform; its
  signatures equal the `.sig` files, and `latest.json`, the Windows setup `.exe` and the
  AppImage verify against the committed key.
- [ ] Not run: the desktop app installing an update from inside the app. It needs a GUI
  session in the foreground; best done by the user installing 0.1.2 and letting it offer
  0.1.3 (now superseded by whatever is latest).

## Follow-ups (not done)

- `requireSignedVersion` for the desktop updater, once the Tauri CLI records versions
  (see Findings).
- ~~A `--no-update-check` flag or env for package-managed installs~~ Done 2026-11-10:
  `--no-update-check` / `APITORRENT_NO_UPDATE_CHECK` (`plans/invitations.md`, "Follow-ups
  round 2").
- The daemon as a Windows service: restart there would need the service manager's
  restart-on-failure; only console and systemd are handled.

## Things not to do

- Don't commit a private key, not even a test one beyond a throwaway made in the test.
- Don't install without a valid signature, and don't add an "accept unsigned" switch.

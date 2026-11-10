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

## Progress log

- [x] Design written (2026-11-09)
- [ ] Implementation: delegated to a background agent in a worktree
- [ ] Generate the key and store it as GitHub secrets, once the remote exists (needs the
  user)

## Things not to do

- Don't commit a private key, not even a test one beyond a throwaway made in the test.
- Don't install without a valid signature, and don't add an "accept unsigned" switch.

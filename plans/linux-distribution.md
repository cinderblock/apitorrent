# Linux distribution for the daemon

## Goal

Make `apitorrentd` easy to install and keep current on a headless Linux server, in the
ways people expect: our own install script (systemd), `.deb`/`.rpm` files on each
release, an apt/dnf repository that keeps them current, and a Docker image. Prompted by
the user wanting to try apitorrent on their seedbox next to Deluge
(`plans/deluge-coexistence.md`).

## Environment / context

- Releases: `.github/workflows/release.yml`, on a `v*` tag. It builds a **draft**
  release; a person publishes it. Programs only see published releases.
- Today the daemon ships as a bare binary and a `.tar.gz` per target
  (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, Windows, macOS), each with a
  minisign `.sig`, plus the signed `apitorrentd-manifest.json` that self-update reads.
- The `.deb`/`.rpm`/`.AppImage` on releases are the **desktop app** (package name
  `apitorrent`). The daemon package must not clash: it is `apitorrentd`.
- v0.1.3 Linux binaries: 92 MB bare, ~30 MB `.tar.gz`. Built on `ubuntu-latest`
  (24.04), so they need glibc 2.39: Debian 12, Ubuntu 22.04 and RHEL 9 can't run them.
- TLS: rustls + `rustls-platform-verifier`, so the OS CA store must exist
  (`ca-certificates` in any container image). SQLite is bundled; no OpenSSL.
- Every daemon flag has an `APITORRENT_*` environment variable, so an `EnvironmentFile`
  configures it.
- Self-update replaces the executable in place (needs write access to its directory) and,
  under systemd (`INVOCATION_ID` set), exits 75 for `Restart=on-failure` to restart it.
- Testing: sentinel has Docker (it also runs production containers: use unique names,
  clean up). The repo is public, so GitHub Actions and ghcr.io are free.

## Decisions already made (don't re-ask)

- **All four channels**, per the user (2026-11-13): our own install script using
  systemd; `.deb`/`.rpm` as release assets; an apt/dnf repository; a Docker image.
- **Repository hosting: GitHub Pages**, static, rebuilt by Actions when a release is
  published. Cloudflare Pages can't hold the packages (25 MiB file limit) and any
  Cloudflare change needs per-change approval; Pages on our own public repo needs
  neither. apt's `Filename:` must be inside the repo, so the packages live on Pages too
  (last 3 versions, well under the 1 GB site limit).
- **Two update paths, never both:** the install script's install updates itself (binary
  in `/opt/apitorrent`, owned by the service user); a package install updates through
  apt/dnf, and its unit sets `APITORRENT_NO_UPDATE_CHECK=true`.
- **A `.deb`/`.rpm` adds the repository** on install (as Chrome and VS Code do), so even a
  downloaded package keeps itself current.
- **Service user is configurable.** Default is a system user `apitorrent`, but running as
  Deluge's user is the expected case for people moving from Deluge (it must read Deluge's
  config and write the same files). Script: `--user`. Package: a systemd drop-in.
- No `ProtectHome=` in the unit: Deluge downloads often live under `/home`.

## Plan / steps

1. [x] **Install script + unit** (`packaging/linux/`): `install.sh` with `--user`,
   `--data-dir`, `--download-dir`, `--version`, `--binary PATH` (an unreleased build),
   `--uninstall`; writes `/etc/apitorrent/apitorrentd.env` (kept on re-run) and the unit;
   enables and starts it; prints where the setup token is and the SSH tunnel command.
   Tested in a systemd container on sentinel. Also a release asset, and in the README.
2. [ ] ← current **Portable Linux binaries:** build the `*-linux-gnu` targets against an older glibc
   (cargo-zigbuild, `x86_64-unknown-linux-gnu.2.28`) so the same target names keep
   self-update working for existing installs. Strip debug info to shrink them.
3. [ ] **`.deb`/`.rpm` for the daemon** with nfpm in `release.yml`: binary, unit, env
   file (config, not replaced), system user, repo source + key. Signed like other assets.
4. [ ] **apt/dnf repository on GitHub Pages:** workflow on `release: published` (and by
   hand) rebuilds it from the last 3 releases' packages, signs with a repo GPG key kept in
   Actions secrets; a script makes the key (like `scripts/update-signing-key.ts`).
5. [ ] **Docker image** `ghcr.io/cinderblock/apitorrent`: multi-arch, from the release
   binaries, `ca-certificates`, `/data` volume; docs recommend `--network host` (peer
   ports, and the API stays on loopback).
6. [ ] **Docs:** README install section per channel (the script's is written); the
   `docs/architecture.md` "Services" bullet is fixed.

## Findings / gotchas

- `docs/architecture.md` ("Cross-platform notes" → Services) claims service files exist;
  none are in the repo (checked 2026-11-13).
- `/tmp` is often mounted `noexec` (Docker's `--tmpfs` is too): the installer can't test a
  downloaded binary there. It tries it as `/opt/apitorrent/apitorrentd.new` instead.
- Switching `--user` left `/var/lib/apitorrent` owned by the old user, and the daemon
  crash-looped. The installer now hands its own data directory to the new user, and
  refuses a data directory the user can't write.
- Testing systemd in Docker on sentinel needs a writable cgroup: run with `--cgroupns=private
  --cap-add SYS_ADMIN --security-opt seccomp=unconfined --security-opt apparmor=unconfined
  --tmpfs /run --tmpfs /run/lock` and `sh -c "mount -o remount,rw /sys/fs/cgroup && exec
  /sbin/init"` (not `--privileged`). Test files are in `~/apitorrent-install-test` there.
- A container's loopback isn't the host's: with bridge networking the daemon sees the
  Docker gateway, not loopback, and refuses plain HTTP. Host networking avoids that.

## Progress log

- [x] Plan written (2026-11-13)
- [x] Step 1 tested on sentinel (Ubuntu 24.04 + systemd container): fresh install from
  v0.1.3, re-run with `--version v0.1.2` keeping choices, `apitorrentd update` as the
  service user back to v0.1.3, `--user deluge` switch, `--uninstall`. Shellcheck clean.

## Open questions for the user

1. Pages URL: `https://cinderblock.github.io/apitorrent/` works with no DNS change. A
   custom domain later needs a DNS change (per-change approval).

## Things not to do

- Don't ship the package unit with self-update on: apt and the daemon would fight over
  `/usr/bin/apitorrentd`.
- Don't drop the `*-linux-gnu` target names: v0.1.3 installs look them up in the manifest.
- Don't host packages on Cloudflare Pages (25 MiB limit), and touch no Cloudflare
  settings without approval.

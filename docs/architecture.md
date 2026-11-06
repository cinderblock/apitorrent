# apitorrent architecture

apitorrent is a BitTorrent client meant to replace Deluge. It has a fast, cross-platform
Rust core, a native desktop app (Tauri), and a full web UI. The daemon, the desktop app and
the web UI share one API and one security model.

This document describes the target design. `plans/` tracks what is built so far.

## Goals

1. **A core you can trust for years.** It is correct, survives crashes, and stays fast with
   10,000+ torrents. State is never lost on power failure.
2. **Every UI is a client of one API.** The web UI, the desktop UI, the CLI and plugins all
   go through the same versioned API. The desktop app can embed the core, or act as a thin
   client to a remote daemon.
3. **Secure by default.** It never accepts an unverifiable certificate. Passkeys are the
   primary login. Plugins are sandboxed, and get only the capabilities you grant them.
4. **Files are first class.** A file belongs to the library, not to one torrent. Several
   torrents can share it, and one can keep seeding after a newer pack supersedes it.
5. **Automation is first class.** Plugins provide sources such as feeds, release watchers
   and scrapers. The core provides rules, retention and distribution to other instances.
6. **Windows, macOS and Linux all get full support**, without WSL.

## Process model

```
                ┌──────────────────────────── apitorrent-core ─────────────────────────────┐
                │  TorrentRegistry · Queue · Rules · ContentLibrary · Feeds · Distribution │
                │  EventBus (revisioned state store)        Persistence (SQLite, WAL)      │
                │                         │                                                │
                │                   Engine trait                                           │
                │        ┌────────────────┼──────────────────┐                             │
                │   engine-lt (libtorrent 2.0, cxx)    engine-mock (tests/UI dev)          │
                └──────────────────────────────┬───────────────────────────────────────────┘
                                               │
                                       apitorrent-server (axum)
                      HTTP API + WebSocket sync · auth · static web UI · optional TLS/ACME
                                               │
          ┌──────────────────┬─────────────────┼───────────────────┬────────────────────┐
     apitorrentd        desktop app        web UI (browser)       CLI            WASM plugins
   (headless daemon)  (embedded core, or                                        (wasmtime host,
                       thin client to a                                          in-process)
                       remote daemon)
```

- **`apitorrentd`** is the headless daemon. It runs as a systemd service, a Windows
  service, a launchd agent or a container.
- **The desktop app** runs in one of two modes, chosen per profile:
  - **Local**: the core runs inside the app process. The app can also serve the web UI and
    API on the LAN, like a daemon.
  - **Remote**: a thin client to one or more daemons. Path mappings let you open remote
    download folders in Explorer, Finder or your file manager (see "Remote file access").
- **The web UI** is the same frontend as the desktop app, built once. The server embeds it.
  Desktop-only features, such as revealing a file in Explorer, are behind a capability
  check.

## Crates and packages

| Path | Role |
|---|---|
| `crates/apitorrent-types` | Shared domain types (ids, info-hashes, torrent state, API DTOs). Exported to TypeScript. |
| `crates/apitorrent-engine` | The `Engine` trait, the engine event types and an engine-agnostic settings model. |
| `crates/apitorrent-engine-mock` | A deterministic in-memory engine that simulates peers and progress. Used by tests and UI development. |
| `crates/apitorrent-engine-lt` | The libtorrent-rasterbar 2.0 backend: a cxx bridge over a small C++ shim. |
| `crates/apitorrent-core` | Orchestration: registry, persistence, queue, labels, rules, content library, feeds, the event store. |
| `crates/apitorrent-auth` | Users, passkeys (WebAuthn), optional passwords (Argon2id), sessions, scoped API tokens, device pairing. |
| `crates/apitorrent-server` | axum HTTP/WS API, static web UI, TLS (rustls), ACME. |
| `crates/apitorrent-client` | A Rust client for the API with strict TLS and certificate pinning. Used by the desktop thin client and the CLI. |
| `crates/apitorrent-plugin-host` | The wasmtime component-model host, WIT worlds, capability enforcement. |
| `crates/apitorrentd` | Daemon binary. |
| `crates/apitorrent-cli` | `apitorrent` CLI. |
| `apps/desktop` | Tauri 2 app. |
| `web/` | The web UI (TypeScript, React, Vite, Bun), shared with the desktop app. |
| `wit/` | Plugin interface definitions. |
| `plugins/` | First-party plugins (RSS/Atom, Ubuntu releases, Raspberry Pi images, …). |

## Engine

### Choice: libtorrent-rasterbar 2.0, behind our own trait

| | libtorrent RC_2_0 | rqbit 9.x (Rust) |
|---|---|---|
| Maturity | 20 years. Powers qBittorrent and Deluge. | Young. v9.1 (Oct 2026) still fixed remote crash bugs. |
| BitTorrent v2 (BEP 52) | Full, including hybrid torrents | Groundwork only. Hybrid torrents can be added. |
| Piece picking | Rarest-first, sequential, priorities, time-critical | Sequential only |
| MSE/PE encryption | Yes. Many private trackers require it. | No |
| Webseeds, i2p, super-seeding, share mode | Yes | No |
| uTP, DHT, PEX, LSD, UPnP/NAT-PMP | Yes | Yes (uTP disabled by default) |
| Rust integration | No maintained bindings, so we write a cxx shim | Native |
| Build | C++ with CMake and Boost headers. Heavier on Windows. | `cargo build` |

The protocol engine is the part users notice when it goes wrong, and libtorrent is the only
option that is both feature-complete and proven. Deluge's core is already libtorrent, so
we keep what was good about it and replace everything Deluge layers on top.

The **`Engine` trait** keeps the choice reversible. The core never touches libtorrent
types. A pure-Rust engine (rqbit, or our own) can be added later as another backend, and the
mock engine lets everything above the engine be built and tested without C++.

### Engine boundary rules

- The engine owns the peer wire, the trackers, DHT, piece picking and disk I/O. It knows
  nothing about labels, users, feeds or libraries.
- Engine → core communication is an **event stream** (libtorrent alerts mapped to
  `EngineEvent`) plus **periodic stat batches** (`post_torrent_updates`, so only torrents
  that changed are reported).
- Core → engine communication is **commands** (`add`, `pause`, `set_file_priorities`,
  `move_storage`, `force_recheck`, `save_resume_data`, …).
- Fast-resume data is opaque bytes that the core persists in SQLite, versioned per backend.

## Persistence

- **SQLite in WAL mode**, one database per instance: torrents, files, labels, rules, feeds,
  library entries, users, credentials, sessions, tokens, audit log.
- Fast-resume blobs live in the same database. They are written on every
  `save_resume_data` alert, and flushed with a checkpoint on shutdown.
- The `.torrent` / metadata info-dict is stored once and keyed by info-hash. A v2 hash or a
  hybrid hash pair both count as identity.
- Schema migrations are embedded and forward-only, and a backup is taken before each
  upgrade.

## API

- **Commands**: JSON over HTTP, `POST /api/v1/<area>/<verb>`. Typed DTOs live in
  `apitorrent-types`, and TypeScript types are generated from them so the UI cannot drift.
- **State sync**: `GET /api/v1/sync` as a WebSocket. The server holds a **revisioned
  store**. A client gets a snapshot at revision N, then a stream of patches (upserts and
  removes per entity kind). It subscribes to field sets, such as the summary columns for the
  list view or the full detail of the selected torrent, so 10,000 torrents cost only what
  the UI shows. After reconnecting, a client resumes from its last revision, or gets a
  fresh snapshot if that revision has been compacted away.
- **Versioning**: `/api/v1` stays stable once released, and capabilities are negotiated in
  the hello message (`server_capabilities`).
- An **OpenAPI** document is generated for the HTTP surface.

## Security model

### Transport

- **The daemon binds plain HTTP only to loopback by default.**
- **Behind a reverse proxy** (Caddy, …) is the expected deployment. Configure
  `trusted_proxies`; `X-Forwarded-*` is honoured only from those addresses.
- **Built-in TLS** (rustls) is for direct exposure. It supports:
  - Bring your own certificate and key, reloaded on change.
  - ACME HTTP-01 and TLS-ALPN-01, built in.
  - ACME DNS-01 through a `dns-provider` plugin (Cloudflare, Route53, RFC 2136, …). The
    ACME state machine stays in the core, so the TLS listener owns its certificates. Only
    "publish this TXT record" is pluggable.
- A non-loopback plaintext bind is refused unless `trusted_proxies` is set, or the operator
  sets `allow_insecure_lan = true`. The UI then shows a persistent warning.

### Client trust (desktop thin client, CLI, instance-to-instance)

These clients **never accept a certificate that fails verification**. There is no "ignore
certificate errors" switch. A server is trusted in one of three ways:

1. **Public or OS-trusted CA**, through the platform verifier: Windows CertGetCertificateChain,
   macOS Security.framework, or the Linux system store.
2. **Private CA**: the user imports a CA certificate for that profile.
3. **Pinned key, established by pairing.** The server shows a short pairing code, and the
   client verifies the server's SPKI hash against it out of band before pinning it. This
   replaces Deluge's accept-any-self-signed behaviour with something you actually checked.

### Authentication

- **Passkeys (WebAuthn) are primary.** The relying-party ID is the configured public
  hostname. `localhost` works for local setups.
- **Passwords are optional** (Argon2id), for bootstrap or for environments without passkey
  support. An admin can disable them.
- **Bootstrap**: on first start, the daemon prints a one-time setup URL with a token, and
  the first user enrolls a passkey there. Nothing is reachable before enrollment except
  that page.
- **Sessions**: an HttpOnly, Secure, SameSite=Strict cookie, rotated on privilege change,
  with idle and absolute expiry.
- **CSRF**: SameSite=Strict, plus an `Origin` / `Sec-Fetch-Site` check on every state-changing
  request and on WebSocket upgrade.
- **API tokens**: scoped (`read`, `torrents:write`, `admin`, …), hashed at rest, optionally
  tied to an IP range. Used by scripts and the *arr ecosystem.
- **Device pairing**: the desktop thin client gets a device credential. It is a bearer
  token bound to the pinned key, and can be revoked per device.
- **Roles**: `admin`, `user` (owns its torrents) and `viewer`. Multi-user from day one,
  because adding it later is painful.
- **Audit log** of security-relevant events.

### Plugins

- Plugins are **WebAssembly components only** (wasmtime). Native plugins are not supported:
  a native plugin can do anything the daemon can.
- Each plugin declares the capabilities it needs in its manifest: network host allow-list,
  filesystem preopens, API scopes, timers, KV storage. The admin approves them at install,
  and any expansion at upgrade.
- Fuel and memory limits stop a buggy plugin from stalling the core.

## Content library (cross-torrent file sharing)

This is the feature Deluge cannot do, and it is designed in from the start.

### Model

- A **storage root** is a directory the instance manages, such as `D:\torrents` or
  `/srv/torrents`. It has an id, a volume identity, and capabilities: hardlink, reflink and
  symlink support, and whether it is case-sensitive.
- A **content item** is one verified file, identified by size plus its strongest known hash:
  the BEP 52 merkle root (`pieces root`), a full-file SHA-256, or a v1 piece-hash fingerprint.
  It has one physical location.
- A **placement** is a torrent's view of a file: `(torrent, file index) → content item`,
  materialised at the torrent's save path.

### Materialising placements

In order of preference, when torrent B needs file X that the library already has:

1. **Same path**: B's save path is laid out so that it points at the existing file.
2. **Reflink / copy-on-write clone** (Btrfs, XFS, APFS, ReFS): the files share blocks and are
   fully independent on write. This is the best option where available.
3. **Hardlink** on the same volume (NTFS, ext4, APFS, …). It is safe because the content is
   complete and verified, and the engine is told it already has those pieces.
4. **Symlink** (on Windows, only where the user has the privilege or Developer Mode).
5. **Copy**, as the last resort, and only with the user's consent per root.

### Matching (the "pack" case)

When a torrent is added, before it starts:

1. **v2 or hybrid torrents**: an exact match on `(length, pieces root)`. No I/O is needed to
   trust it.
2. **v1 torrents**: look for candidates by `(length, normalised name)`, then by length alone.
   Verify every piece that lies **wholly inside** the candidate file against the new
   torrent's piece hashes. If all of those pieces pass, the file matches. Pieces at a file
   boundary that straddle a file we don't have are the only ones downloaded.
3. Matched files are materialised. The torrent is added with those pieces marked as already
   verified (libtorrent `seed_mode` for complete torrents, or a targeted recheck for
   partial ones).
4. **Protecting shared bytes**: the engine must never write into a file shared by another
   torrent. A boundary piece that overlaps a shared file is therefore downloaded through
   copy-on-write. The libtorrent backend uses a priority-0 part-file trick, to be validated
   in the M3 spike, or a custom `disk_interface` if that trick does not hold.

The old torrent keeps seeding from the same physical file. Removing a torrent removes its
placements, and the content item is deleted only when no placement and no retention rule
still references it.

## Remote file access (thin client)

- Each storage root on the daemon has an optional set of **share locators**: a UNC path
  (`\\nas\torrents`), `smb://`, `nfs://` or `sftp://` URLs, or a plain mounted path.
- Each client profile holds **path mappings**: remote prefix → local path, with
  suggestions from the share locators.
- **Verification**: the daemon writes a marker file (`.apitorrent-root`) containing the
  root's id. A client marks a mapping verified only once it can read that marker through the
  local path, so "open folder" never lands in the wrong place.
- The desktop actions are open folder, reveal file and open file, using the mapped path and
  the OS shell (`explorer /select,`, `open -R`, `xdg-open`, or DBus FileManager1).

## Automation

### Sources (plugins)

A `source` plugin turns some upstream into **release candidates**: `{ id, title, version,
published, links (torrent / magnet / webseed), metadata }`. Examples:

- A generic RSS/Atom/JSON feed, with filters.
- **Ubuntu releases**: watch `releases.ubuntu.com` / cdimage, and pick the newest LTS or
  interim per flavour and architecture.
- **Raspberry Pi images**: the `os_list` JSON.
- **Open-data publishers**: dataset feeds.

### Rules (core)

A rule says which candidates to take: match criteria, version ordering, **keep the latest
N**, a target label or root, file selection, and what to do with superseded items: keep
seeding, stop, or remove after a ratio or time.

**Stable aliases**: a rule can maintain a "latest" path, such as
`library/ubuntu/latest-desktop-amd64.iso`, that always points at the current release.

### Distribution ("make it available to my servers")

- An instance can **publish a channel**: a signed, revisioned list of torrents chosen by a
  rule or a label.
- Other instances **subscribe** over the authenticated API, with the same strict TLS and
  pinning as the thin client, and run their own rules over the channel. A server farm then
  follows one curator instance, and the swarm does the data transfer.

## Deluge migration

An importer reads a Deluge state directory: `torrents.state`, which is a Python pickle
(read with `serde-pickle`; no Python needed), `*.torrent`, `*.fastresume`, labels and
per-torrent options. It re-adds the torrents with their fast-resume data, so no recheck is
needed. That is the onboarding path for existing Deluge users.

## Performance targets

- Sustain 10,000 loaded torrents, of which 2,000 are active, on a small home server.
- The UI list stays smooth at 10,000 rows. The UI virtualises the list, and the sync
  protocol sends only the fields the UI subscribes to.
- Startup with 10,000 torrents takes under 5 s to a usable UI. The engine re-adds torrents
  lazily in the background, using fast-resume data.
- Line-rate download on 10 GbE with NVMe storage. That is engine-bound, so we tune the
  libtorrent disk settings and use mmap storage.

## Cross-platform notes

- **Paths**: torrent file names are sanitised. That covers Windows reserved names (`CON`,
  `NUL`, …), trailing dots and spaces, illegal characters, `..` and absolute components, and
  long paths: the core uses `\\?\` internally on Windows.
- **Services**: a systemd unit, a Windows service (`windows-service` crate) and a launchd
  plist.
- **Desktop**: Tauri 2, with WebView2 on Windows, WKWebView on macOS and WebKitGTK on Linux.

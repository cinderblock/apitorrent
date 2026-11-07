# apitorrent architecture

apitorrent is a BitTorrent client meant to replace Deluge. It has its own Rust BitTorrent
engine, a native desktop app (Tauri), and a full web UI. The daemon, the desktop app and the
web UI share one API and one security model.

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
           ┌──────────────────────────────── apitorrent-core ────────────────────────────────┐
           │  TorrentRegistry · Queue · Rules · ContentLibrary · Feeds · Distribution        │
           │  EventBus (revisioned state store)          Persistence (SQLite, WAL)           │
           │                              │                                                  │
           │                     apitorrent-engine (pure Rust)                               │
           │   session · peer wire (sans-IO) · picker · choker · storage · trackers · DHT    │
           └──────────────────────────────┬──────────────────────────────────────────────────┘
                                          │
                                  apitorrent-server (axum)
                 HTTP API + WebSocket sync · auth · static web UI · optional TLS/ACME
                                          │
     ┌──────────────────┬─────────────────┼────────────────┬──────────────┬────────────────────┐
apitorrentd        desktop app       web UI (browser)     CLI      TS plugins (V8      external tools
(headless       (embedded core, or                                  isolates,           (scoped API
 daemon)         thin client to a                                   in-process)          tokens)
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
| `crates/apitorrent-bencode` | A strict, depth-limited, zero-copy bencode codec. Fuzzed. |
| `crates/apitorrent-metainfo` | `.torrent` and magnet parsing for v1, v2 and hybrid torrents. Info-hashes, file trees, merkle trees, path sanitising, torrent creation. |
| `crates/apitorrent-wire` | The sans-IO peer wire protocol: handshake, messages, extension protocol, MSE/PE. Pure state machines, no sockets. |
| `crates/apitorrent-engine` | The engine: session, peer manager, piece picker, choker, storage, announce scheduling, DHT/LSD/PEX, uTP, rate limiting. |
| `crates/apitorrent-tracker` | Tracker clients: HTTP/HTTPS (strict TLS through the OS trust store) and UDP (BEP 15), announce and scrape. Pure codecs plus a shared async client. |
| `crates/apitorrent-dht` | The Mainline DHT: BEP 5, 32 (IPv6), 33 (scrape), 42 (secure IDs) and 43 (read-only). A sans-IO node plus a tokio driver; persistence as opaque bytes the core stores. |
| `crates/apitorrent-types` | Shared domain and API DTO types. Exported to TypeScript. |
| `crates/apitorrent-core` | Orchestration: registry, persistence, queue, labels, rules, content library, feeds, the event store. |
| `crates/apitorrent-auth` | Users, passkeys (WebAuthn), optional passwords (Argon2id), sessions, scoped API tokens, device pairing. |
| `crates/apitorrent-server` | axum HTTP/WS API, static web UI, TLS (rustls), ACME. |
| `crates/apitorrent-client` | A Rust client for the API with strict TLS and certificate pinning. Used by the desktop thin client and the CLI. |
| `crates/apitorrent-plugin-host` | Embedded V8 (`deno_core`): one isolate per plugin, capability-checked ops, the plugin lifecycle. |
| `crates/apitorrentd` | Daemon binary. |
| `crates/apitorrent-cli` | `apitorrent` CLI. |
| `apps/desktop` | Tauri 2 app. |
| `web/` | The web UI (TypeScript, React, Vite, Bun), shared with the desktop app. |
| `packages/plugin-sdk` | `@apitorrent/plugin-sdk`: types, a test harness and a build helper for plugin authors. |
| `plugins/` | First-party plugins (qBittorrent API, RSS/Atom, Ubuntu releases, Raspberry Pi images, ACME DNS providers, …). |

## Engine

### Choice: our own Rust engine

Decided 2026-11-06. libtorrent was the first choice. We dropped it because the features that
set apitorrent apart, packs and shared files above all, depend on owning the storage layer.
In libtorrent that means a custom `disk_interface` written in C++, across an FFI boundary.

| Option | Verdict |
|---|---|
| libtorrent through cxx | Proven, but the storage work would be in C++ behind FFI, and the Windows build needs Boost. Rejected. |
| Fork rqbit's `librqbit` | Its session and storage design is sequential-only, with no encryption and partial v2 support. We'd rewrite exactly the parts we forked it for. Rejected as a base. |
| **Our own engine, reusing leaf crates** | We own the session, peer manager, picker, choker and storage. We depend on proven, separable crates (rqbit's DHT, uTP, UPnP and LSD crates, Apache-2.0) once each one passes evaluation. **Chosen.** |

Dependencies beat forks: we keep getting upstream fixes. If a leaf crate needs changes, we
propose them upstream (each post needs the owner's approval), or vendor a fork with its
licence notices intact.

### Design principles

- **Sans-IO protocol core.** The wire protocol, the per-torrent state, the picker and the
  choker are deterministic state machines. They take events (bytes received, timer fired,
  disk write finished) and return actions (send, connect, read, write). A thin tokio layer
  does the I/O. This makes the hard parts unit-testable, and allows **deterministic network
  simulation**: thousands of virtual peers, packet loss and churn, reproducible from a
  seed, all inside `cargo test`.
- **Storage is ours.** A storage layer built on placements (see "Content library")
  replaces "a torrent owns its save path". The write path stages every piece and verifies
  its hash **before** any byte lands in a file another torrent shares. This removes the
  corruption hazard that cross-seed and qui warn about.
- **Never trust the network.** Every parser has length and depth limits and a fuzz target.
  No panics on peer input; a misbehaving peer costs us only its connection.
- **Interop is tested, not assumed.** CI runs the engine against libtorrent (qBittorrent-nox),
  Transmission and rqbit in containers: download from each, seed to each, v1, v2 and hybrid,
  TCP and uTP, with and without encryption.

### Feature scope

| Area | BEPs / features |
|---|---|
| Core protocol | BEP 3, 6 (fast), 10 (extension protocol), 20 (peer id) |
| Metadata | BEP 9 (magnet metadata), 12 (multitracker), 47 (pad files, attributes), 52 (v2, hybrid), 53 (magnet file selection) |
| Discovery | BEP 5 (DHT), 11 (PEX), 14 (LSD), 15 (UDP tracker), 23 (compact), 7 / 32 (IPv6), 48 (scrape) |
| Transport | TCP, BEP 29 (uTP), BEP 55 (holepunch), MSE/PE encryption, SOCKS5 proxy, IPv6 |
| Data | BEP 19 (webseeds), 17 (HTTP seeds). Not BEP 30, the superseded v1 merkle extension. |
| Picking | rarest-first with availability tracking, priorities, sequential and streaming deadlines, endgame |
| Choking | tit-for-tat with optimistic unchoke, a rate-based seed choker, upload slots |
| NAT | UPnP IGD, NAT-PMP / PCP |
| Limits | global, per-torrent and per-peer-class rate limits, connection limits, scheduling |
| Later | I2P, BEP 21 (partial seeds), super-seeding, share mode |

### Engine API

The core talks to the engine through a Rust API: commands (`add`, `pause`,
`set_file_priorities`, `relocate`, `recheck`, …) and an event stream (state changes, stat
batches, errors). The engine knows nothing about labels, users, feeds or libraries.
Resume state is a versioned, serialisable struct that the core stores.

## Persistence

- **SQLite in WAL mode**, two databases per instance: `apitorrent.db` for torrents, files,
  labels, rules, feeds and library entries, and `auth.db` for users, credentials, sessions,
  tokens and the security audit log (see "Authentication").
- Resume state (piece bitfields, file mtimes and sizes for quick validation, stats) is
  saved incrementally. A bitfield is flushed after each verified piece batch, at most every
  few seconds, so a power failure loses seconds of progress, not hours.
- The torrent info-dict is stored once and keyed by info-hash. A v2 hash or a hybrid hash
  pair both count as identity.
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
  `--trusted-proxy` (addresses or networks); `X-Forwarded-For` and `X-Forwarded-Proto` are
  honoured only from those peers. The client address is the right-most untrusted entry of
  `X-Forwarded-For`. The proxy must pass the `Host` header through, because the Host,
  Origin and passkey checks all use it. `--public-url https://…` names the public origin.
- **Built-in TLS** (rustls) is for direct exposure. It supports:
  - Bring your own certificate and key, reloaded on change.
  - ACME HTTP-01 and TLS-ALPN-01, built in.
  - ACME DNS-01 through a `dns-provider` plugin (Cloudflare, Route53, RFC 2136, …). The
    ACME state machine stays in the core, so the TLS listener owns its certificates. Only
    "publish this TXT record" is pluggable.
- A non-loopback bind needs authentication enabled, and plain HTTP there is refused unless
  `--trusted-proxy` is set or the operator passes `--allow-insecure-lan`. Either way the
  daemon logs a loud warning recommending a TLS reverse proxy, and the UI shows a
  persistent warning on any page reached over plain HTTP from a non-loopback host.
  (Built-in TLS isn't implemented yet.)

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

Built in M4 (`crates/apitorrent-auth`, wired into `apitorrent-server`). Credentials live in
their own SQLite file, `auth.db` in the data directory (WAL, forward-only migrations,
`secure_delete`, owner-only permissions on Unix), so they can be backed up or wiped apart
from the torrent database.

- **Every `/api/v1` route needs a credential** except `info`, `auth/status` and the setup,
  sign-in and sign-out endpoints. That includes unknown paths (401, not 404) and the sync
  WebSocket. Writes (anything but `GET`/`HEAD`) also need a credential that may write.
- **Passkeys (WebAuthn) are primary**, through `webauthn-rs` 0.6 (RustCrypto, no OpenSSL).
  - One relying party per host: `localhost` (any port, `http` or `https`) always, plus the
    host of each `--public-url`, which must be `https`. The request's `Host` picks the RP,
    and a passkey works only on the host it was made on. An IP address can never be an RP
    ID, so `http://127.0.0.1` gets password and token sign-in only, and the UI points to
    `http://localhost` instead.
  - Passkeys are **discoverable** (resident key required) and **user-verified**, so
    sign-in is usernameless and the login endpoints never reveal which accounts exist.
    Signature counters are checked.
  - Users can have several passkeys, named and revocable. The last sign-in method can't be
    removed.
  - Ceremony state stays in server memory: single-use, five-minute expiry, bound to its
    host (and, for re-auth and adding a passkey, to its session), and capped in number.
- **Passwords are optional** (argon2id, 64 MiB, 3 passes; 12 to 1024 characters; at most
  two hashes at once; hashes with old parameters are upgraded on sign-in). The instance
  chooses `--passwords off` (the default), `fallback` (a password alone signs in, for hosts
  without passkey support) or `second-factor` (a user who set one must enter it after the
  passkey; until then the session can only finish signing in or sign out, and it lapses
  after five minutes).
- **Bootstrap**: while no user exists, the daemon prints a one-time setup URL,
  `/setup#token=…`, on stderr. The token is in the fragment, so it never reaches the server
  or an access log; the setup page reads it and removes it from the address bar. It has
  160 bits, lasts an hour, and is replaced (and reprinted) when it expires or after ten
  wrong guesses. `--write-setup-token` also writes it to `<data dir>/setup-token` (mode
  0600 on Unix; on Windows it inherits the data directory's per-user ACL), deleted once
  used. The token, checked in constant time, enrolls the first admin with a passkey (or a
  password in `fallback` mode). Nothing but setup and status works before then.
- **Sessions**: a 256-bit random id in a cookie, stored only as its SHA-256. Idle expiry
  (7 days by default) and absolute expiry (30 days), both configurable. Every sign-in,
  re-authentication and second-factor step issues a new id and ends the old one. Sign-out
  deletes it; users can list their sessions and end any of them.
  - Over HTTPS (a trusted proxy reporting `X-Forwarded-Proto: https`) the cookie is
    `__Host-apitorrent-session` with `Secure; HttpOnly; SameSite=Strict; Path=/`.
  - Over plain HTTP (loopback, or a LAN with `--allow-insecure-lan`) it is
    `apitorrent-session` with the same attributes minus `Secure`: browsers reject
    `__Host-` without `Secure`, and Safari refuses `Secure` cookies on `http://localhost`
    even though it is a secure context. On loopback the prefix would add nothing anyway.
    Each transport reads only its own cookie name, so neither can stand in for the other.
- **Recent authentication**: adding or removing a passkey, setting or removing the
  password, and creating an API token need a sign-in from the last ten minutes. Otherwise
  the server answers `reauth_required` and the UI asks for the passkey (or the password, in
  `fallback` mode) again.
- **CSRF**: SameSite=Strict, plus checks on every state-changing request and WebSocket
  upgrade: `Origin`, when present, must equal the request's own origin, port included (other
  ports on the same host are other sites' pages, and browsers call them `same-site`), and
  `Sec-Fetch-Site` may not be `cross-site` or `same-site`. A cookie-authenticated WebSocket
  must carry `Origin`. Requests with `Authorization: Bearer` skip these checks: a browser
  never adds that header to a cross-site request, since we grant no CORS. A bad bearer
  token is a 401, never a fall-back to the cookie.
- **API tokens**: `apt_` plus 256 random bits, shown once, stored as SHA-256. Named, with
  scope `read` (`GET` and the sync socket) or `full`, an optional expiry, last-used time,
  and revocation. Tokens can never manage credentials: account endpoints need a browser
  session. Finer scopes (`torrents:write`, `admin`, …) and IP-range binding can extend
  this later.
- **Rate limits and lockout**: sign-in and setup calls are limited per client address
  (IPv6 by /64) to bursts of 15 and 30 a minute, and ten consecutive failures lock the
  address out for a minute, doubling up to an hour. Five wrong passwords lock that
  account's password sign-in the same way. Passkey sign-in is never locked per account,
  since it can't be guessed and locking it would only let an attacker lock the owner out.
  Unknown users cost a full argon2 verification, so timing doesn't reveal them.
- **Long-lived sockets**: the sync WebSocket re-checks its credential whenever any session
  or token is revoked, and on every 30-second ping, which catches expiry. It closes with
  code 4401 once the credential is no longer valid.
- **No-auth development mode**: `--insecure-no-auth-loopback` treats direct loopback
  requests with a loopback `Host` as a synthetic local admin. It is refused with a
  non-loopback address or with trusted proxies, and requests carrying proxy headers
  (`X-Forwarded-For`, `Forwarded`, `X-Real-IP`) don't get the free pass, since a local
  proxy would make remote clients look local.
- **Device pairing** (planned): the desktop thin client gets a device credential. It is a
  bearer token bound to the pinned key, and can be revoked per device.
- **Roles**: `admin`, `user` (owns its torrents) and `viewer` are stored, and viewers may
  only read. The first user is the admin. Creating further users (invitations) is still to
  come.
- **Audit log**: setup, sign-ins and failures, lockouts, re-authentication, and passkey,
  password, token and session changes go to `auth_events` with time, user and client
  address, kept for a year.

## Plugins

### Runtime: TypeScript on embedded V8

Decided 2026-11-06, replacing the earlier WebAssembly plan. WebAssembly adds a toolchain
most plugin authors don't know, and that cost falls on exactly the people we want writing
plugins.

| Option | Verdict |
|---|---|
| WebAssembly components | Sandboxed and multi-language, but authoring friction is high. Rejected. |
| Bun as the plugin runtime | Great tooling, but it has **no permission system**: every plugin would run with the daemon's full privileges. It can't be embedded either, so plugins would be subprocesses. Rejected as the runtime. **Kept as the authoring toolchain.** |
| Full Node or Deno compatibility | A large surface and a large attack surface. Most of it is never needed. Rejected. |
| **Embedded V8 with our own globals** (`deno_core`) | TypeScript, fast, in-process, with a small curated API in which every privileged call is an op the host checks against the plugin's grants. **Chosen.** |

- Authors write TypeScript against `@apitorrent/plugin-sdk`, and test and bundle it with
  Bun (`bun test`, `bun build` → a single ESM file). npm packages work as long as they bundle
  to plain JS without Node built-ins.
- **Each plugin gets its own V8 isolate** with a heap limit. A watchdog terminates runaway
  execution and records the reason.
- **Globals**: `fetch` (host-checked), `URL`, `TextEncoder` / `TextDecoder`, `crypto.subtle`
  and `getRandomValues`, timers, `structuredClone`, `console` (to the plugin log),
  `AbortController`, `Headers` / `Request` / `Response`, plus the `apitorrent` module.
- **The `apitorrent` module**: torrents (query, add, control), events (subscribe),
  library, rules, KV storage, scheduling, settings schema (rendered by the UI), HTTP routes
  under `/plugins/<id>/` (or a dedicated listener), and roles such as `source`,
  `dns-provider` and `api-compat`.

### Capabilities

A plugin's manifest declares what it needs: network hosts, API scopes, HTTP routes,
listeners, filesystem paths, schedule. The admin approves those at install, and any
expansion at upgrade. Undeclared calls fail. Plugins get **no** raw filesystem, process,
environment or socket access.

### External integrations

Anything that speaks the HTTP API with a scoped token is an external integration, in any
language and any runtime, Bun included. Plugins are for code that should live inside the
daemon, be managed by it and stay sandboxed. External integrations are for everything else.

### First-party plugins

They prove the plugin API, and they ship in the box:

- **`qbittorrent-api`**: a qBittorrent WebAPI v2 compatibility layer, so Sonarr, Radarr,
  autobrr, cross-seed and friends work unchanged. Login maps to scoped API tokens.
  Categories and tags map to labels. It mounts on its own listener, or under a URL base
  (the *arr apps support one). Off by default.
- **Sources**: RSS/Atom/JSON feeds, Ubuntu releases, Raspberry Pi images, open-data
  publishers.
- **DNS providers** for ACME DNS-01.

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

Because the engine's storage works through placements, sharing is native. Two torrents
that share a file both read from the same content item. Links are only needed when a user
wants to see the file at a path of their own.

### Materialising placements on disk

In order of preference, when torrent B's visible layout needs file X that the library
already has:

1. **Hardlink** on the same volume (NTFS, ext4, APFS, …). Built: no extra space, and
   each torrent keeps a normal file at its own path.
2. **Reflink / copy-on-write clone** (Btrfs, XFS, APFS, ReFS). Planned.
3. **Copy**, when the volumes differ and the library setting allows it (on by default:
   reading a local disk beats downloading).

Symlinks aren't used: on Windows they need a privilege, and a deleted target breaks them.

**Linked files are copy-on-write in the engine.** Before writing into a linked file, the
engine compares the bytes. Equal bytes are skipped. If anything would change, it first
replaces its link with a private copy, so the other torrent's file is never modified
through a link. That matters for v1 matches, where only the interior pieces were checked
and the bookend bytes (an ID3 tag, say) can differ. The flag is stored per file and
survives restarts.

The design began with a shared "content item" that both torrents' storage would read
through, so links wouldn't be needed for seeding. Hard links give the same sharing with
far less machinery, and each torrent's directory is a real, complete set of files.

### Matching (the "pack" case)

When a torrent is added, before it starts:

1. **v2 or hybrid torrents**: an exact match on `(length, pieces root)`. No I/O is needed to
   trust it.
2. **v1 torrents**: look for candidates by `(length, normalised name)`, then by length alone.
   Verify every piece that lies **wholly inside** the candidate file against the new
   torrent's piece hashes. If all of those pieces pass, the file matches. Pieces at a file
   boundary that straddle a file we don't have are the only ones downloaded.
3. A boundary piece that overlaps a shared file is assembled in a **staging area**. Once it
   verifies, only the bytes that belong to files this torrent owns are written out. The
   shared file is never written to.

The old torrent keeps seeding from the same content item. Removing a torrent removes its
placements, and a content item is deleted only when no placement and no retention rule
still references it.

### Going further than file-level matching

- **Bookends.** A v1 piece that straddles file X (which we have) and a neighbour Y is
  verified the moment both are available from *any* source. If Y has to be downloaded,
  X's part of the piece comes from the library copy, Y's part from peers, and the whole
  piece is hashed in the staging area. If it fails, the piece is downloaded again entirely
  from peers. Only if that clean copy disagrees with the library bytes in X's range is the
  candidate marked as a different version.
- **Piece-level reuse.** Every piece that verifies is genuinely the torrent's data, even
  when the file as a whole doesn't match (a repack that changed one region, say). Those
  pieces are copied into the new torrent's own file, and only the failing ones are
  downloaded. The two files stay separate content items, because their bytes differ.
- **v2 needs no bookends.** v2 files start on piece boundaries, so `pieces root` matches
  exactly. Piece layers then pinpoint which pieces differ when the roots don't match.
- **Fingerprints for future torrents.** In idle time the library computes each file's v2
  `pieces root` and full SHA-256, even for files that came from v1 torrents. A later v2 or
  hybrid torrent of the same file then matches with no I/O at all. v1 candidates are
  still found by length (and name) and then checked by hashing at the new torrent's
  alignment, which costs one read of the candidate file.

### Unwanted files and the part store

A torrent's save directory only ever holds files the user wants. Deselecting a file
removes it from that directory.

- Bytes of unwanted files that come in as part of boundary pieces go to a **part store**,
  never to the save directory. It holds one sparse slot per piece, managed by apitorrent in
  its data directory. It keeps those boundary pieces verifiable and seedable.
- **Deselecting a file that already has data** moves it out of the save directory:
  - The boundary bytes that neighbouring wanted files still need go to the part store.
  - The rest goes to a managed **held** area (a rename, so it lives on the same volume as
    the save path) or is deleted, depending on the per-torrent retention setting, which
    defaults to *hold*.
  - Re-selecting a held file moves it back with no download. Held data is listed in the
    UI, counts against a quota, and is purged when its torrent is removed (or by hand).
- Files the user wants are written in place while downloading. An optional
  "incomplete" suffix or directory keeps partial wanted files out of view too.

### Pack switchover (superseded torrents)

Some trackers publish single-episode torrents, then a season pack, then delete the
singles. apitorrent automates the switch:

1. Adding the pack matches its files against the library, so nothing is downloaded twice.
   Each single whose files are all covered by the pack is recorded as **superseded by**
   it.
2. The single keeps seeding until it is **retired**. Retirement happens when the tracker
   reports it unregistered, or a rule or the user retires it. Its content stays, because
   the pack references it.
3. Spotting "unregistered" is tracker-specific. Core ships a default set of
   announce-failure patterns. A **tracker adapter** plugin can replace them, query the
   tracker's API for packs that contain a torrent's files, and fetch the pack
   automatically.

## Resource limits

Limits form a hierarchy: session, then **group**, then torrent, then peer. A group is a
named set of torrents, selected by label, tracker or rule. The tightest level that applies
wins.

- **Bandwidth:** download and upload rates through hierarchical token buckets, with
  protocol overhead counted separately and optionally exempt.
- **Memory:** a byte budget for in-flight piece buffers and the write queue, at session
  level and optionally per group or torrent. When it is exhausted the picker stops
  starting new pieces and finishes the ones in progress first.
- **Connections:** open connections per session, group and torrent, plus half-open
  (connecting) attempts at once and new connection attempts per second.
- **Queueing:** the number of active downloads and seeds, like Deluge's queue.

## Misbehaving peers

One failed piece is not proof: several peers usually contributed to it, and honest peers
occasionally send bad data. Peers are scored, and banned only on evidence.

1. If every block of a failed piece came from one peer, that peer is at fault.
2. Otherwise the digest of each block is recorded along with who sent it. The piece is
   downloaded again, preferring a single peer, and suspects are put **on parole**: they
   are assigned only whole pieces, so a later failure is attributable. When the piece
   passes, block digests that differ from the good copy prove which peer sent bad data
   (libtorrent calls this "smart ban").
3. For v2 torrents, merkle hash requests give block-level proof immediately.
4. **Proven bad data means a ban.** The peer's IP is banned for the session's ban period,
   which escalates on repeat offences and is global, since poisoning tends to span
   swarms. An unproven involvement costs trust points, and repeated involvement leads to a
   ban.
5. Protocol violations, such as oversized or unrequested blocks or a bad handshake,
   disconnect the peer and count against its trust. Slow peers are only snubbed, never
   banned.
6. IP blocklists in the common P2P formats are supported, and can be loaded by plugins.

## File naming policy

Each storage root has a naming policy:

- **portable** (the default): Windows rules everywhere, so a directory can be shared over
  SMB, moved to exFAT, or opened from any OS with the same names.
- **native**: only the rules of the host filesystem, plus the rules that are always
  enforced: no `..`, no separators and no control characters.

The policy is forced to portable when the root is on a filesystem that needs it (NTFS,
exFAT or FAT mounted on Linux, or SMB). The torrent path to disk path mapping is always
stored, so renames are reversible. Seeding and library matching use torrent paths and
hashes, never disk names.

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
published, links (torrent / magnet / webseed), metadata }`.

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
(read with `serde-pickle`; no Python needed), `*.torrent`, `*.fastresume` (libtorrent
bencoded resume data, of which we read the bitfield and file info), labels and per-torrent
options. Imported torrents get a fast validation (sizes and mtimes) instead of a full
recheck. That is the onboarding path for existing Deluge users.

## Performance targets

- Sustain 10,000 loaded torrents, of which 2,000 are active, on a small home server.
- The UI list stays smooth at 10,000 rows. The UI virtualises the list, and the sync
  protocol sends only the fields the UI subscribes to.
- Startup with 10,000 torrents takes under 5 s to a usable UI. Torrents resume lazily.
- Line-rate download on 10 GbE with NVMe storage. Piece hashing runs on a dedicated pool,
  using SHA-NI or ARMv8 crypto extensions where available. Writes are vectored, and the
  disk I/O pool is separate from the network.

## Cross-platform notes

- **Paths**: torrent file names are sanitised. That covers Windows reserved names (`CON`,
  `NUL`, …), trailing dots and spaces, illegal characters, `..` and absolute components, and
  long paths: the core uses `\\?\` internally on Windows.
- **Services**: a systemd unit, a Windows service (`windows-service` crate) and a launchd
  plist.
- **Desktop**: Tauri 2, with WebView2 on Windows, WKWebView on macOS and WebKitGTK on Linux.

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
- **The web UI** is the same frontend as the desktop app, built once. Release builds of the server embed it (`apitorrent-webui`).
  Desktop-only features, such as revealing a file in Explorer, are behind a capability
  check (`isDesktop()` in `web/src/desktop/`), and live in that directory.

### The desktop app (built in M7)

- **The webview never talks to the network.** It loads only the bundled UI (navigation
  elsewhere opens the default browser). Every API call goes through a Tauri command and
  the sync WebSocket through a Tauri channel, so Rust does the TLS (with the profile's
  trust setting) and adds the API token, which JavaScript never sees. The web app has one
  seam for this, `web/src/api/transport.ts`.
- **Remote mode signs in as a paired device (or with a scoped API token), not a
  passkey.** A passkey's relying party must be the page's own host, and the app's page is
  `tauri.localhost`. Loading the daemon's origin instead would put TLS in the webview's
  hands (so pinned keys and private CAs couldn't work), would need IPC for a remote
  origin, and still fails on macOS (WKWebView allows WebAuthn only for an app's signed
  Associated Domains, which can't name every user's server) and Linux (WebKitGTK has no
  WebAuthn). So the app pairs (see "Device pairing"): the approval happens in the user's
  own browser, on the daemon's origin, where passkeys work. The credential (or a pasted
  API token) is kept in the OS keychain (`keyring`); the page never sees it, nor the poll
  secret (`pairing.rs`).
- **Native dialogs come from Rust.** The folder picker (`tauri-plugin-dialog`) is reached
  only through the app's own `pick_folder` command; the page has no plugin permissions.
  It is offered for local paths only: Local mode's folders, path mappings, and the Add
  dialog's folder in Local mode (where the server's paths are this computer's).
- **Local mode** runs the core in the app process and talks to it through
  `apitorrent_server::Embedded`: requests go straight through the router, the sync socket
  over an in-memory HTTP connection, and nothing listens on a port. Such requests carry a
  marker type no network request can, and are the machine's owner. Serving the UI on the
  LAN starts a normal listener next to it, with the daemon's rules.
- Profiles (`profiles.json`, atomic writes) are Remote (URL, trust, path mappings) or Local
  (folders, peer port, optional LAN listener). The window closes to the tray; a second
  launch hands its arguments (magnet links, `.torrent` files) to the first; finished
  downloads raise a notification.
- `apps/desktop/src-tauri` is a workspace member but not a default member, so plain
  `cargo build`/`cargo test` skip Tauri.

## Crates and packages

| Path | Role |
|---|---|
| `crates/apitorrent-bencode` | A strict, depth-limited, zero-copy bencode codec. Fuzzed (see `fuzz/`). |
| `crates/apitorrent-metainfo` | `.torrent` and magnet parsing for v1, v2 and hybrid torrents. Info-hashes, file trees, merkle trees, path sanitising, torrent creation. |
| `crates/apitorrent-wire` | The sans-IO peer wire protocol: handshake, messages, extension protocol. Pure state machines, no sockets. |
| `crates/apitorrent-mse` | Message Stream Encryption (MSE/PE): sans-IO initiator and receiver handshakes, and `MseStream` over any async byte stream. |
| `crates/apitorrent-utp` | uTP (BEP 29) with LEDBAT congestion control: a sans-IO connection, a socket multiplexer and tokio streams. |
| `crates/apitorrent-engine` | The engine: session, peer manager, piece picker, choker, storage, announce scheduling, LSD/PEX, rate limiting. Runs connections over TCP or uTP, encrypted or not, and drives the DHT. |
| `crates/apitorrent-tracker` | Tracker clients: HTTP/HTTPS (strict TLS through the OS trust store) and UDP (BEP 15), announce and scrape. Pure codecs plus a shared async client. |
| `crates/apitorrent-dht` | The Mainline DHT: BEP 5, 32 (IPv6), 33 (scrape), 42 (secure IDs) and 43 (read-only). A sans-IO node plus a tokio driver; persistence as opaque bytes the core stores. |
| `crates/apitorrent-types` | Shared domain and API DTO types. Exported to TypeScript. |
| `crates/apitorrent-core` | Orchestration: registry, persistence, queue, labels, seed limits, content library, the event store, and automation (candidates, rules, aliases, channels and subscriptions). |
| `crates/apitorrent-auth` | Users, passkeys (WebAuthn), optional passwords (Argon2id), sessions, scoped API tokens, device pairing. |
| `crates/apitorrent-webui` | The built web UI, compiled into release binaries (`embed` feature) with brotli and gzip copies and ETags. |
| `crates/apitorrent-server` | axum HTTP/WS API, static web UI, TLS (rustls), ACME. |
| `crates/apitorrent-client` | A Rust client for the API with strict TLS and certificate pinning. Used by the desktop thin client and the CLI. |
| `crates/apitorrent-plugin-api` | The plugin contract without V8: manifests, capabilities and their expansion, network grants, the settings-schema subset, and the `AppApi` (core implements), `PluginService` (host implements, server calls) and `SourceFetcher` (host implements, core's rules call) traits. |
| `crates/apitorrent-plugins` | The plugin host on embedded V8 (`deno_core`): one isolate per plugin, capability-checked ops, curated globals, the plugin lifecycle. Only it and `apitorrentd` link V8. |
| `crates/apitorrent-update` | The daemon's signed self-updates: the compiled-in update key, the latest published GitHub release, the signed manifest, download, verify, swap and restart. See "Updates". |
| `crates/apitorrentd` | Daemon binary. |
| `crates/apitorrent-cli` | `apitorrent` CLI. |
| `apps/desktop` | Tauri 2 app. |
| `web/` | The web UI (TypeScript, React, Vite, Bun), shared with the desktop app. |
| `packages/plugin-sdk` | `@apitorrent/plugin-sdk`: types, a `bun test` harness and a build script for plugin authors. |
| `plugins/` | First-party plugins: `rss`, `ubuntu-releases`, `qbittorrent-api` and `cloudflare-dns`; planned: Raspberry Pi images. Release daemons embed them (the `first-party-plugins` feature). |

## Engine

### Choice: our own Rust engine

Decided 2026-11-06. libtorrent was the first choice. We dropped it because the features that
set apitorrent apart, packs and shared files above all, depend on owning the storage layer.
In libtorrent that means a custom `disk_interface` written in C++, across an FFI boundary.

| Option | Verdict |
|---|---|
| libtorrent through cxx | Proven, but the storage work would be in C++ behind FFI, and the Windows build needs Boost. Rejected. |
| Fork rqbit's `librqbit` | Its session and storage design is sequential-only, with no encryption and partial v2 support. We'd rewrite exactly the parts we forked it for. Rejected as a base. |
| **Our own engine, reusing leaf crates** | We own the session, peer manager, picker, choker and storage. We meant to depend on proven, separable crates (rqbit's DHT, uTP, UPnP and LSD crates, Apache-2.0) once each one passed evaluation; the DHT and uTP crates didn't (see the bootstrap plan's findings), so those are our own. **Chosen.** |

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

- **SQLite in WAL mode**, three databases per instance: `apitorrent.db` for spaces,
  torrents (keyed by space and id), files, labels and library entries, `automation.db` for
  release candidates, rules, aliases, channels and subscriptions (see "Automation"), and
  `auth.db` for users, credentials, space memberships, sessions, tokens and the security
  audit log (see "Authentication"). Each has its own
  migrations, so work on one never collides with another's schema versions.
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
- **Built-in TLS** (rustls, `crates/apitorrent-tls`) is for direct exposure. It runs on the
  **ring** provider, always passed explicitly: ring builds on Windows MSVC with only a C
  compiler, where aws-lc-rs needs CMake and NASM, and nothing depends on rustls's
  process-wide default. ALPN offers `h2` and `http/1.1`; the sync WebSocket works over
  both (HTTP/1.1 `Upgrade`, or HTTP/2 extended CONNECT per RFC 8441). Handshakes run off
  the accept loop with a 10-second timeout and at most 512 in flight. The certificate is
  resolved per handshake from a store that is swapped atomically, so a new certificate
  applies to the next connection with no restart. Certificates come from one of:
  - **Bring your own** (`--tls-cert`, `--tls-key`): a PEM chain (leaf first) and key.
    The files are polled every 10 seconds (polling sees in-place rewrites, renames and
    repointed symlinks on every platform and filesystem). A replacement must load, have a
    matching key and be within its validity period; otherwise it is refused with a loud
    error and the previous certificate keeps serving.
  - **Self-signed** (`--tls-self-signed`): for LAN use with pinning. Its P-256 key is
    kept in `<data dir>/tls/self-signed/` so the pairing code survives restarts; the
    certificate (one year) is reissued with the same key 30 days before expiry or when
    the names change.
  - **ACME** (RFC 8555, through `instant-acme`): `--acme-domain` (repeatable),
    `--acme-email` (optional), Let's Encrypt production by default or `--acme-staging`,
    or any CA by `--acme-directory` (with `--acme-directory-ca` for a private CA's root).
    Challenges: **TLS-ALPN-01** (default; the HTTPS listener answers validators that
    offer only `acme-tls/1`, and closes those connections after the handshake) or
    **HTTP-01** (`--acme-challenge http-01`, answered by the plain-HTTP listener on
    `--http-redirect-port`). **DNS-01** (`--acme-challenge dns-01 --acme-dns-plugin <id>`)
    goes through a `dns-provider` plugin, which `apitorrent_plugins::dns::PluginDnsProvider`
    turns into an `apitorrent_tls::acme::DnsProvider` (publish a TXT record, remove it
    afterwards); it is required for wildcards and needs no inbound port. After a record is
    published, the ACME manager waits until every authoritative name server of the zone
    serves it (found by iterative resolution from the root servers, following referrals
    and a CNAME on the `_acme-challenge` name, asked directly so no resolver cache holds
    a stale answer), up to `--acme-dns-timeout` (300 s), before telling the CA; or asks
    the resolvers named by `--acme-dns-resolver` instead (restricted or split-horizon
    networks). If no DNS server answers at all it waits a minute and lets the CA try. A
    small DNS client of our own (`acme::wire`: UDP, TCP when truncated) does the asking.
    The ACME state machine stays in the core, so the TLS listener owns its certificates;
    only "publish this TXT record" is pluggable. Wildcard names don't become public hosts
    or passkey origins; name the hosts under them with `--public-url`. The account key and the
    certificates live in `<data dir>/acme/<CA directory>/` (owner-only on Unix), with
    each certificate and its key in one file so it's replaced atomically. Renewal starts
    at 2/3 of the lifetime (less up to 1/30 of it as jitter); failures back off from 5
    minutes, doubling, to 6 hours (1 hour within a week of expiry or with no
    certificate). Each order uses a fresh key.
- With built-in TLS:
  - A non-loopback bind needs neither `--trusted-proxy` nor `--allow-insecure-lan`
    (authentication is still required).
  - Responses carry `Strict-Transport-Security: max-age=31536000`, except for loopback
    names and IP literals: HSTS covers every port of a host, so on `localhost` it would
    force the user's other local servers to HTTPS.
  - Session cookies are `__Host-…; Secure`.
  - Passkey relying parties follow the TLS origin: each ACME domain, and each
    non-wildcard DNS name in a bring-your-own certificate (read at startup), becomes an
    `https://name[:port]` public URL unless `--public-url` already names that host. Pass
    `--public-url` yourself when a port forward makes the public port differ from the
    listen port. A self-signed certificate adds none, since browsers refuse WebAuthn on a
    page with a certificate error.
  - `--http-redirect-port` adds a plain-HTTP listener on the API's address that answers
    ACME HTTP-01 and redirects (308) everything else to HTTPS, only for host names the
    server answers to (never an open redirect). Redirects go to the port of the first
    `https` `--public-url`, or the listen port.
  - `GET /api/v1/tls` (signed in) reports the certificate's source, validity, names, SPKI
    SHA-256 and pairing code, and ACME progress.
- A non-loopback bind needs authentication enabled, and plain HTTP there is refused unless
  `--trusted-proxy` is set or the operator passes `--allow-insecure-lan`. Either way the
  daemon logs a loud warning recommending TLS, and the UI shows a persistent warning on any
  page reached over plain HTTP from a non-loopback host.

### Client trust (desktop thin client, CLI, instance-to-instance)

These clients **never accept a certificate that fails verification**. There is no "ignore
certificate errors" switch. A server is trusted in one of three ways:

1. **Public or OS-trusted CA**, through the platform verifier: Windows CertGetCertificateChain,
   macOS Security.framework, or the Linux system store.
2. **Private CA**: the user imports a CA certificate for that profile.
3. **Pinned key, established by pairing.** The server shows a short pairing code, and the
   client verifies the server's SPKI hash against it out of band before pinning it. This
   replaces Deluge's accept-any-self-signed behaviour with something you actually checked.
   - The pin is the SHA-256 of the leaf certificate's DER `SubjectPublicKeyInfo`. The
     pairing code is the first 100 bits of it as RFC 4648 base32, in five groups of four
     (`ABCD-EFGH-IJKL-MNOP-QRST`): matching it takes about 2^100 key generations. Compare
     case-insensitively, ignoring `-` and spaces; the alphabet has no `0`, `1` or `8`, so
     those may be read as `O`, `I` and `B`.
   - The daemon prints the code at startup with `--tls-self-signed` (and logs it for any
     certificate), and `GET /api/v1/tls` returns it, for a signed-in page reached over a
     trusted path such as `https://localhost`.
   - Pin self-signed keys only. ACME keys change on every renewal, and bring-your-own
     keys whenever the operator rotates them; trust those through the platform verifier
     or a private CA instead.

Built in `crates/apitorrent-client` (rustls with the ring provider). The pin is the
SHA-256 of the server certificate's DER `SubjectPublicKeyInfo`. The user types the short
pairing code the daemon prints (its first 100 bits in base32, `ABCD-EFGH-IJKL-MNOP-QRST`;
case, spacing and `0`/`1`/`8` for `O`/`I`/`B` don't matter), or the full hash in hex or
base64. The client learns the key the server presents, pairs only if the code names it,
then pins the full hash. A pin is strict: only that key is accepted, not a certificate a
public CA issued for the same name (a server that moves to a CA-signed certificate is
re-added with OS trust). Handshake signatures are always checked against the
certificate's key. For a pinned key the host name and the
certificate's dates aren't checked: the key is the identity. A private CA is checked by
webpki with only that CA as a root. Plain `http://` is allowed only to loopback (an SSH
tunnel), and the token is only ever sent to `/api/` paths.

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
  - Over HTTPS (built-in TLS, or a trusted proxy reporting `X-Forwarded-Proto: https`)
    the cookie is `__Host-apitorrent-session` with `Secure; HttpOnly; SameSite=Strict; Path=/`.
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
  and revocation. Each is bound to the space it was made in (see "Spaces"), and works
  there only. Tokens can never manage credentials: account endpoints need a browser
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
- **Device pairing** (`apitorrent-auth/src/devices.rs`), for the desktop thin client,
  shaped like OAuth's device authorization grant (RFC 8628):
  - The app asks without signing in (`POST /auth/devices/requests`, rate-limited per
    address like sign-in, at most 256 pending, each for ten minutes, in memory). It sends
    its name, its platform and, with pinned trust, the SPKI SHA-256 it pinned. It gets a
    poll secret (256 bits, never shown) and a user code (eight base32 characters,
    `ABCD-EFGH`; case, `-` and `0`/`1`/`8` look-alikes don't matter).
  - The app shows the code and opens `<server>/pair#code=…` in the default browser. A
    signed-in browser session looks the code up (wrong codes count against the address
    like wrong passwords), sees the device's name, platform and address with a warning to
    approve only a code its own app shows, picks `full` or `read`, and approves (recent
    authentication, as for creating a token) or refuses. Typing the code on the Account
    page works the same way.
  - The app polls (`POST /auth/devices/poll`) and receives `apd_` + 256 bits once; only
    its SHA-256 is stored (`devices` in `auth.db`). It works like an API token of the
    approver: its scope, the approver's space, no credential management. A device may
    list and remove only itself (the app signs out on the server when its connection is
    removed or re-paired); the Account page lists every device with its last use and
    address, and removes any. Removal closes its sync sockets.
  - **Bound to the pinned key**: when the daemon serves TLS itself, a request whose pin
    isn't the key it serves is refused (something answered in between), and a matching
    pin binds the device: its credential works only while the daemon serves that key. A
    re-keyed server needs its devices paired again, as their pins do anyway. With OS or
    private-CA trust, or behind a TLS proxy (where the daemon can't see the key the app
    saw), a device is unbound but otherwise the same.
  - Pasted API tokens keep working in the app.
- **Roles**: the instance's role (`admin`, `user` or `viewer`) is stored per user, and the
  first user is the admin. An `admin` is a **server admin**: every space, its members,
  creating spaces, instance settings, bans and plugins. Everything else is decided by a
  role held **per space** (`admin`, `user`, `viewer`; see "Spaces"); viewers may only
  read. An instance role of `viewer` caps a user at reading everywhere.
- **Invitations**: everyone after the first admin joins with a one-time link,
  `<origin>/enroll#token=<secret>`. The origin is the first `--public-url` when there is
  one (passkeys work there), else the one the inviter is using. The secret (256 bits) sits
  in the fragment, as the setup token does, so it never reaches a server or access log;
  the enrollment page reads it, removes it from the address bar, and sends it in request
  bodies only. `auth.db` keeps only its SHA-256 (`enrollments`, with the spaces and roles
  it grants in `enrollment_spaces`).
  - An invitation carries an instance role and a role in each of its spaces. It expires
    after 7 days by default (1 to 30), can be revoked, and works once.
  - **A server admin** invites into any spaces with any instance role. **A space's admin**
    invites only into spaces they administer, and only with the `user` instance role.
    Creating one needs a browser session with recent authentication, as creating an API
    token does; tokens can't invite.
  - The person opening it picks a username and registers a passkey (discoverable,
    user-verified, as at setup), or a password when the daemon runs with
    `--passwords fallback` (a password that can't sign in alone is no way to enroll). The
    page reuses the setup page's hints: an insecure-transport warning, and where passkeys
    work when they don't on this host (with the link carried over).
  - Opening a link is rate-limited and locked out per address like setup and sign-in; a
    wrong, used, revoked or expired secret counts as a failure and is audited
    (`enroll_failed`). The link is checked again inside the transaction that creates the
    user, so two tabs can't both use it.
  - **Joining with an existing account** (`POST /auth/enroll/accept`, a signed-in browser
    session, not an API token): the invitation's spaces go to the caller's own account,
    each at the higher of the role it gives and the one they have; no user is made. The
    same rate limit, one-time use, expiry and audit (`joined`) apply. The instance role
    never changes, so an invitation that makes a server admin can't be joined (409), and
    a viewer cap a server admin set stays. An invitation that would give nothing new is
    refused (409) and stays usable. This is how a space's admins add people who already
    have accounts. `enroll/info` says whether a link can be joined (`joinable`).
- **Managing users** (server admins, recent authentication): list users with their spaces,
  credentials and last activity; change instance roles; disable, enable or delete a user.
  Nobody can do this to themselves, and the last enabled server admin can't be demoted,
  disabled or deleted. **Disabling** deletes the user's sessions at once and suspends their
  API tokens (they work again once the user is enabled); sign-in is refused meanwhile.
  **Deleting** removes the user with their passkeys, sessions, tokens and memberships.
  Either wakes open sync sockets, which re-check their credential and close.
- **Recovery links** are built like invitations, for one existing user (a new one replaces
  any still waiting). Whoever opens it registers a new passkey (or password); the user's
  old passkeys and password are removed and all their sessions end. API tokens stay. A
  disabled user's link works only once they are enabled again.
- **Audit log**: setup, sign-ins and failures, lockouts, re-authentication, passkey,
  password, token and session changes, invitations and recovery links (issued, revoked,
  used, failed), and user changes go to `auth_events` with time, user and client address,
  kept for a year.

## Plugins

Built in M8 (`plans/m8-plugins.md`).

### Runtime: TypeScript on embedded V8

Decided 2026-11-06, replacing the earlier WebAssembly plan. WebAssembly adds a toolchain
most plugin authors don't know, and that cost falls on exactly the people we want writing
plugins.

| Option | Verdict |
|---|---|
| WebAssembly components | Sandboxed and multi-language, but authoring friction is high. Rejected. |
| Bun as the plugin runtime | Great tooling, but it has **no permission system**: every plugin would run with the daemon's full privileges. It can't be embedded either, so plugins would be subprocesses. Rejected as the runtime. **Kept as the authoring toolchain.** |
| Full Node or Deno compatibility | A large surface and a large attack surface. Most of it is never needed. Rejected. |
| Deno's extension crates (`deno_web`, `deno_fetch`, `deno_crypto`) | Checked 2026-11-08: `deno_crypto` pulls in aws-lc-rs (CMake and NASM on Windows), `deno_fetch` brings its own TLS stack with an "ignore certificate errors" switch, `deno_web` brings Blob URLs, BroadcastChannel, Locks and CSS. Rejected. |
| **Embedded V8 with our own globals** (`deno_core` 0.413, V8 150) | TypeScript, fast, in-process, with a small curated API in which every privileged call is an op the host checks against the plugin's grants. **Chosen.** |

- Authors write TypeScript against `@apitorrent/plugin-sdk`, test with `bun test` (the SDK's
  harness plays the host and enforces the same capabilities), and bundle with
  `apitorrent-plugin-build` into `<id>-<version>.plugin.json`: the manifest plus one ES
  module. npm packages work as long as they bundle to plain JS without Node built-ins.
- **Each plugin runs in its own V8 isolate on its own thread**, with a JavaScript heap limit
  (64 MiB) and a separate budget for `ArrayBuffer` memory (64 MiB), which the heap limit
  doesn't cover: a counting allocator refuses allocations past it and the plugin gets a
  `RangeError`. **A watchdog** terminates code that runs more than 2 s without yielding, and
  the heap-limit callback ends a plugin that runs out of memory; both record why in the
  plugin's log and error.
- **Globals**: `fetch` (host-checked, see below), `URL`/`URLSearchParams` (the `url` crate),
  `TextEncoder`/`TextDecoder` (every WHATWG encoding, via `encoding_rs`), timers,
  `structuredClone`, `AbortController`/`AbortSignal`, `Event`/`EventTarget`,
  `DOMException`, `Headers`/`Request`/`Response` (whole bodies, no streams), `crypto`
  (`getRandomValues`, `randomUUID`, `subtle.digest` and HMAC), `atob`/`btoa`, and
  `console` (to a per-plugin log ring of 1000 lines). `Deno`, deno_core's internals,
  `WebAssembly` (its memories escape the budget), `SharedArrayBuffer` and `Atomics` are
  removed. An uncaught error in a timer, a handler or a promise is logged, not fatal.
- **The `apitorrent` API** is the object passed to the plugin's `start(api)`: `torrents`
  (list, get, add, pause, resume, set label, seed limits, rate limits, queue moves,
  remove), `app.info` (version, download directory, instance seed limits and queue),
  `events.on` (torrent added, removed, state changed, finished), `kv` (JSON values, 16 MiB
  per plugin), `schedule.every` (jobs that don't overlap and resume their cadence after a
  restart), `settings` (with defaults and change notifications), `http.route`, `auth`
  (sign-in with API tokens, below), and `roles.provide`. The host reaches the app
  through the `AppApi` trait, which the core implements; those calls run on the daemon's
  runtime, never on a plugin's thread.

### Capabilities

A plugin's manifest declares everything it may do, and an admin approves it before the
plugin runs. Undeclared calls throw `NotCapable`.

- **`network`**: host patterns for `fetch`: `example.com` (HTTPS on 443), `*.example.com`
  (subdomains), `example.com:8443` or `:*`, and `http://…` to allow plain HTTP. Names are
  compared after IDNA. The host follows redirects itself and checks every hop; it never
  goes from HTTPS to HTTP and drops `Authorization` when a redirect leaves the origin. A
  granted *name* must resolve to public addresses (DNS-rebinding defence); loopback and
  private addresses are reachable only through a grant that names the IP address (or
  `localhost`). TLS is the tracker crate's strict configuration. Bodies are capped (8 MiB
  out, 32 MiB in).
- **`networkFromSettings`**: settings fields whose values are also grants, so a feed reader
  can reach the feeds an admin types in. A URL grants its origin; anything else is read as
  a pattern. Grants update when the settings change.
- **`api`**: `torrents:read`, `torrents:add`, `torrents:control`, `torrents:remove`,
  `events:read`, `tokens:verify`. A plugin's save path is relative to the download
  directory; absolute paths and `..` are refused.
- **`tokens:verify`** (only with the `api-compat` role): `api.auth.login(token)` checks an
  apitorrent API token a client presented (as a password, say) and returns an opaque
  session id and the caller; `api.auth.session(id)` returns the caller while the session
  lives. The host keeps the sessions, in memory: an hour unused ends one, a restart ends
  all, and every use re-checks the token, so revoking or expiring it ends the session at
  once. The plugin never holds a token, and nothing weaker than a token signs anyone in.
  The caller carries `mayWrite` (false for viewers and read tokens), which a plugin must
  honour on routes it authenticates itself.
- **`routes`**, **`kv`**, **`schedule`**, **`roles`** (`source`, `api-compat`,
  `dns-provider`, `tracker-adapter`). A route may declare `"auth": "plugin"` (only with
  `api-compat`): see "Management API and plugin routes".

Plugins get **no** filesystem, process, environment or socket access, and open no
listeners. An admin can mount one plugin's routes at the root of an extra listener with
`--plugin-listen <id>=<address>` (below); the daemon owns that socket.

**Roles** the host calls into:

- `source`: `candidates()` (release candidates for rules).
- `dns-provider`: `setTxt({ domain, name, value })` and `removeTxt(…)`, for ACME DNS-01
  (see "Transport").
- `tracker-adapter`: declares `trackers` (host names, or `*.` and a domain) and provides
  `isUnregistered({ torrent, name, tracker, message })` and optionally
  `findPack({ torrent, name, infoHashV1, infoHashV2, tracker, files })` returning
  `{ magnet }`, `{ torrent: bytes }` or `null` (see "Pack switchover").
- `api-compat`: no calls; it permits routes the plugin authenticates and `tokens:verify`.

**Approval and upgrades.** A new plugin waits for approval; the admin approves a specific
version, and the grants become exactly what it declares. A new version within the grants
replaces the running one at once; one that asks for more waits as *pending*, with the new
capabilities listed in words, while the old version keeps running.

**Settings** are described by a JSON Schema subset (types, titles, defaults, enums,
ranges, lengths, `uri` and `multiline` formats, nested objects and arrays) that the host
validates and the UI renders; unsupported keywords are refused at install. Top-level
`writeOnly` strings are secrets: the API never returns them.

### Management API and plugin routes

- `GET/POST /api/v1/plugins` (list, install a bundle), `GET/DELETE /api/v1/plugins/{id}`,
  and actions under `/api/v1/plugins/{id}/-/`: `approve`, `enable`, `disable`, `logs`,
  `settings`. The `-` segment keeps them apart from plugin routes.
- **Admin only.** Installing, approving and uninstalling also need a browser session with
  recent authentication (like creating an API token), and go to the security audit log;
  enabling and disabling work with a full-scope token, so a script can switch a plugin off.
- **Plugin routes** are everything else under `/api/v1/plugins/{id}/`, with ordinary API
  authentication (viewers may `GET`). The plugin learns who called but never sees
  `Cookie` or `Authorization`, can't set cookies, and its responses carry
  `Content-Security-Policy: sandbox; default-src 'none'`, so it can't run script as this
  origin.
- **Routes the plugin authenticates** (`"auth": "plugin"`, `api-compat` plugins only, and
  said in words at approval) take requests without apitorrent sign-in, for other clients'
  logins (qBittorrent's `auth/login` and `SID` cookie). The server asks the host how the
  matched route authenticates before deciding; the host checks again before calling the
  plugin. A request that did sign in (bearer token, session, loopback with auth off) still
  names its caller, and a bad bearer token is still a 401. Such a route sees the request's
  cookies other than apitorrent's session cookies, and may set host-only cookies (no
  `Domain`) not named like apitorrent's. The Origin and Host checks apply as everywhere.
- **`--plugin-listen <id>=<address>`** serves one plugin's routes at the root of an extra
  listener (`/api/v2/…` there is `/api/v1/plugins/<id>/api/v2/…`), for clients that can't
  take a URL base. It is the API's router with the path rewritten, so the host check,
  authentication, sandboxing and listen rules are the same, and it shares built-in TLS.
  Management paths (`/-/…`) and dot segments get a 404. `--plugin-listen <id>@<slug>=<address>`
  serves that space's instance: every request there is in the space, as under
  `/s/<slug>/` (a token of another space gets 403). The space must exist at start.
- The UI's Plugins page (`/plugins`) installs, approves, enables, shows logs and permissions,
  and renders each plugin's settings form.
- On disk: `<data dir>/plugins/plugins.db` (versions, grants, settings, KV, schedule) and
  `bundles/<sha256>.js` (content-addressed, checked on load). `--no-plugins` turns the host
  off; the endpoints then answer `plugins_disabled`.

### External integrations

Anything that speaks the HTTP API with a scoped token is an external integration, in any
language and any runtime, Bun included. Plugins are for code that should live inside the
daemon, be managed by it and stay sandboxed. External integrations are for everything else.

### First-party plugins

They prove the plugin API, and they ship in the box:

- **`rss`** (built): RSS 2.0, RSS 1.0, Atom and JSON feeds; include and exclude expressions
  per feed; label, folder and paused per feed; deduplicated by guid; a new feed's existing
  items are only marked seen unless *backfill* is on; conditional requests; failed adds
  retried. Provides the `source` role: offers the items that pass a feed's filters to
  rules, and adds them directly unless the feed's `addDirectly` is off.
- **`ubuntu-releases`** (built): new Ubuntu release torrents (LTS or all, chosen flavours
  and architecture) from `meta-release` and the release directories. Provides `source`:
  offers each release to rules (see "Automation"); adds directly only if asked.
- **`qbittorrent-api`** (built): qBittorrent's Web API v2, so Sonarr, Radarr, Prowlarr,
  autobrr, cross-seed and friends work unchanged. It answers as qBittorrent 4.6.7 (Web API
  2.9.3) under `/api/v1/plugins/qbittorrent-api/api/v2/` (the *arr apps take
  `/api/v1/plugins/qbittorrent-api` as their URL base) or at the root of a
  `--plugin-listen` port. Off until approved and enabled.
  - **Sign-in**: `auth/login` takes any username and an API token as the password
    (`tokens:verify`); the `SID` cookie is the host's session id. Unauthenticated calls
    get 403, which tells clients to sign in again. Writes need `mayWrite`.
  - **Mapping**: categories are labels (a category's folder, kept by the plugin, is a path
    inside the download directory); tags are kept by the plugin; `hash` is the v1
    info-hash (or the truncated v2 one); states map to qBittorrent's (`stalledUP`,
    `pausedDL`, `metaDL`, …); share limits map to seed limits (`-2` follows the instance,
    `-1` none); `topPrio` and friends move the queue (409 without queueing, as
    qBittorrent); save paths outside the download directory are refused.
  - **Approximations**: `recheck` and `reannounce` succeed without doing anything,
    force-start resumes, completion time and last activity are approximate, session
    totals are all-time totals, and `sync/maindata` is always a full update.
  - Tested by replaying pinned conversations of Sonarr (shared by Radarr and Prowlarr),
    autobrr and cross-seed, multipart layouts included, and end to end in Rust.
- **Sources** (planned): Raspberry Pi images, open-data publishers.
- **`cloudflare-dns`** (built): ACME DNS-01 through Cloudflare's API with a token scoped
  to the zone; finds the zone by walking up the name unless a zone id is set; quoted TXT
  content; removes only the record with the given value.

**Shipping them.** `bun run build:first-party` builds every `plugins/*` into
`dist/first-party-plugins/`; `apitorrentd` built with the `first-party-plugins` feature
(release builds) embeds them. On every start the daemon offers them to the host, which
installs each as an upload would: a new plugin waits for approval (nothing runs until an
admin approves its capabilities, and it starts disabled), a newer version within the
grants upgrades, one asking for more waits as pending. A version is offered once; an older
version than the installed one never is; a plugin an admin uninstalled is never offered
again (the store remembers offers). Decided 2026-11-09, `plans/plugin-ecosystem.md`.

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
2. **Reflink / copy-on-write clone** (Btrfs, XFS, APFS, ReFS). Built, through the
   `reflink-copy` crate. It is also how the engine unshares a linked file before
   changing it, so that costs no space or time where the filesystem can clone.
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
- **Piece-level reuse.** Built. Every piece that verifies is genuinely the torrent's data,
  even when the file as a whole doesn't match (a repack that changed one region, say).
  Matching reads up to 8 pieces spread across a same-length candidate. All passing is a
  full match, which is hard-linked. Some passing is a partial match, which is cloned or
  copied. Either way the new torrent's check keeps every piece that verifies, so only the
  rest is downloaded. A change the sampling misses is still safe: the file is linked, and
  the engine unshares it before writing the differing piece.
- **Supersession waits for proof.** A source counts as superseded only once the new
  torrent's own check has verified every piece wholly inside each matched file. Sampling
  alone can miss a changed region, and retiring a torrent on a guess would be wrong.
- **v2 needs no bookends.** v2 files start on piece boundaries, so `pieces root` matches
  exactly. Piece layers then pinpoint which pieces differ when the roots don't match.
- **Fingerprints for future torrents.** Built. In idle time the library computes the v2
  `pieces root` and full SHA-256 of each complete file that came from a v1 torrent (one
  read, rate-limited, default 16 MiB/s, paused while a torrent is checking). They are
  stored by path with the file's size and modification time, and a stale one is never
  used. A later v2 or
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
   announce-failure patterns. A **tracker adapter** plugin replaces them for the trackers
   it names (`isUnregistered`); one that doesn't answer retires nothing.
4. **Pack lookups.** A torrent whose (adapted) tracker says it is gone, and which nothing
   supersedes yet, is offered to the adapter's `findPack`, at most every six hours. The
   pack it names (a magnet or `.torrent` bytes, fetched by the plugin under its own
   network grants) is added next to it, with its label; library matching reuses the
   files, the pack supersedes the single once its check proves them, and the next pass
   retires the single. All of this follows the `retireSuperseded` setting. The core sees
   adapters through `apitorrent_plugin_api::TrackerAdapters` (`apitorrent-core`'s
   `adapters` module); the plugin host implements it.

## Resource limits

Limits form a hierarchy: session, then **space**, then **group**, then torrent, then peer.
A space's limits cover all of its torrents (see "Spaces"); a group is a named set of
torrents, selected by label, tracker or rule, in any space. The tightest level that applies
wins. Queue slots work the same way: each space's are planned first, then the instance's
across every space.

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
  (`\\nas\torrents`), `smb://` or `nfs://` URLs.
- Each client profile holds **path mappings**: remote prefix → local path, with
  suggestions from the share locators.
- **Verification**: the daemon writes a marker file (`.apitorrent-root`) containing the
  root's id. A client marks a mapping verified only once it can read that marker through the
  local path, so "open folder" never lands in the wrong place.
- The desktop actions are open folder, reveal file and open file, using the mapped path and
  the OS shell (`explorer /select,`, `open -R`, `xdg-open`, or DBus FileManager1).

Built in M7. The daemon's download directory is its storage root for now: it gets a random
id per path (a moved root is a new root) and a marker `{"kind": "apitorrent-storage-root",
"id": …}`, and `GET /api/v1/roots` lists the roots. A mapping may name a root, a directory
above it, or one inside it; every root it relates to must have its marker at the matching
local place. The app re-checks the mapping on every action, not just when it was saved,
since shares get unmounted and remounted. Server-supplied paths with `.` or `..` parts, or
with names that mean something else locally (`a:b` on Windows), are refused, and "open
file" refuses programs and scripts (use "show in folder").

Share locators (`apitorrent_types::share`) are set by a server admin in Settings → Storage
roots (`PUT /api/v1/roots/{id}/locators`), checked and normalised (credentials, queries,
`.`/`..` and unknown schemes refused; at most 8 per root), kept in the instance database
by root id, and listed by `GET /roots`. The desktop app (`shares.rs`) turns each into
local candidates: on Windows the UNC path (an `smb://` URL rewritten as one; NFS exports
through the Client for NFS's UNC form); on macOS and Linux the mount of that share found
in the mount table (`/proc/self/mounts`, or `mount`), plus GNOME's GVFS folder. The root's
own path comes first when it is a path on this kind of OS. The first candidate whose marker
verifies is suggested; otherwise the first one is, unverified, with a note to mount it.
Only the instance's download directory is a storage root so far (see `ISSUES.md`).

## Automation

Built in M9 (`plans/m9-automation.md`), in `apitorrent-core/src/automation/`, with its own
database, `automation.db`. `Core::start` starts it; the server reaches it through `Core`.

### Sources and release candidates

A **source** turns some upstream into **release candidates**: `{ id, title, version,
published, links (torrent / magnet / webseed), metadata }`. Two kinds of source exist:

- **Plugins with the `source` role** call `api.candidates.submit([...])` (at most 1000 at a
  time). No API scope is needed: a candidate becomes a torrent only through a rule an admin
  or user wrote. The core checks each candidate (lengths, a parsable magnet, `http(s)`
  links, metadata up to 16 KiB) and lists the refused ones with reasons. The older pull
  method, `roles.provide("source", { candidates })`, still works.
- **Subscribed channels** (below): each entry is a candidate from `channel:<subscription
  id>`, with its `.torrent` file.

Candidates are stored per source (`plugin:<id>` or `channel:<id>`), so a rule created later
still sees releases offered earlier. Submitting one again updates it. A source keeps at most
5000; one unseen for 180 days is forgotten unless a rule still keeps it.

**`.torrent` links are fetched as the source plugin.** When a rule takes a candidate that
has only a `.torrent` URL, the core asks the plugin host (the `SourceFetcher` trait in the
plugin contract, wired by the daemon) to `GET` it with that plugin's own grants, DNS
rebinding checks and strict TLS, as the plugin's own `fetch` would; the plugin's log notes
it. The core never fetches a plugin-supplied URL with its own network access, which would
let a plugin reach hosts it was never granted. Nothing is fetched until a rule wants it. A
web seed link is added to a magnet link as `ws=`; a `.torrent` file carries its own.

`rss` offers every item that passes a feed's filters, and still adds directly per feed
(`addDirectly`, on by default). `ubuntu-releases` offers each release (point version from
the file name, release date, flavour, architecture, codename, LTS) and adds directly only
with `addDirectly` on.

### Rules (core)

A rule (`/api/v1/rules`, the Rules page at `/rules`) holds:

- **Match criteria**: sources (empty means any), a title condition and a title to
  exclude, and metadata conditions (all must hold; numbers and booleans compare as their
  JSON text, and a list matches if any element does). Each condition is *contains*, *is*,
  a *glob* or a *regular expression* (Rust `regex`, with a compiled-size cap), ignoring
  case.
- **Version ordering** over the version, the title or the publication date: **SemVer**,
  leniently (any number of dotted parts, leading zeros allowed, so Ubuntu's `24.04.1` works,
  which the `semver` crate refuses; `24.04` equals `24.4.0`; pre-releases sort before their
  release), **dates** (RFC 3339, `YYYY-MM-DD`, `YYYY.MM.DD`, `YYYYMMDD`, with time zones), or a
  **natural sort** (digit runs compared as numbers). Candidates whose version doesn't parse
  under the scheme are left out rather than guessed at; ties go to the newer publication
  date.
- **Keep the latest N** (0 keeps every match).
- **Target**: a label, a save path (absolute, or relative to the download directory), and
  whether to add paused.
- **File selection by glob**: include and exclude lists. As in `.gitignore`, a glob
  without a `/` matches the file name in any folder (`*.iso`); one with a `/` matches the
  whole path inside the torrent (`**` crosses folders, `*` doesn't). A `.torrent` gets its
  priorities when added; a magnet link gets them once its metadata arrives.
- **Superseded items**, those a newer release pushes out of the latest N: keep seeding,
  stop (pause now), or **seed limits** (a ratio and/or a time, then pause, remove, or remove
  with the data), which become the torrent's own limits through the existing seed-limit
  machinery (`seeding.rs`); nothing is duplicated.
- **An alias** (below).

**Passes.** The rules run every minute, and at once when a source submits, a rule changes
or a subscription brings a new revision; between passes a 10-second tick applies pending
file selections and updates aliases. A pass plans each enabled rule (`plan.rs`, pure) and
carries the plan out:

- *take*: add the torrent. If it's already there (added by hand or by another rule), the
  rule adopts it. A failed take is retried after 10 minutes.
- *supersede*: apply the superseded action once.
- A taken torrent that disappears (removed by someone, or by its seed limits) is marked
  *gone* and never taken again.
- Editing a rule doesn't touch what it already took; items that stop matching keep their
  state, unranked.

**Dry run** (`POST /rules/dry-run`, and in the editor): the same plan for a rule, saved or
not, against the current candidates: each match with its rank and verdict (*take*, *keep*,
*supersede*, *older*, *gone*, or *unusable* with the reason). Nothing changes.

**Seed limits** (built, in core): a ratio and/or a total seeding time, then pause, remove,
or remove with the files. The torrent's own limits win over its group's, which win over the
instance's. Seeding time counts only while actually seeding, and survives restarts. Limits
act once: a torrent the user resumes afterwards seeds on until its own limits change.
Imported from Deluge's "stop seed at ratio" settings and libtorrent's `seeding_time`.

### Stable aliases

A rule can keep a "latest" path, such as `library/ubuntu/latest-desktop-amd64.iso`, that
always names its newest completed release: the largest wanted, complete file matching the
alias's glob, in the best-ranked kept torrent that has finished. It only moves forward: a
newer release replaces it once that release completes.

- **A hard link** when the alias is on the same volume: no extra space, and it keeps
  working after the torrent it came from is removed, data and all. On another volume, or a
  filesystem without hard links, a **copy** (a copy-on-write clone where the filesystem can,
  through `reflink-copy`).
- **Not a symlink.** Windows needs a privilege (or developer mode) to make symlinks, and a
  symlink dangles once its target torrent is removed, which is exactly what superseded
  releases do. The content library avoids symlinks for the same reasons.
- **Atomic replacement.** The new link is made under a temporary name in the alias's
  folder and renamed over the alias, so a reader sees the old file or the new one, never
  a missing or partial one (Rust's `rename` replaces atomically on every platform; on
  Windows it uses POSIX rename semantics where the filesystem supports them).
- **Safe on Windows.** No symlink privilege is needed. A program that has the old alias
  open without delete sharing blocks the rename (`ERROR_ACCESS_DENIED` or
  `ERROR_SHARING_VIOLATION`); the old alias then stays intact, the rule shows "in use",
  and the next tick tries again. On Unix an open reader keeps reading the old file.
- **It only replaces its own file.** Alias paths are relative to the download directory
  (no `..`, no drive), two rules can't share one, and an existing file the alias didn't
  make is never overwritten. Deleting a rule leaves its alias file in place.

### Distribution ("make it available to my servers")

An instance can **publish a channel** (`/api/v1/channels`, admin-only): a signed,
revisioned list of torrents chosen by a rule (what it keeps, with the candidates'
versions and metadata) or by a label, with each torrent's `.torrent` file (or magnet links
only). Other instances **subscribe** over the authenticated API and run their own rules
over it, so a server farm follows one curator instance and the swarm does the data
transfer.

**The feed** (`GET /api/v1/channels/{id}/feed`, any credential, so a read-only token is
enough) is `{ format, payload, signature, publicKey }`: `payload` is the base64 of the
document's exact JSON (`{ format, channel, name, description, revision, issuedAt, entries
}`), and `signature` an Ed25519 signature over `"apitorrent-channel-v1\0"` followed by
those bytes. Signing exact bytes needs no canonical JSON; the domain tag keeps the key
from signing anything usable elsewhere. The revision moves only when the entries change
(a hash of them is stored), and never goes backwards.

**The key** is the instance's Ed25519 key, a PKCS#8 file at
`<data dir>/automation/channel-key.p8` (owner-only on Unix), made on first start. It comes
from `ring`, which the TLS stack already uses as its provider: no second implementation of
Ed25519 to build or audit, constant-time, maintained. (`ed25519-dalek` was the
alternative; its stricter verification rules matter for multi-party protocols, not for a
key pinned by the subscriber.)

**Subscribing** (`/api/v1/subscriptions`, admin-only, since a subscription stores another
server's token and makes this daemon connect out) uses `apitorrent-client`, so the trust
rules are the thin client's: the OS verifier, a private CA, or a key pinned from the
publisher's pairing code (the subscriber probes the server, checks the code names the key
it presents, and pins the full SHA-256). There is no way to skip certificate checks. Each
poll (every 5 minutes to 7 days, or "Check now"):

1. fetches the feed with the subscription's API token (stored in `automation.db`, never
   returned by the API);
2. verifies the signature against the **publisher key pinned at subscription** (given
   by the admin, or else the key of the first feed, which arrived over the verified TLS
   link and then stays pinned); another key is refused even with a valid signature;
3. requires the document to name the subscribed channel (no replaying another channel's
   feed) and refuses a **revision lower than the last accepted one**; the same revision
   is a no-op;
4. stores each entry as a candidate, checking that an included `.torrent` is the torrent
   the entry names, and wakes the rules.

The publisher learns nothing about subscribers beyond their token's use. Deleting a
subscription forgets its candidates; torrents rules took stay.

## Deluge migration

An importer reads a Deluge state directory: `torrents.state`, which is a Python pickle
(read with `serde-pickle`; no Python needed), `*.torrent`, `*.fastresume` (libtorrent
bencoded resume data, of which we read the bitfield and file info), labels and per-torrent
options. Imported torrents get a fast validation (sizes and mtimes) instead of a full
recheck. That is the onboarding path for existing Deluge users.

- **Into a space.** An import goes into one space: the request's for the web UI's
  "Import from Deluge…" (`POST /api/v1/import/deluge`, the space's admins), or
  `--space <slug>` for `apitorrentd import deluge --config <dir>`, which creates the
  space if it's missing (with Deluge's download folder as its default). Two Deluge
  instances become two spaces. Save paths outside the space's storage roots are skipped
  and listed.
- **With the daemon stopped.** The command writes the rows into `apitorrent.db`; they
  start on the daemon's next start. One planner (`apitorrent_core::deluge_import`) serves
  both ways in.
- **`--dry-run`** opens the database read-only (or not at all) and prints a plain-text
  report made to be pasted into a chat: counts, every save path and the storage root it
  falls in, labels, Deluge's global and per-torrent limits and where they would go, and
  what can't be carried over and why (skipped torrents, and Deluge options with no
  equivalent here: move-completed, upload slots, sequential download, super-seeding,
  first/last piece priority, per-label limits). Logs go to stderr, so the report copies
  cleanly.
- **Settings** (`--settings`): into `default`, Deluge's limits become the instance's; into
  another space, its rates, connections and slots become the space's, and Deluge's global
  stop-at-ratio becomes each imported torrent's own seed limits (seed limits have no space
  level).
- **Twice is safe.** Torrents the space already has are listed and left alone.

## Spaces

A space is a named partition of one daemon, with its own storage, limits, labels,
automation and members (`plans/spaces.md` has the decisions behind it). On upgrade a
`default` space (id 1) takes everything that exists; an install with one space looks and
works as before, and the UI hides the switcher.

### The space

- `apitorrent.db`'s `spaces` table: id, slug (used in URLs), name, description, and
  settings as JSON (`SpaceSettings`): **storage roots** (directories its torrents may save
  into; none means anywhere), a **default folder** (else the instance's download directory
  for `default`, its first root, or `<download dir>/<slug>`), **folders by label**, and
  **limits**: download and upload rate and connections (an engine level between the session
  and groups, `Session::set_space`) and queue slots.
- A torrent is keyed by `(space, id)` (`apitorrent_core::Key`); its id is still its
  info-hash, so URLs and qBittorrent hashes don't change. Labels are a torrent's, so each
  space has its own.
- **Deleting a space** is refused while it has torrents, rules, channels or subscriptions;
  `default` stays.
- **Network identity is shared** (decided 2026-11-09): one peer port, one DHT node, one
  peer id.

### Members and which space a request is in

- `auth.db`'s `space_members`: a role per (user, space). Server admins and the local owner
  act as admins everywhere. API tokens carry the space they were made in
  (`api_tokens.space_id`); each user's last space is remembered.
- A request's space comes from, in order: the **token's space** (a URL naming another is
  403); the **path prefix** `/s/<slug>/`, which the server strips before routing (web UI,
  API, sync socket and plugin routes alike; `OriginalUri` keeps it for links); a **host
  name** mapped with `--space-host <host>=<slug>`; then the caller's **default**, their
  last space or their first, or `default` for a server admin. A caller who isn't a member
  gets **404**, as for a space that doesn't exist, so slugs don't leak.
- Writes need the `user` role in the space; channels, subscriptions and imports need its
  `admin`. Instance settings, bans, plugin management and spaces themselves are server
  admins'.
- A space's **admins** see its members and invitations, invite people into it (see
  "Authentication"), and change or remove its existing members. They add someone who
  already has an account by inviting them: signed in, the person joins with that account.
  Adding a user directly stays with server admins, who can see every account.
- **A space keeps an admin**: its last admin can't leave it or step down (409). Server
  admins don't count as its admins (they administer every space anyway), nor do disabled
  users; a server admin can still change or remove anyone. Routes that name what
  they act on (`/spaces/…`, `/invitations…`, `/users…`) check the caller's rights there,
  not in the request's own space, so this works from a page in any space.
- `GET /space` (the request's space, the caller's role and the spaces they may switch to),
  `GET/POST /spaces` and `GET/PUT/DELETE /spaces/{slug}` (server admins),
  `/spaces/{slug}/members[/{user}]` and `GET /spaces/{slug}/invitations` (server admins and
  the space's admins), `POST /invitations`, `GET /invitations` (server admins),
  `DELETE /invitations/{id}`, and `GET /users`, `PATCH|DELETE /users/{id}` and
  `POST /users/{id}/recovery` (server admins). The public `POST /auth/enroll/info`,
  `…/start`, `…/finish` and `…/password` open a link. `GET /torrents?space=all` and `/sync?space=all` show
  every space to a server admin; summaries carry their `space`.

### One torrent in two spaces

The engine keeps **one torrent per info-hash**; each space holding it is an **entry**
(`apitorrent_engine::EntryParams`) with its own trackers, limits (rates and connections,
drawing from its space's and group's buckets), pause state and transfer counts.

- **Peer attribution.** Every connection belongs to one entry, whose limits it draws from
  and whose counts and tracker reports it adds to. A peer a tracker returned belongs to
  the entry that announced there; DHT, PEX, LSD, web seed and incoming peers belong to the
  oldest running entry.
- **Trackers.** Each entry announces to its own list, with its own `uploaded` and
  `downloaded`; `left` is the swarm's, so an entry added once the data is complete
  announces `left=0` from the start. A URL two running entries list is announced by the
  older only; pausing or removing it hands that tracker to the next. Private torrents
  keep DHT and PEX off, as before.
- **Editing an entry's trackers** (`PUT /torrents/{id}/trackers {tiers}`, the space's
  users; the details' Trackers tab): tiers are trimmed, empty ones dropped, each URL
  `http`, `https` or `udp` and listed once. The list is stored per entry
  (`torrents.trackers`, JSON; NULL means the `.torrent`'s own) and survives restarts and
  swarm moves. The engine retargets the running entry (`TorrentHandle::set_trackers`):
  a new tier that keeps a tracker of an old one takes over its state (started, tracker
  id, schedule, an announce under way), so a tracker that stays never sees `stopped` and
  `started` again; a started tier whose trackers are all gone is told `stopped`. A
  magnet still fetching its metadata can't be edited yet.
- **Internal transfers are never reported.** Data one entry's peers brought in reaches
  the others through the files (below), not the wire, so no other entry's tracker ever
  hears of it as downloaded or uploaded.
- **Files.** The engine torrent's storage is the *primary* entry's save path (the oldest;
  only the primary saves pieces, so a restart comes back on the same files). Every other
  entry gets each wanted file in its own save path once the file is complete, by the
  content library's hard link, then reflink, then copy (`Core::place_secondaries`). File
  selections are per entry; the swarm downloads the union, so a file only another space
  wants still lands in the primary's folder.
- **Removing an entry** removes only its own files (never a path another entry uses).
  Removing the primary moves the swarm to the next entry's files (`Core::move_swarm`):
  the engine torrent restarts there and checks them. The swarm runs while any entry does.
- Private trackers usually put a `source` in the info dict, so the same release from two
  trackers has two info-hashes: two torrents sharing files through the content library,
  as before.

### Automation and plugins per space

- Rules, channels and subscriptions belong to a space (`automation.db` migration 2) and
  act on its torrents and folders. A candidate belongs to the space whose source offered
  it, or to every space when the source is global; a channel id stays unique across the
  instance, since it names a feed.
- A plugin whose manifest says `"spaceAware": true` runs **one instance per space**, each
  with its own settings (`plugins.db`'s `space_settings`; `default`'s stay in `plugins`),
  key-value data and schedule (kept under `<id>@<space>`), its torrents API working on its
  space, and its candidates its space's. Its `tokens:verify` takes only tokens of that
  space, and `mayWrite` follows the owner's role there. Plugin routes go to the instance
  of the request's space; that space's admins may change its settings. Other plugins are
  global (*shared*): one instance on `default`, managed by server admins. A space's admins
  get the Plugins page for their space: they see every plugin, labelled *per space* or
  *shared*, and read and change the settings and logs of the per-space ones' instances in
  their space; installing, approving, enabling and removing stay with server admins. `rss` and `qbittorrent-api`
  are space-aware, so a Sonarr with a token from space `tv` and the URL base
  `/s/tv/api/v1/plugins/qbittorrent-api` works in `tv`.

## Performance targets

- Sustain 10,000 loaded torrents, of which 2,000 are active, on a small home server.
- The UI list stays smooth at 10,000 rows. The UI virtualises the list, and the sync
  protocol sends only the fields the UI subscribes to.
- Startup with 10,000 torrents takes under 5 s to a usable UI. Torrents resume lazily.
- Line-rate download on 10 GbE with NVMe storage. Piece hashing runs on a dedicated pool,
  using SHA-NI or ARMv8 crypto extensions where available. Writes are vectored, and the
  disk I/O pool is separate from the network.

## Updates

The desktop app and the daemon update themselves. They install only what apitorrent's own
update key signed, so no paid code-signing certificate is involved. Plan and decisions:
`plans/update-signing.md`.

- **The key** is one minisign (Ed25519) pair, in the format the Tauri CLI writes. The
  private half exists only in the GitHub Actions secrets `TAURI_SIGNING_PRIVATE_KEY` and
  `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` (made once by `scripts/update-signing-key.ts`). The
  public half, `release/update-signing.pub`, is compiled into both programs by
  `apitorrent-update`'s build script. **A build without that file never installs an
  update**; it still says when one exists. There is no switch to accept an unsigned or
  older build.
- **Releases** (`.github/workflows/release.yml`): every file gets a `.sig`; the desktop
  build makes signed updater bundles; the last job writes `latest.json` (the desktop
  app's) and `apitorrentd-manifest.json`, which lists each target's bare executable with its
  URL, size and SHA-256, and signs both. The release is a draft; programs look only at
  GitHub's latest *published* release (`/releases/latest` never returns drafts or
  prereleases), so nothing ships until a person publishes it.
- **The daemon** checks once a day (`UpdateSettings.checkDaily`, on by default, kept in
  `<data dir>/update.json`) and tells admins in the web UI. `--no-update-check`
  (`APITORRENT_NO_UPDATE_CHECK`), for installs a package manager keeps current, starts no
  updater at all: no check, the update endpoints answer `updates_unavailable` (so the UI
  shows nothing), and `apitorrentd update` reports but won't install. An admin installs with
  `POST /api/v1/system/update/install` (audited), the UI's button, or `apitorrentd
  update`; `autoInstall` (off by default) installs without asking. The install, in
  order: the manifest's signature against the compiled-in key (before parsing it), its
  `product`, a version newer than the running one, an entry for this build's target, and
  a download whose size and SHA-256 match. Everything comes from the signed manifest, not
  the release's metadata. The download lands next to the executable and is renamed over
  it (Unix: atomic; Windows: the running exe is renamed to `<exe>.old` first and removed
  on the next start). Then the daemon shuts down cleanly and restarts: under systemd
  (`INVOCATION_ID` set) it exits with code 75 for `Restart=on-failure` to start the new
  binary; otherwise it re-executes itself with the same arguments.
- **The desktop app** uses `tauri-plugin-updater` with the same key, checks daily, and
  installs when the user clicks. Local mode runs the core inside the app's process, so
  the app update is its daemon update; the in-process server has no update endpoints.
  The plugin refuses older versions, but the Tauri CLI that signs (2.10.1) doesn't record
  the version in the signature yet, so `requireSignedVersion` is off: a forged
  `latest.json` could pair a higher version number with an older signed installer. That
  needs control of the release's assets.

## Cross-platform notes

- **Paths**: torrent file names are sanitised. That covers Windows reserved names (`CON`,
  `NUL`, …), trailing dots and spaces, illegal characters, `..` and absolute components, and
  long paths: the core uses `\\?\` internally on Windows.
- **Services**: a systemd unit, a Windows service (`windows-service` crate) and a launchd
  plist. A systemd unit needs `Restart=on-failure` (or `always`) for self-updates: the
  daemon exits with code 75 to be restarted into the new binary.
- **Desktop**: Tauri 2, with WebView2 on Windows, WKWebView on macOS and WebKitGTK on Linux.

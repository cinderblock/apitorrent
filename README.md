# apitorrent

A BitTorrent client meant to replace Deluge. It has its own Rust engine, a native desktop app,
a full web UI with passkey login, and sandboxed TypeScript plugins.

> **Status: early development.** The engine downloads and seeds v1, v2 and hybrid
> torrents from `.torrent` files and magnet links, finds peers through HTTP, HTTPS and UDP
> trackers, the DHT, local discovery and peer exchange, over TCP or uTP with optional
> encryption, and can be driven from the web UI, with passkey sign-in, or from the desktop
> app (a thin client to a daemon, or the engine built in). Sandboxed TypeScript plugins
> run inside the daemon (RSS feeds and Ubuntu releases so far). Don't point it at
> anything you care about yet. See
> [`plans/apitorrent-bootstrap.md`](plans/apitorrent-bootstrap.md) for progress, and
> [`docs/architecture.md`](docs/architecture.md) for the design.

## Highlights

Most of this is built. Still to come: plugins (the sources, the qBittorrent API), rules over
feeds, and distribution channels.

- **Its own Rust engine.** BitTorrent v1, v2 and hybrid torrents, uTP, DHT, PEX, LSD,
  encryption and webseeds. The protocol core is deterministic, so it can be tested by
  simulation.
- **One API for everything.** The web UI, desktop app, CLI and plugins all use it. The
  desktop app embeds the engine, or acts as a thin client to a headless daemon. Path
  mappings open remote download folders in your file manager.
- **Secure by default.**
  - Passkey login.
  - Strict TLS: a certificate is trusted through a CA or a key pinned at pairing, never
    accepted unverified.
  - Plugins are sandboxed and get only the capabilities you grant them.
- **Skipped files stay out of your folders.** A deselected file is never in the save
  directory, not even partly: bytes it shares with wanted files go to an internal part
  store, and a file deselected after downloading moves to a held area (or is deleted), so
  selecting it again needs no download.
- **Limits that nest.** Rates, connections and memory are capped for the whole session,
  for groups of torrents (chosen by label), and per torrent. A Deluge-style queue limits
  how many torrents download and seed at once, and can let slow torrents stop holding a
  slot. Seeding stops at a ratio or a seeding time (for everything, a group or one
  torrent), then pauses or removes the torrent.
- **Bad peers are banned on evidence.** Corrupt data is traced to the peer that sent it,
  block by block, before anyone is banned. IP blocklists load from common formats.
- **A content library.** A new torrent reuses files other torrents already have (hard
  links, so no extra space), instead of downloading them again. A season pack picks up
  the single episodes you have; they keep seeding until their tracker drops them, then
  retire on their own. Shared files are copy-on-write, so one torrent never changes
  another's data.
- **Automation.** Plugins watch sources such as feeds and Ubuntu releases and offer what
  they find. Rules decide what to fetch (the latest N, with a label, a folder and only the
  files you want) and what to retire, keep a stable "latest" path, and signed channels push
  it to your other servers.
- **Moving from Deluge.** Import a Deluge config directory: torrents keep their files,
  progress, labels, queue order, limits and totals, with no rehash and no re-download.
- **qBittorrent WebAPI compatibility** (a plugin), for the *arr ecosystem.

## Development

You need Rust (stable) and Bun. CI (`.github/workflows/ci.yml`) runs the same checks on
Linux, Windows and macOS; `release.yml` builds the release binaries and installers from a
version tag into a draft release.

```sh
bun install                            # plugin SDK and first-party plugins (root workspace)
cargo test                             # Rust tests, all but the desktop app (the plugin
                                       # end-to-end test runs Bun)
cargo test -p apitorrent-desktop       # the desktop app: build web/ first, it embeds the UI
cd web && bun install && bun test      # web UI tests (web/ has its own lockfile)
bun run test:plugins                   # plugin SDK and plugin tests, from the root

# Run the daemon with the web UI at http://localhost:8742
cd web && bun run build && cd ..
cargo run -p apitorrentd -- --web-root web/dist
```

For a release build, compile the UI into the binary, so the daemon is a single file with
nothing to deploy beside it:

```sh
cd web && bun install && bun run build && cd ..
cargo build --release -p apitorrentd --features embedded-ui
```

The embedded UI is served with brotli or gzip, ETags, and year-long caching for Vite's
hashed assets. `--web-root` still overrides it. Without the feature, the daemon serves the
API only unless `--web-root` is given.

`apitorrentd --help` lists the options; each one can also be set with an `APITORRENT_*`
environment variable.

For UI work, run `bun run dev` in `web/` alongside the daemon: Vite proxies `/api` to it,
and you sign in at http://localhost:5173 as usual.
Everything that parses input from the network or from files has a fuzz target in `fuzz/`
(bencode, `.torrent` files, magnet links, the peer wire protocol and its extensions, DHT
messages, tracker responses, Deluge's pickle). With nightly and `cargo install cargo-fuzz`:
`cd fuzz && cargo +nightly fuzz run bencode -- -max_total_time=60`. On Windows, put MSVC's
`clang_rt.asan_dynamic-x86_64.dll` directory (`VC/Tools/MSVC/<version>/bin/Hostx64/x64`)
on `PATH` first.

The UI's API types in `web/src/api/generated` are generated from the Rust types with
`cargo test -p apitorrent-types`.

The desktop app (`apps/desktop`, Tauri 2) is a workspace member but not a default one, so
plain `cargo build` and `cargo test` skip its slow build; name it to include it. It embeds
the built web UI, so build that first:

```sh
cd web && bun run build && cd ..
cargo build -p apitorrent-desktop      # or: cargo test -p apitorrent-desktop
cd apps/desktop/src-tauri && cargo tauri dev     # runs Vite and the app together
cd apps/desktop/src-tauri && cargo tauri build   # installers in target/release/bundle
```

## Plugins

Plugins are TypeScript, bundled into one ES module and run inside the daemon on embedded
V8, each in its own isolate with memory limits and a watchdog. A plugin can do only what
its manifest declares and an admin approved on the Plugins page: the hosts `fetch` may
reach (strict TLS, as everywhere), torrent calls, storage, scheduled jobs, HTTP routes
under `/api/v1/plugins/<id>/`, and roles such as `source`. It has no filesystem, process,
environment or socket access. An update that asks for more waits for approval while the
current version keeps running.

- `packages/plugin-sdk` (`@apitorrent/plugin-sdk`): types, a `bun test` harness that
  enforces capabilities like the daemon, and `apitorrent-plugin-build`, which writes the
  installable `dist/<id>-<version>.plugin.json`.
- `plugins/rss`: RSS, Atom and JSON feeds with per-feed filters, deduplicated by guid.
  Offers items to rules, and adds them directly per feed (on by default).
- `plugins/ubuntu-releases`: new Ubuntu release torrents as they come out, offered to rules
  (or added directly, if you ask).

```sh
cd plugins/rss && bun test && bun run build   # then install dist/rss-1.1.0.plugin.json
```

Plugins live in `<data dir>/plugins`; `--no-plugins` turns them off. See "Plugins" in
[`docs/architecture.md`](docs/architecture.md) for the design and the full API.

## Rules, aliases and channels

`source` plugins offer **release candidates** (a title, a version, a date, links and
metadata such as the flavour). **Rules**, on the Rules page (`/rules`) or under
`/api/v1/rules`, decide what becomes a torrent:

- **Match** by source, title (contains, is, glob or regular expression, and a title to
  exclude) and metadata fields.
- **Order** by version (`24.04.1`, `2.1.0-rc.1`), date, or text with numbers compared as
  numbers, and **keep the latest N** (or all).
- **Place** them: a label, a folder, paused or not, and which files to download by glob
  (`*.iso` matches the name in any folder).
- **Retire** what newer releases push out: keep seeding, pause, or seed to a ratio or a
  time and then pause or remove (the same seed limits as everywhere else).
- **Dry run** a rule against the current candidates before saving it.
- **Keep a "latest" path**, such as `library/ubuntu/latest-desktop-amd64.iso` inside the
  download directory: a hard link to the newest completed release (a copy on another
  drive), swapped in one step, and still valid after the old torrent is removed. If a
  program has the old one open on Windows, it is swapped once it's closed.

**Channels** publish what a rule keeps, or every torrent with a label, as a signed,
revisioned list with the `.torrent` files. Another apitorrent subscribes with an API token
(a read-only one is enough) over the same strict TLS as the desktop app: by the publisher's
pairing code, a private CA or the OS. It checks each list against the publisher's channel
key (Ed25519, kept in `<data dir>/automation/`), refuses one whose revision goes back, and
runs its own rules over the entries. Channels and subscriptions are admin-only. Rules and
everything they took live in `<data dir>/automation.db`. See "Automation" in
[`docs/architecture.md`](docs/architecture.md).

## Signing in

Everything except the sign-in pages needs an account. On first start, with no users yet,
the daemon prints a one-time setup link on its console:

```text
========================================================================
 apitorrent has no users yet. To create the first admin, open

   http://localhost:8742/setup#token=abcd-efgh-ijkl-mnop-qrst-uvwx-yz23-4567
 ...
```

Open it in a browser (or open `/setup` and paste the token), pick a username and create
a passkey. That account is the admin. The token works once and expires after an hour; a
new one is printed while nobody has used one. Pass `--write-setup-token` to also write it
to `<data dir>/setup-token` (owner-only on Unix), for headless installs. It is deleted
once used.

- **Passkeys** are the sign-in method. Browsers only allow them on `https://` or
  `http://localhost`, so open the UI as `http://localhost:8742`, not
  `http://127.0.0.1:8742`. Each passkey works on the host it was made on.
- **Passwords** are off by default. `--passwords fallback` lets a password sign in on its
  own (for hosts without passkey support); `--passwords second-factor` asks for it after
  the passkey.
- **API tokens** for scripts are made on the Account page, read-only or full, and sent
  as `Authorization: Bearer <token>`.
- `--insecure-no-auth-loopback` turns sign-in off for requests from this machine. It is
  for development only, and refused with a non-loopback `--api`.

### Reaching it from other machines

The API listens on `127.0.0.1:8742` by default. The supported way to expose it is a reverse
proxy with TLS in front (Caddy, nginx), which passes the `Host` header through:

```sh
apitorrentd --api 0.0.0.0:8742 \
  --public-url https://torrents.example.com \
  --trusted-proxy 10.0.0.5
```

`--public-url` makes that host a passkey host and an accepted `Host` name, and
`--trusted-proxy` makes the daemon believe that proxy's `X-Forwarded-For` and
`X-Forwarded-Proto` headers. Without a trusted proxy a non-loopback address is refused,
unless you pass `--allow-insecure-lan` and accept plain HTTP on your network (passkeys
won't work there; use `--passwords fallback`). See "Security model" in
[`docs/architecture.md`](docs/architecture.md) for the details.

### Built-in HTTPS

The daemon can also serve HTTPS itself. With built-in TLS a non-loopback `--api` needs no
proxy and no `--allow-insecure-lan`; responses carry HSTS, and sign-in cookies are
`Secure`. TLS is always strict: there is no option to accept a certificate that fails
verification, on either end.

**Your own certificate.** Give a PEM chain (leaf first) and its key. The files are
checked every 10 seconds and a new certificate is picked up without a restart. If the
new files don't load (bad PEM, a key that doesn't match, already expired) the daemon logs
an error and keeps serving the previous certificate.

```sh
apitorrentd --api 0.0.0.0:443 \
  --tls-cert /etc/apitorrent/fullchain.pem --tls-key /etc/apitorrent/privkey.pem
```

**Let's Encrypt (ACME).** The names must resolve to this machine, and the CA must reach
it on port 443 (the default `tls-alpn-01` challenge) or on port 80 (`http-01`, which
needs `--http-redirect-port`; that listener also redirects plain HTTP to HTTPS).
Certificates renew on their own, two thirds of the way through their lifetime. Using
`--acme-domain` accepts the CA's terms of service. Try a setup with `--acme-staging`
first: its certificates aren't trusted, but its rate limits are generous.

```sh
apitorrentd --api 0.0.0.0:443 --http-redirect-port 80 \
  --acme-domain torrents.example.com --acme-email you@example.com
```

`--acme-directory` (and `--acme-directory-ca` for a root your OS doesn't trust) points at
another CA, such as a private step-ca. Wildcard names need the DNS-01 challenge, which
will come from a `dns-provider` plugin.

The names on the certificate become passkey hosts at `https://<name>:<port>`. If a port
forward makes the public port differ from `--api`'s (say 443 outside, 8443 inside), pass
`--public-url https://torrents.example.com` so passkeys and redirects use the public one.

**Self-signed, for the desktop app on a LAN.** `--tls-self-signed` makes a certificate
whose key is kept in the data directory, and prints a pairing code at startup:

```
 To pair the desktop app or CLI, check that it shows this code:

   K3QZ-7XWA-PL2M-ND4R-B6TE
```

When the desktop app first connects, it shows the code it computed from the certificate
it received. Pair only if the two match. The app then trusts exactly that key. Browsers
warn about a self-signed certificate (and won't offer passkeys behind the warning), so
for browser use prefer your own certificate or ACME. The code is also shown by
`GET /api/v1/tls`, which reports the current certificate and ACME progress.

## The desktop app

The desktop app keeps a list of **connections**, each one of two kinds:

- **A server**: a thin client to a daemon (a seedbox, a NAS). It signs in with an **API
  token**, not a passkey: passkeys belong to the server's own web address, and the app's
  window isn't it. Make a token on the server's Account page and paste it in; it's kept
  in the system keychain. The server's certificate is trusted in one of three ways, and
  never skipped:
  - it's from a public CA, or one this computer already trusts;
  - it's from your own CA (import its certificate into the connection);
  - **pairing**: enter the code the server shows (the SHA-256 of its public key), and the
    app pins that key.
- **This computer**: the engine runs inside the app. It can also serve the web UI to your
  network, with the same rules as the daemon (sign-in required; plain HTTP only behind a
  trusted proxy or if you accept the risk).

**Path mappings** let a server connection open downloads in your file manager: map a
folder on the server (`/srv/torrents`) to where this computer sees it (`\\nas\torrents`,
`/mnt/torrents`). A mapping is used only after the app reads the server's
`.apitorrent-root` marker through it, so "Show in folder" never opens the wrong place.

Closing the window keeps apitorrent in the tray (downloads go on, and a notification says
when one finishes); quit from the tray menu. Clicking a `magnet:` link or opening a
`.torrent` file brings up the Add dialog; installers register both.

## Licence

MIT. See [LICENSE](LICENSE).

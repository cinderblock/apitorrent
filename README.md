# apitorrent

A BitTorrent client meant to replace Deluge. It has its own Rust engine, a native desktop app,
a full web UI with passkey login, and sandboxed TypeScript plugins.

> **Status: early development.** The engine downloads and seeds v1, v2 and hybrid
> torrents from `.torrent` files and magnet links, finds peers through HTTP, HTTPS and UDP
> trackers, the DHT, local discovery and peer exchange, over TCP or uTP with optional
> encryption, and can be driven from the web UI, with passkey sign-in. The desktop app is
> next. Don't point it at
> anything you care about yet. See
> [`plans/apitorrent-bootstrap.md`](plans/apitorrent-bootstrap.md) for progress, and
> [`docs/architecture.md`](docs/architecture.md) for the design.

## Highlights (planned)

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
  slot.
- **Bad peers are banned on evidence.** Corrupt data is traced to the peer that sent it,
  block by block, before anyone is banned. IP blocklists load from common formats.
- **A content library.** A new torrent reuses files other torrents already have (hard
  links, so no extra space), instead of downloading them again. A season pack picks up
  the single episodes you have; they keep seeding until their tracker drops them, then
  retire on their own. Shared files are copy-on-write, so one torrent never changes
  another's data.
- **Automation.** Plugins watch sources such as feeds, Ubuntu releases and Raspberry Pi
  images. Rules decide what to fetch and what to retire, and channels push it to your other
  servers.
- **Moving from Deluge.** Import a Deluge config directory: torrents keep their files,
  progress, labels, queue order, limits and totals, with no rehash and no re-download.
- **qBittorrent WebAPI compatibility** (a plugin), for the *arr ecosystem.

## Development

You need Rust (stable) and Bun.

```sh
cargo test --workspace                 # Rust tests
cd web && bun install && bun test      # web UI tests

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
The UI's API types in `web/src/api/generated` are generated from the Rust types with
`cargo test -p apitorrent-types`.

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

## Licence

MIT. See [LICENSE](LICENSE).

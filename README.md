# apitorrent

A BitTorrent client meant to replace Deluge. It has its own Rust engine, a native desktop app,
a full web UI with passkey login, and sandboxed TypeScript plugins.

> **Status: early development.** The engine downloads and seeds v1, v2 and hybrid
> torrents from `.torrent` files, finds peers through HTTP, HTTPS and UDP trackers, and
> can be driven from the web UI. Magnet links, DHT, authentication and the desktop app are
> next. Don't point it at anything you care about yet. See
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
- **A content library.** A file can belong to several torrents. A new pack reuses the files
  you already have, and the old torrents keep seeding them.
- **Automation.** Plugins watch sources such as feeds, Ubuntu releases and Raspberry Pi
  images. Rules decide what to fetch and what to retire, and channels push it to your other
  servers.
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

The API listens on `127.0.0.1:8742` by default and refuses other addresses until
authentication is implemented. `apitorrentd --help` lists the options; each one can also
be set with an `APITORRENT_*` environment variable.

For UI work, run `bun run dev` in `web/` alongside the daemon: Vite proxies `/api` to it.
The UI's API types in `web/src/api/generated` are generated from the Rust types with
`cargo test -p apitorrent-types`.

## Licence

MIT. See [LICENSE](LICENSE).

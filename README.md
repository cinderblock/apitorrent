# apitorrent

A BitTorrent client meant to replace Deluge. It has its own Rust engine, a native desktop app,
a full web UI with passkey login, and sandboxed TypeScript plugins.

> **Status: early development.** Nothing is usable yet. See
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
- **A content library.** A file can belong to several torrents. A new pack reuses the files
  you already have, and the old torrents keep seeding them.
- **Automation.** Plugins watch sources such as feeds, Ubuntu releases and Raspberry Pi
  images. Rules decide what to fetch and what to retire, and channels push it to your other
  servers.
- **qBittorrent WebAPI compatibility** (a plugin), for the *arr ecosystem.

## Development

You need Rust (stable) and Bun.

```sh
cargo test --workspace
```

## Licence

MIT. See [LICENSE](LICENSE).

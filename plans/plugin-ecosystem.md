# Plugin ecosystem: qBittorrent API, bundled plugins, DNS-01, tracker adapters

## Goal

Finish what M8 (`plans/m8-plugins.md`) left as follow-ups, so the plugin host carries real
integrations:

1. **`plugins/qbittorrent-api`**: qBittorrent WebAPI v2, so Sonarr, Radarr, Prowlarr,
   autobrr and cross-seed work unchanged. Close the gaps in the host, core and server:
   plugin-authenticated routes, a token check, more of the torrent API, a URL base and an
   optional listener of its own.
2. **First-party plugins ship with the daemon**: built at release time, embedded, offered on
   first start, still waiting for an admin's approval.
3. **The `dns-provider` role wired to ACME DNS-01**, daemon flags (wildcards included), and a
   Cloudflare provider plugin, tested against a mock DNS API and Pebble.
4. **The `tracker-adapter` role**: per-tracker "unregistered" detection and pack lookups,
   wired into `retire_superseded`.

## Environment / context

- Worktree `C:\Users\camer\.t3\worktrees\apitorrent\t3-a954d065`, branch `t3/a954d065`,
  based on master `52b08ea`. The main thread cherry-picks these commits onto master, where
  another agent is building rules, aliases and distribution channels in core at the same
  time: keep edits to `apitorrent-core/src/lib.rs`, `apitorrent-server/src/lib.rs`,
  `apitorrentd/src/main.rs`, `apitorrent-types/src/lib.rs` and the root `Cargo.toml` small
  and separate.
- Heavy cargo runs: `~/.srm/bin/srmctl run --slots 4 --low --label apitorrent-plugins2 --
  cargo …`. Build dir is machine-wide (`~/.cargo/build/<hash>`); about 50 GB free on C:.
- Pebble and `pebble-challtestsrv` are installed in `~/go/bin`.
- No real Cloudflare, no real DNS, no real *arr installs: mocks on 127.0.0.1 only.

## Decisions already made (don't re-ask)

- **qBittorrent login maps to API tokens** (task): the password is an apitorrent API token;
  no weaker auth path. *(user)*
- **Plugin-authenticated routes need the `api-compat` role.** A route declared with
  `"auth": "plugin"` is let through without apitorrent sign-in, and the plugin checks the
  credentials itself. Only compatibility layers need that, and approval says it in words.
  *(mine)*
- **The host owns plugin login sessions.** `api.auth.login(token)` verifies the token
  (new scope `tokens:verify`) and returns an opaque session id; `api.auth.session(id)`
  re-checks on every use that the token still exists (revoking it ends the session at
  once), with a 1-hour idle timeout, in memory only. The plugin never has to keep tokens.
  *(mine)*
- **No extra login limiter**: tokens are 256-bit random values and checking one costs what a
  bearer request to the main API costs, which isn't limited either. *(mine)*
- **The plugin enforces the caller's write permission** (`caller.mayWrite`): a read token or
  a viewer can only read through the compatibility layer. *(mine)*
- **The username is ignored**: the token is the credential and carries its user. *(mine)*
- **Plugin-route cookies**: on plugin-authenticated routes only, a plugin may set cookies,
  never ones named like apitorrent's session cookies, and it sees the request's cookies
  minus apitorrent's own. *(mine)*
- **Its own listener is a daemon flag, not a capability**: `--plugin-listen
  <id>=<addr:port>` mounts one plugin's routes at `/` of an extra listener, with the same
  host check, authentication and listen rules as the API. Plugins still never open
  sockets. *(mine)*
- **Tags and category save paths live in the plugin's KV**; categories are labels. *(mine)*
- **DNS propagation is checked by the ACME manager**, not by each provider: after a provider
  publishes the record, the manager waits until the zone's authoritative servers (found by
  iterative resolution from the root) answer with it, or the resolvers named by
  `--acme-dns-resolver`. Every provider gets it for free. *(mine)*
- **Tracker adapters declare their trackers when they provide the role**
  (`trackers: ["tracker.example.org", "*.example.net"]`); the host logs them. *(mine)*
- **Pack lookups are triggered by "unregistered"**: a torrent whose tracker says it is gone,
  and which nothing supersedes yet, is offered to its adapter's `findPack`; a pack it returns
  is added next to it, library matching supersedes the single, and the next pass retires
  it. *(mine)*

## Plan / steps

1. ✅ Survey; this plan.
2. ✅ Contract: route `auth`, scope `tokens:verify`, `PluginCaller.mayWrite`, `AppApi` additions
   (app info, seed limits, rate limits, queue moves), `Credentials` and `TrackerAdapters`
   traits, validation and approval wording.
3. ✅ Host: `api.auth`, `api.app.info`, the new controls, role metadata, `route_auth` for the
   server, bundled offers, DNS provider and tracker adapter glue.
4. ✅ Core: the new `AppApi` calls; tracker adapters in `retire_superseded`, with tests.
5. ✅ Server: plugin-authenticated routes, cookies, the credentials check, `--plugin-listen`.
6. ✅ `plugins/qbittorrent-api` with Bun tests on pinned *arr request fixtures, and a Rust end
   to end test (real core, auth, server and host).
7. **(current)** TLS: DNS-01 propagation check, daemon flags, `plugins/cloudflare-dns`, mock-API and Pebble
   tests.
8. Bundled first-party plugins; `ci.yml` and `release.yml`.
9. Docs (README, architecture, this plan), web UI capability wording, final checks.

## Findings / gotchas

- **The client fixtures are written from the clients' source, not captured traffic.** No
  *arr install was used (task rule). `plugins/qbittorrent-api/test/fixtures/*.json` name
  the source files each is based on (Sonarr's `QBittorrentProxyV2.cs`, go-qbittorrent,
  cross-seed's `QBittorrent.ts`); multipart bodies are pinned byte for byte in each
  client's layout (.NET `HttpFormData`, Go `mime/multipart`, undici `FormData`).
- **Sonarr asks `app/webapiVersion` before signing in** and treats anything but 404 as
  "supported": the unauthenticated answer must be 403, as qBittorrent's is.
- **cross-seed insists on a `Set-Cookie` from `auth/login`**, so a login by a client that is
  already signed in to apitorrent (a bearer token, or loopback with auth off) still gets a
  marker `SID` cookie.
- **A category without a folder saves in the download directory** (qBittorrent's manual
  mode), which the Rust end-to-end test first got wrong.
- `cargo fmt` reorders `mod` lines that master leaves unsorted in
  `apitorrent-types/src/lib.rs`, `apitorrentd/src/main.rs` and `apitorrent-core/src/lib.rs`;
  those hunks are kept out of these commits so the shared files only carry real changes.
- **Two `bun install`s at once fail** (`EEXIST: failed to link package`): the rss e2e tests
  raced in a fresh checkout. Serialised with a lock (`eee511e`).
- Bash heredocs in this environment break on some quoting (a Python edit with `\r\n`
  silently failed to match); multi-line edits go through scratchpad scripts or the Edit
  tool.

## Progress log

- [x] Survey and plan (2026-11-09 1:00 AM PST (09:00Z))
- [x] Contract, host and server (`1baa78d`): route auth, tokens:verify, sessions,
  plugin listener, app info, wider controls, role glue, bundled offers; 7 host tests, 3
  server tests
- [x] Core tracker adapters (`56d64bf`), with a fake-adapter test
- [x] SDK types and harness (`3cb8897`)
- [x] qbittorrent-api (`d871c3d`): 10 Bun tests (3 pinned client conversations), Rust end
  to end through the URL base and its own listener
- [ ] DNS-01 and Cloudflare
- [ ] Bundled plugins and workflows
- [ ] Docs and final checks

## Open questions for the user

*(none yet)*

## Things not to do

- Don't call the real Cloudflare API or touch any real DNS.
- Don't add an "ignore certificate errors" option, anywhere.
- Don't add a qBittorrent login that accepts anything weaker than an API token.
- Don't run `cargo test --workspace` (it builds the Tauri app); use the default members.

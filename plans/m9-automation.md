# M9: automation (rules, stable aliases, distribution channels)

## Goal

Build the "Automation" section of `docs/architecture.md`:

- **Rules (core)** over **release candidates** that `source` plugins (and subscribed
  channels) submit: match, order by version, keep the latest N, label and save root, file
  selection by glob, and what happens to superseded items (reusing seed limits). Stored in
  the DB, with a REST API, a Rules UI and a dry run.
- **Stable aliases**: a rule keeps a "latest" path (`library/ubuntu/latest-desktop-amd64.iso`)
  pointing at its newest completed release; hard links, atomic replacement, safe on Windows.
- **Distribution channels**: an instance publishes a signed, revisioned list of torrents
  chosen by a rule or a label; other instances subscribe over the authenticated API with
  strict TLS and pinning, verify the signature against a pinned publisher key, refuse
  revisions that go backwards, and run their own rules over the entries.

Seed limits (`apitorrent-core/src/seeding.rs`) already exist and are reused, not duplicated.

## Environment / context

- Worktree `C:\Users\camer\.t3\worktrees\apitorrent\t3-32a8c740`, branch `t3/32a8c740`,
  based on master `52b08ea` (M8 plugins merged). The main thread cherry-picks these commits
  onto master, where another agent is adding the `qbittorrent-api` plugin, bundled
  first-party plugins and the `dns-provider` / `tracker-adapter` roles at the same time.
- Heavy cargo runs: `~/.srm/bin/srmctl run --slots 4 --low --label apitorrent-automation -- cargo …`.
  Plain `cargo test` (default members) plus `-p` for touched crates; not `--workspace`
  (the Tauri crate is slow).
- rustc 1.99, bun 1.4.2.

## Decisions already made (don't re-ask)

- **Automation lives in core** (`apitorrent-core/src/automation/`), as part of `Core`:
  `Core::start` starts it and the server reaches it through `core.automation()`. So no new
  `ServerConfig` field, nothing new for the daemon or the desktop app to wire except the
  plugin link fetcher (below). *(mine: keeps shared files nearly untouched)*
- **Its own database file, `automation.db`** in the data dir, with its own forward-only
  migrations. The architecture doc said rules live in `apitorrent.db`; a separate file means
  this milestone's migrations can never collide with the parallel agent's (both would add
  "migration 10" to `db.rs`), and automation's writes don't contend with the torrent
  database's lock. *(mine)*
- **Sources push candidates**: `api.candidates.submit([...])` in the plugin API, allowed
  for plugins that declare the `source` role (no new API scope, so the contract enum is
  untouched). The existing pull method `roles.provide("source", { candidates })` stays.
  *(mine)*
- **A candidate's `.torrent` link is fetched as the source plugin**, through the plugin
  host's own `fetch` (its grants, its DNS-rebinding checks, strict TLS), only once a rule
  takes the candidate. Core never fetches URLs a plugin named with its own privileges
  (that would let a plugin reach hosts it was never granted). The plugin contract gains a
  `SourceFetcher` trait; `apitorrent-plugins` implements it; the daemon hands it to core.
  *(mine)*
- **Plugins keep adding directly as an option.** Every feed item / release is submitted as a
  candidate. `rss` keeps adding directly per feed (`addDirectly`, default on, the previous
  behaviour); `ubuntu-releases` only submits candidates unless `addDirectly` is on (default
  off), since "latest N, superseded, alias" is exactly its use case. *(mine)*
- **Superseded handling reuses `SeedLimits`**: keep seeding, stop (pause now), or apply
  seed limits (ratio and/or time, then pause / remove / remove with data) as the torrent's
  own limits through `Core::set_seed_limits`. *(mine)*
- **Signing: Ed25519 through `ring` 0.17**, already in the tree as rustls's provider: no new
  crypto crate to build or audit, constant-time, and the same library the TLS stack trusts.
  `ed25519-dalek` was the alternative (good, but a second Ed25519 implementation in the
  binary for no gain here: keys are pinned, so malleability rules don't matter). The key is
  a PKCS#8 v2 document in `<data dir>/automation/channel-key.p8` (owner-only on Unix).
  *(mine)*
- **The signature covers exact bytes**: the feed is `{ payload: base64(JSON), signature,
  publicKey }`, signed over `"apitorrent-channel-v1\0" || payload`. No canonical-JSON
  questions; the domain prefix keeps the key from signing anything else usable. *(mine)*
- **Alias paths are relative to the download directory** and may only replace files the
  alias itself created (recorded in the DB), so a rule can't be used to overwrite arbitrary
  files. *(mine)*
- **Globs and versions are our own small, tested code** (glob → `regex`; lenient semver
  that accepts Ubuntu's `24.04.1`, which the `semver` crate rejects for its leading zero;
  ISO dates; natural sort). `regex` is already in the tree. *(mine)*

## Plan / steps

1. ✅ Read the M8 plan, contract, core, server, client; write this plan.
2. ✅ Types: `apitorrent-types/src/automation.rs` (candidates, rules, dry run, aliases,
   channels, subscriptions), exported to TypeScript.
3. ✅ Core: `automation/` module: DB, candidate store, version ordering, globs, rule engine
   (taking, file selection, superseded), dry run, aliases, channel key and publishing,
   subscriptions (via `apitorrent-client`). Unit tests plus a core integration test.
4. ✅ Plugin contract: `AppApi::submit_candidates` (default method), `SourceFetcher` trait;
   host op `op_at_candidates_submit`, `PluginHost` implements `SourceFetcher`; daemon wiring.
5. ✅ Server: `automation_routes.rs` (rules, candidates, dry run, channels, feed, key,
   subscriptions), one `.merge` line.
6. ✅ Two-instance channel test over 127.0.0.1 with self-signed TLS and pinning; bad
   signature and backwards revision rejected.
7. ← current: SDK (`api.candidates.submit`, harness), rss and ubuntu-releases submit candidates.
8. Web: `web/src/rules/` (Rules page with editor and dry run, Channels and subscriptions),
   navigation hook.
9. Docs: architecture "Automation", README, this plan. Full checks.

## Findings / gotchas

- **`semver`-style equality.** A derived `PartialEq` on the version key said `24.04` ≠
  `24.4.0` while `Ord` said equal (the unit test caught it:
  `assertion left == right failed` at `version.rs`). Equality is now defined by the order.
- **File globs and the torrent's folder.** `TorrentFile::path` includes a multi-file
  torrent's top folder (`ubuntu-24.04/ubuntu-24.04-desktop-amd64.iso`), so an alias glob
  `*.iso` never matched and the alias stayed empty in the first core test run. Globs now
  follow `.gitignore`: without a `/` they match the file name, with one the whole path.
- **A manual refresh raced the background poller.** Creating a subscription wakes the
  poller; `refresh` then found the subscription "in flight" and returned without polling
  (`revision: None, last_checked: None` in the first channel test run). Each subscription
  now has its own async lock: refresh waits, the poller skips busy ones.
- **Alias paths on Windows** came out as `downloads\library/ubuntu/…` from
  `Path::join("library/ubuntu/…")`; they are now joined component by component.
- **Windows rename over an open file.** Rust's `fs::rename` replaces atomically, but a
  reader that opened the alias without `FILE_SHARE_DELETE` blocks it (`ERROR_ACCESS_DENIED`
  / `ERROR_SHARING_VIOLATION`). That is reported as "in use" and retried every pass; the old
  alias stays intact (unit test `an_open_alias_is_retried_not_broken`, using
  `OpenOptionsExt::share_mode(FILE_SHARE_READ)`). `std::fs::File::open` itself shares
  delete, so ordinary readers in Rust don't block it.
- The subscription client (`Remote::request`) reads a whole response with no size cap. A
  publisher is pinned and authenticated, so this is a robustness gap, not a trust one; see
  open question 2.

## Progress log

- [x] Plan
- [x] Types and plugin contract (`6c96687`)
- [x] Core automation: candidates, rules, dry run, superseded, aliases, channels,
  subscriptions; unit tests and `tests/automation.rs` (`9c4bf0a`, `238a716`)
- [x] Plugin host: `api.candidates.submit`, `SourceFetcher`; host test; daemon wiring
  (`2f3d373`)
- [x] Server routes; `tests/channels.rs` (two instances, bad signature, backward revision,
  REST scopes) (`fb2f7f6`)
- [ ] SDK and plugins
- [ ] Web UI
- [ ] Docs, full checks

## Open questions for the user

1. **Plaintext subscription tokens.** A subscription's API token for the publisher is
   stored in `automation.db` as is (it must be sent), never returned by the API.
   Recommendation: fine for now (same trust as the data dir); an OS keychain later for the
   desktop app.
2. **A response size cap for `apitorrent-client`.** `Remote::request` reads whole bodies.
   Recommendation: add a capped variant when the client crate is next touched; channels
   are small (at most 1000 entries).

## Things not to do

- Don't add an "ignore certificate errors" option for subscriptions (or anything else).
- Don't let core fetch a plugin-supplied URL with the daemon's own network access.
- Don't use symlinks for aliases (Windows needs a privilege; a deleted target dangles).
- Don't add migrations to `db.rs` for automation; it has its own `automation.db`.
- Don't use HTML `title=` tooltips in the UI.

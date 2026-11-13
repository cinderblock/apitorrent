# Running beside Deluge, and moving over gradually

## Goal

Let apitorrent run on the same machine as one or more Deluge instances, on the same
files, and take torrents over a few at a time. Asked for on 2026-11-12:

1. A peer port that can't clash with Deluge's (or anything else's): picked
   automatically, not 6881.
2. Use Deluge's own API to pause a torrent there when apitorrent takes it over.
3. Dedupe: imported torrents are report-only with a one-button fix; torrents added in
   apitorrent dedupe as soon as they finish.
4. (The user will stop `deluged` for good once everything is moved; nothing to build.)
5. A GUI-driven import (a plugin if it had to be; it doesn't, there's a built-in route),
   with filters in both the GUI and the CLI. Two Deluge instances, each its own space.
6. Client identity spoofing (user agent, peer id) for trackers that only allow known
   clients, set automatically by the import.
7. Tracker passkeys: detect the passkey in tracker URLs and change it on every torrent at
   once (for when the user rolls their token). Asked the same day.

## Environment / context

- Repo `C:\Users\camer\git\apitorrent`, branch `master`. Public repo, CI is free.
- apitorrent will run on the seedbox (see memory); this machine is for tests.
- Existing pieces this builds on:
  - `crates/apitorrent-deluge`: reads a Deluge config dir (`torrents.state` pickle,
    fastresume, labels, `core.conf`). `load()` → `DelugeState`.
  - `crates/apitorrent-core/src/deluge_import.rs`: `plan()`, `write()`, `report()`,
    `run_offline()` (CLI). `Core::import_deluge` (online) is in `lib.rs`.
  - `crates/apitorrentd/src/import.rs`: `apitorrentd import deluge` flags.
  - `POST /api/v1/import/deluge` (`ImportDelugeRequest`), web
    `web/src/components/ImportDeluge.tsx` (one form, imports into the current space).
  - Dedupe: `crates/apitorrent-core/src/dedupe.rs` (planner),
    `library_index.rs` (passes, `dedupe_now`). `LibrarySettings.dedupe`:
    `off | report (default) | on`. "Deduplicate now" already exists.
  - Identity: `session.rs` makes one `PeerId::azureus(b"AT", …)`;
    `SessionConfig.client_name` goes in extension handshakes; tracker UA is
    `apitorrent/<ver>` in `apitorrent-tracker/src/client.rs`; DHT `v` is `AT01`.
  - Peer listen: `--peer-listen` default `0.0.0.0:6881`, `--dht-port` default 6882.
    Deluge's default listen range is 6881–6891 and its DHT shares the listen port, so
    both of ours can clash.

## Decisions already made (don't re-ask)

- **Peer port: random, then kept.** On first start, with no `--peer-listen`, pick a free
  port (TCP and UDP both bindable) from a high range and store it in the settings, so
  port forwards and trackers see a stable port. The DHT port follows the same rule. An
  explicit `--peer-listen`/`--dht-port` still wins. Shown and changeable in Settings
  (applies on restart unless rebinding live turns out to be easy).
- **One owner per torrent.** While Deluge's daemon is reachable and has a torrent
  active, apitorrent won't start its copy unless it has just paused Deluge's. "Take over"
  = pause in Deluge, confirm paused, then resume here.
- **Deluge API = the daemon RPC** (port 58846, TLS, zlib + rencode), not the web UI's
  JSON-RPC: the import already needs the config dir, whose `auth` file has the
  `localclient` credentials and whose `ssl/daemon.cert` lets us pin the daemon's
  certificate (no unverified TLS, per the project's TLS rule). Credentials are read from
  the config dir each time, never stored in apitorrent.db.
- **Dedupe policy:** default becomes `on`. Files of imported torrents are never replaced
  by an automatic pass; they are reported as "waiting for you", and "Deduplicate now"
  merges them too. A torrent finishing wakes a pass soon after, instead of waiting for
  the hourly one.
- **Identity is chosen per tracker, with an instance default.** Presets (apitorrent,
  Deluge/libtorrent, qBittorrent, Transmission) set the peer-id prefix, the HTTP
  User-Agent and the extension-handshake `v`. A torrent uses the identity of its
  trackers' rule (first match), for its announces and its peer handshakes. The DHT keeps
  apitorrent's. The import adds a rule, for each tracker host its torrents use, with the
  Deluge identity (exact versions from the daemon when it is reachable), unless a rule
  for that host exists. The UI warns that many private trackers forbid spoofing.
- **Import filters (CLI and GUI, same model):** label (include/exclude), tracker host,
  save-path prefix, state (seeding/downloading/paused/error), name substring, info-hash
  list. The GUI previews the Deluge torrents and lets the user tick them.
- **Two instances = two spaces**: the GUI import picks an existing space or creates one
  (slug suggested from the config dir), like `--space` on the CLI.

## Plan / steps

Work is split three ways; integration of 2 and 3 into the import happens last.

1. [x] Peer/DHT port auto-pick and persist (main thread). `ee4af6d`. Desktop app not
       compile-checked yet (`cargo check -p apitorrent-desktop`).
2. [ ] Deluge daemon RPC client in `apitorrent-deluge` (delegate, worktree).
3. [ ] Client identity: engine, tracker, settings, UI (delegate, worktree).
4. [x] Dedupe policy (main thread), `e54a3fc`: an `imported` table (migration 14:
       space, torrent, source config dir, imported_at, taken_over_at) written by both
       import paths; automatic passes hold imported copies (`heldBytes/heldFiles`),
       "Deduplicate now" merges them; imported copies preferred as keeper; default `on`;
       a swarm going Downloading → Seeding wakes a pass.
5. [x] Import filters (main thread). Clippy and tests pass on sentinel; web tests and
       typecheck pass. Not yet clicked through in a browser against a real Deluge dir.
       - core: `deluge_import::{summarize, matches, select, suggested_slug}`,
         `Core::preview_deluge`; `ImportDelugeRequest.filter`, `ImportReport.notSelected`.
       - server: `POST /import/deluge/preview` (space admin).
       - CLI: `--label --exclude-label --tracker --save-path --only-complete
         --only-incomplete --only-paused --only-active --name --hash`.
       - web: `ImportDeluge.tsx` is now a two-page wizard (source + space → preview with
         filters and ticks → import); a new space is created first (server admins only)
         with roots = the outermost save paths; `requestIn(slug, …)` in `api/client.ts`
         sends the import to another space's API. Logic in `components/delugeImport.ts`
         (tested). The GUI sends the ticked ids as `filter.ids`.
6. [ ] Take-over: per torrent and in bulk, through the RPC client; guard against two
       owners (main thread, after 2).
7. [ ] Import sets identity rules (after 3).
8. [x] Tracker passkeys (main thread). Detector in `apitorrent-tracker/src/passkey.rs`,
       grouping/replace in `apitorrent-core/src/passkeys.rs`, routes
       `/tracker-passkeys` (named so to stay clear of the WebAuthn `/auth/passkeys`), web
       `TrackerPasskeysSection.tsx` in Settings (space admins). Engine announce logs and
       tracker network errors no longer carry URLs with passkeys. Not done: the details'
       Trackers tab still shows full URLs (it is the editor for them). Original note: find them in announce URLs (a long hex/base62 path
       segment such as `/<32 hex>/announce`, or query keys like `passkey`, `authkey`,
       `torrent_pass`, `pid`), group by tracker host and passkey, and replace one passkey
       with another on every matching torrent in the space in one action (API, CLI-free,
       web UI under Trackers). Passkeys are secrets: masked in the UI and never logged.
       Builds on the existing per-torrent `PUT /torrents/{id}/trackers`.
9. [ ] Docs (`docs/architecture.md` Deluge section, README), plan wrap-up.

## Findings / gotchas

- The web import today imports into the *request's* space only; a second instance needs
  the space made first. The wizard fixes that.
- Deluge 2 keeps deselected files' boundary bytes in a `.parts` file, so the import's
  "move deselected files to the held area" should find nothing to move. Unverified;
  check with a real Deluge state when testing the take-over.

## Progress log

- [x] 2026-11-12: plan written. Delegates started for steps 2 (Deluge RPC) and 3
      (identity), each in its own worktree.
- [x] Step 1 ports, step 4 dedupe committed on master.
- [x] 2026-11-13: step 5 committed (local build slots were full, so clippy and tests ran
      on sentinel with `rbuild`). Next: step 6 take-over once the RPC delegate lands.
- [x] 2026-11-13: step 8 (tracker passkeys) committed.

## Open questions for the user

(none yet)

## Things not to do

- Don't store Deluge's password in apitorrent's database.
- Don't accept the Deluge daemon's certificate unverified: pin `ssl/daemon.cert`.
- Don't let an automatic dedupe pass replace a file of an imported torrent.

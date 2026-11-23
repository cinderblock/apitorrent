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

1. [x] Peer/DHT port auto-pick and persist (main thread). `ee4af6d`. Desktop app
       compile-checked 2026-11-13 (`cargo check -p apitorrent-desktop`, clean).
2. [x] Deluge daemon RPC client in `apitorrent-deluge` (delegate, worktree). Done
       2026-11-12: `apitorrent_deluge::rpc` (`DelugeClient`), checked against a real
       Deluge 2.1.1 daemon. API and how it was tested under Findings. Merged to master
       2026-11-13.
3. [x] Client identity: engine, tracker, settings, UI (delegate, worktree, branch
       `t3/client-identity-per-tracker`). Done 2026-11-13: profiles and per-tracker rules
       in the engine, `Settings.identity`, `Core::ensure_tracker_identity` /
       `ensure_tracker_identities` for step 7, a "Client identity" section in Settings,
       docs. See Findings, "Client identity". Merged to master 2026-11-13.
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
6. [x] Take-over (main thread), 2026-11-13. `apitorrent_core::takeover::pause_in_deluge`;
       migration 15 adds `imported.source_id`; the import takes over what it starts (or
       imports all paused with a warning); `Core::resume` refuses with `other_owner` (409)
       while Deluge runs it; `GET /import/deluge/waiting`, `POST /import/deluge/take-over`;
       Settings → Still with Deluge. No recheck on take-over: Deluge's pieces only grow,
       so the imported bitfield is a subset of what's on disk and stays safe. Tested with
       a fake daemon that accepts but won't speak TLS (refused) and a closed port
       (taken over); not yet against a real running Deluge.
       2026-11-14: tightened. A refused connection counts as "not running" only when no
       `deluged` process uses that config dir (`discover::running_daemon`, from `/proc`);
       a running one that can't be reached is an error and nothing starts here. The
       daemon's own `--port`/`--ui-interface` are used to reach it (pokol's `delugex`
       unit has `--ui-interface ::`, reached at `::1`). "Stopped" stays Deluge's polled
       answer: `Paused`, or `Error` (Deluge refuses to pause an errored torrent, and won't
       restart it by itself).
7. [x] Import sets identity rules (main thread), 2026-11-13. Both import paths add a
       Deluge rule for the tracker hosts of imported **private** torrents (public ones
       keep apitorrent's identity), with the daemon's versions when it answers.
       `ImportReport.identityHosts`; the wizard's result and the CLI report list them.
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
9. [x] Docs (`docs/architecture.md` Deluge section, README), plan wrap-up, 2026-11-13.
10. [x] Find Deluge installations (asked 2026-11-13, when installing on pokol):
    `apitorrent_deluge::discover` reads running `deluged`s from `/proc` (`--config`, or
    the user's `~/.config/deluge`), systemd units, `/home/*/.config/deluge` and the
    usual paths; `GET /import/deluge/found` (space admins) adds how many are already
    imported. The wizard lists them first; picking one fills the folder and a space of
    its own; after importing it offers "Move N over from Deluge now" (take-over) and
    "Import another installation".
11. [ ] ← current **Try it on pokol** (the user's seedbox): two daemons as
    `debian-deluged`, `/var/lib/deluged` (672 torrents, port 58846) and
    `/var/lib/delugex` (243, port 58847), files under `/mnt/storage/deluge/{d,x}`.
    Install with `packaging/linux/install.sh --binary … --user debian-deluged`
    (`plans/linux-distribution.md`), reach it over an SSH tunnel, connect the desktop
    app.
    2026-11-14 11:30 PM PST: the user wants **both** daemons moved onto apitorrent ("they're
    both real"). pokol runs `fc94fab` (installed 11:02 PM PST, 07:02Z), public at
    `https://apitorrent.isozilla.com`; its database has only the Default space and no
    torrents. Order, one user step at a time:
    a. Wizard: `/var/lib/deluged` into a **new space** (the wizard sets its storage roots
       from the save paths; Default's roots don't cover `/mnt/storage`). Tick only 2–3
       seeding torrents from a private tracker first.
    b. Check those: Paused in Deluge, seeding here, the tracker accepts the Deluge
       identity, `.parts` files (the "Still to verify by hand" list).
    c. Import the rest of `deluged` (re-running skips what's there), take over all.
    d. Same for `/var/lib/delugex` into its own space.
    e. remote-add per space (`plans/remote-add.md`), switch the tracker snippet, then
       re-run both imports for stragglers that the old endpoint added meanwhile.
    f. Later, with the user's per-change yes: stop and disable `deluged`/`delugex` and
       `http-endpoints` (ops repo).
    2026-11-22 (switchover thread): a done (the user imported `deluged` into its own
    space); a torrent has since been added to `deluged` (a straggler for e). d not yet.
    Order from here: d → remote-add enabled with a key and hosts in both spaces → tracker
    snippet → re-import stragglers from both (once, after the snippet stops feeding
    Deluge) → f. Open question 1 decides the snippet.

12. [x] "Still in Deluge" shown on each torrent (asked 2026-11-20), `9f38e36`.
    `TorrentSummary.inDeluge` (the config dir) while an import isn't taken over. The list
    and cards read "In Deluge" (warn-coloured chip) instead of "Paused"; the detail pane
    has a banner on every tab, "Starting it here pauses it in Deluge first", with a
    "Pause in Deluge, start here" button (plain resume, which already takes over);
    Pause / Resume reads "Resume, pausing it in Deluge" for such a selection. Deployed
    to pokol as part of `105a06d` (2026-11-20 1:43 PM PST, 21:43Z; schema 16), built on
    sentinel from a clean worktree (`embedded-ui,first-party-plugins`).

13. [x] Ask Deluge again on every start after the take-over (asked 2026-11-22).
    `takeover::recheck_in_deluge`: one status query; torrents Deluge runs again are
    paused there first, as in the take-over; a running daemon that won't answer refuses
    the start. After a restart, `recheck_restored` holds those torrents stopped and asks
    each source once, retrying every 10 s for 2 minutes; if Deluge never answers they go
    back to waiting (In Deluge). Migration 19, `imported.released_at`, stops the asking
    once Deluge doesn't have the torrent, its config dir is gone, or it was a leftover.
    Tests: takeover units and `deluge_take_over` (refused while a daemon holds the port,
    started after a restart). Not yet deployed to pokol.

## Findings / gotchas

- The web import today imports into the *request's* space only; a second instance needs
  the space made first. The wizard fixes that.
- Deluge 2 keeps deselected files' boundary bytes in a `.parts` file, so the import's
  "move deselected files to the held area" should find nothing to move. Unverified;
  check with a real Deluge state when testing the take-over.

### Deluge daemon RPC (step 2)

**API** (`crates/apitorrent-deluge/src/rpc/`, re-exported from `rpc`):

- `DelugeClient::connect_config_dir(&Path)`: port from `core.conf` (`daemon_port`),
  certificate pinned from `ssl/daemon.cert`, `localclient` password from `auth`, then login.
  Lower level: `DelugeClient::connect(&ConnectOptions, &PinnedCert)` + `login(&Credentials)`.
- `versions()` / `deluge_version()` / `libtorrent_version()`, `config_value(key)`,
  `torrents(Option<&[id]>) -> Vec<TorrentStatus>` (name, state, paused, progress, save_path,
  label, trackers, tracker_host, total_size, is_finished), `known_ids(ids)`,
  `pause(ids)` / `resume(ids) -> Vec<String>` (the ids acted on),
  `wait_until_stopped(ids, timeout) -> Vec<TorrentStatus>` (still running),
  `remove_keep_data(id)`, and generic `call(method, args, kwargs) -> Value`.
- Errors: `RpcError`; the daemon's own exceptions are `RpcError::Remote(RemoteError {
  exception, message, .. })` and don't break the connection. A timeout or I/O/protocol error
  does: later calls return `RpcError::Broken`, reconnect.

**Protocol facts, from Deluge 2.1.1's source and confirmed on the wire:**

- The frame header is a version byte **`1`** and a big-endian u32 length, not `b"D"` (that
  was 2.0's betas). Body is `zlib.compress(rencode.dumps(x))`.
- `RPC_ERROR` has **6** fields: `[2, id, exception_class, args, kwargs, traceback]`.
  Non-Deluge exceptions arrive as `WrappedException` with `args = (message, type, tb)`.
- Python's `rencode.dumps` writes floats as **float32** by default; replies carry those.
  The reference decoder doesn't reject trailing bytes; ours does. Its long-string form starts
  with `1`–`9` only (`0` is not a type byte).
- `daemon.login` needs a `client_version` kwarg (any value) or raises `IncompatibleClient`.
- `core.pause_torrent` / `resume_torrent` take one id or a list (2.1); with a list they
  stop at the first unknown id with a bare **`KeyError`**, and with an **empty list act on
  every torrent**. `pause()`/`resume()` look ids up first and send nothing when none match.
- Pausing turns off auto-management (so the queue won't resume it) and is asynchronous;
  the state becomes `Paused` a moment later.
- `get_torrents_status` with an `{"id": [...]}` filter silently leaves out unknown ids. An
  empty key list means *all* keys.
- `remove_torrent(id, False)` on an unknown id: `InvalidTorrentError`. `label.add` of an
  existing label: plain `Exception: Label already exists`.
- The daemon's certificate (`crypto_utils.generate_ssl_keys`) is **X.509 v1, serial 0**,
  RSA 2048, valid 3 years and never renewed. webpki can't parse v1, so rustls'
  `verify_tls1x_signature` can't be used: the verifier accepts only the exact DER from
  `ssl/daemon.cert` and checks handshake signatures against that file's key (x509-parser
  extracts it, webpki `RawPublicKeyEntity` verifies). Dates and host name aren't checked.
- The daemon negotiates TLS 1.3 (`TLS_AES_256_GCM_SHA384`) and TLS 1.2
  (`ECDHE-RSA-AES256-GCM-SHA384`), refuses 1.1 (probed with Python's `ssl`).
- `auth` lines are `user:password[:level]`; Deluge splits on every `:` (a password can't
  contain one) and skips lines with other field counts.
- Deluge 2.1.1 with today's pyOpenSSL (26.x) can't create its certificate
  (`OpenSSL.crypto` has no `X509Req`): a fresh 2.1.1 install needs pyOpenSSL 24.2 /
  cryptography 43 / Twisted 24.7 / service-identity 24.1. Existing installs are unaffected.

**How it was tested against a real daemon** (2026-11-12 1:45–2:10 PM PST (21:45–22:10Z), this
dev machine, Windows 11):

1. Venv in the session scratch dir: `py -3.12 -m venv venv`, then `pip install
   deluge==2.1.1 libtorrent setuptools<81 rencode` (libtorrent 2.1.1 wheel, rencode 1.0.9),
   then `pip install service-identity==24.1.0 twisted==24.7.0 pyopenssl==24.2.1
   cryptography==43.0.3` (see the pyOpenSSL finding).
2. A fresh config dir with a `core.conf` setting `daemon_port` 58899, `allow_remote` false,
   `listen_interface` 127.0.0.1, `listen_ports` [58900, 58900], DHT/UPnP/NAT-PMP/LSD/uTP-PEX
   off, `enabled_plugins` ["Label"].
3. `deluged -c <dir> -u 127.0.0.1 -i 127.0.0.1 -p 58899 -L info -l deluged.log`, run as
   a background process (no window), stopped afterwards.
4. `APITORRENT_DELUGE_TEST_CONFIG=<dir> cargo test -p apitorrent-deluge --test
   deluge_daemon -- --ignored --nocapture`.

What `tests/deluge_daemon.rs` proved: TLS with the pinned v1 certificate; login as
`localclient` (level 10) from the `auth` file; Deluge 2.1.1 / libtorrent 2.1.1.0;
`config_value` (`daemon_port`, `listen_ports`); adding a torrent through `call`
(`core.add_torrent_file`); listing it with every field right (label from the Label plugin,
tracker, save path, 100 %); an unknown id left out of a listing; empty `pause` touching
nothing; pause → `Paused` and `paused` true via `wait_until_stopped`; resume → `Seeding`;
raw `core.pause_torrent([unknown])` → `KeyError`, while `pause([unknown, id])` pauses `id`;
`remove_keep_data` keeps the file, and a second remove is `InvalidTorrentError`; a wrong
password is `BadLoginError` without the password in the message; a different pinned
certificate is refused at the handshake.

Unit tests: rencode vectors produced by rencode 1.0.9 (the Cython build Deluge loads),
frames produced by Deluge's own `transfer_message` logic, TLS 1.2 and 1.3 handshakes with
certificates made by Deluge's own `generate_ssl_keys()` (`tests/fixtures/ssl`, test keys
only), and a fake daemon for the client's error, event, timeout and id handling. Fuzz target
`fuzz/fuzz_targets/deluge_rpc.rs` (frame decode, and rencode decode/re-encode round trip).
It ran for 10 minutes on sentinel, 12:38–12:48 PM PST (20:38–20:48Z) on 2026-11-13:
10.5 million runs, no crash. Run it with
`rbuild -- 'cd fuzz && cargo +nightly fuzz run deluge_rpc -- -max_total_time=600'`.
cargo-fuzz 0.13.2 is installed on sentinel in `~/.cargo/bin`, alongside the nightly toolchain.

### Client identity (step 3)

Formats, from each client's source at the versions the presets use:

- **Deluge 2.1.1** on libtorrent 2.0.10: peer id `-DE211s-` + 12 characters from
  libtorrent's `url_random` alphabet (`0-9A-Za-z-_.!~*()`). Deluge calls
  `generate_fingerprint("DE", major, minor, patch, 0)` (version digits 0-9 then A-Z) and
  puts its release character in the 7th place: `s` stable, `D` dev, else the first letter
  of the pre-release suffix. User agent and `v`:
  `Deluge/2.1.1 libtorrent/2.0.10.0`.
  [core.py](https://github.com/deluge-torrent/deluge/blob/deluge-2.1.1/deluge/core/core.py),
  [generate_peer_id.cpp](https://github.com/arvidn/libtorrent/blob/v2.0.10/src/generate_peer_id.cpp),
  [fingerprint.cpp](https://github.com/arvidn/libtorrent/blob/v2.0.10/src/fingerprint.cpp).
- **qBittorrent 4.6.5**: `-qB4650-` + libtorrent's random tail; user agent and `v`
  `qBittorrent/4.6.5`.
  [sessionimpl.cpp](https://github.com/qbittorrent/qBittorrent/blob/release-4.6.5/src/base/bittorrent/sessionimpl.cpp).
- **Transmission 4.0.6**: `-TR4060-` (major, minor, patch in base 62, then `0` release,
  `B` beta, `Z` dev) + 11 lower-case base-36 characters + a check character that makes
  the sum a multiple of 36. User agent `Transmission/4.0.6`, `v` `Transmission 4.0.6`.
  [session.cc `tr_peerIdInit`](https://github.com/transmission/transmission/blob/4.0.6/libtransmission/session.cc),
  [announcer-http.cc](https://github.com/transmission/transmission/blob/4.0.6/libtransmission/announcer-http.cc).
- **Announces**: libtorrent's parameter order and escaping (lower-case hex, keeps
  `-_.!~*()`), `numwant=200`
  ([http_tracker_connection.cpp](https://github.com/arvidn/libtorrent/blob/v2.0.10/src/http_tracker_connection.cpp));
  Transmission's order, `numwant=80`, `supportcrypto`/`requirecrypto`; both `numwant=0`
  on `stopped`. A peer id and `key` per torrent, as both clients do.

What can still tell a spoof apart (also in `crates/apitorrent-engine/src/identity.rs`):

- HTTP headers: we send `accept: */*` and no `Accept-Encoding`; libtorrent sends
  `Accept-Encoding: gzip` and `Connection: close`; Transmission's curl sends its own set.
- libtorrent's `ipv4=`/`ipv6=` on private torrents and `event=paused`; Transmission's
  separate IPv4 and IPv6 announces. Not done.
- The TLS `ClientHello` of HTTPS announces (rustls, not OpenSSL or curl).
- Peers (not trackers) see handshake reserved bits, extension handshake keys (`reqq`,
  `e`, the `m` ids), uTP and encryption behaviour. Only `v` is changed.

Gotchas: a rule change takes effect at each torrent's next 1 s tick (a generation counter,
no messages), and sends `stopped` with the old peer id before `started` with the new one,
so the tracker doesn't count a ghost peer. Torrents with no rule keep the session's shared
`-AT` peer id; the DHT always uses apitorrent's `v`.

## Progress log

- [x] 2026-11-12: plan written. Delegates started for steps 2 (Deluge RPC) and 3
      (identity), each in its own worktree.
- [x] Step 1 ports, step 4 dedupe committed on master.
- [x] 2026-11-12 2:15 PM PST (22:15Z): step 2, the Deluge daemon RPC client, done and
      checked against a real Deluge 2.1.1 daemon (the throwaway daemon has since stopped).
- [x] 2026-11-13: step 5 committed (local build slots were full, so clippy and tests ran
      on sentinel with `rbuild`).
- [x] 2026-11-13: step 8 (tracker passkeys) committed.
- [x] 2026-11-13 12:48 PM PST (20:48Z): step 2 committed (`d34438b` client, `acb7bcd` fuzz
      target; fuzzed clean for 10 minutes on sentinel) and merged to master. Next: step 6
      take-over.
- [x] 2026-11-13: step 6 (take-over) committed.
- [x] 2026-11-13: identity branch merged (`2652b80`); step 7 committed (`ff4f793`).
- [x] 2026-11-13: step 9, docs pass. Everything asked for is built. Not yet tried by
      hand: the wizard, the Settings sections and a take-over against a real running
      Deluge (see "Still to verify by hand").
- [x] 2026-11-19 (`9380ddd`): wizard's path boxes explained (tap ⓘ), Escape leaves
      Settings, empty torrent list offers Import from Deluge. App rebuilt to
      `target/desktop-app/run/` (11:45 AM PST, 19:45Z); the running copy needs a restart.
- [x] 2026-11-14: take-over refuses when a `deluged` runs for that folder but can't be
      reached, and connects where the daemon's command line says.

## Still to verify by hand

These need a browser and a real Deluge, which the automated tests don't cover:

1. The import wizard against one of the real Deluge config dirs (preview, filters,
   ticks, a new space).
2. A take-over with Deluge running: Deluge's copy shows Paused, apitorrent's starts.
3. A private tracker accepting the Deluge identity after import.
4. Deluge 2's `.parts` files after an import (see Findings).

## Open questions for the user

1. ✅ Answered 2026-11-22: **change the tracker snippet** to the plugin's URLs (no nginx
   forward; a forward would have to put the key in a public, unauthenticated URL).
   Before d (asked 2026-11-22): test whether the tracker accepts apitorrent's own
   identity or needs the Deluge spoof. Current step: the user checks the tracker's rules
   for an allowed-client list. If none is mentioned, live test: switch that host's
   identity rule (Settings → Client identity) to apitorrent, watch the Trackers tab on
   one torrent for an accept or a "client not allowed" error, then put the Deluge rule
   back (same list). Then d.

## Things not to do

- Don't store Deluge's password in apitorrent's database.
- Don't accept the Deluge daemon's certificate unverified: pin `ssl/daemon.cert`.
- Don't let an automatic dedupe pass replace a file of an imported torrent.

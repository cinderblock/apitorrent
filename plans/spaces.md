# Spaces: several separate setups in one daemon

## Goal

Today the user runs two Deluge instances to keep two parts of their hosting apart. Each
part has its own storage folders, speed limits and access. One apitorrent daemon should do
the same job with **spaces**. A space is a named partition of the daemon with its own:

- storage
- limits
- labels
- automation
- members

The two Deluge instances are then imported into two spaces.

## Decisions already made (don't re-ask)

From the user, 2026-11-09:

- **Network identity is shared.** There is one listen port, one DHT node and one peer ID.
  A space has no port, interface or VPN of its own.
- **Login picks the space.**
  - A user can belong to several spaces, with a role in each.
  - The space can also come from a URL path prefix: `/s/<slug>/…`.
  - Or, optionally, from a hostname the admin maps to a space.
- **Both kinds of users.**
  - Different people use different spaces.
  - Different apps (Sonarr-style tools on the qBittorrent API) are also tied to different
    spaces.
- **Identical data is shared silently.**
  - Deleting in one space never deletes another space's copy. This works like two torrents
    sharing files today through the content library.
  - If two spaces add the *same torrent* (same infohash), there is one swarm, not two.
  - Data that moves inside the daemon between spaces is never reported to any tracker as
    downloaded or uploaded.
- **Deluge import is done by hand.**
  - Both instances are on the seedbox, and the user isn't ready to import yet.
  - The user will run the import there and paste text output back and forth.
  - The importer's output must therefore be plain text that is easy to copy.
  - Claude does not SSH to the seedbox for this.

## Design

### The space

- Each space has an id, a slug for URLs, a display name and a description.
- **Storage:**
  - the storage roots it may use
  - its default download folder
  - its folder rules
- **Limits:**
  - download and upload rate
  - connections
  - active-torrent slots
- **Its own copies of:**
  - labels
  - automation (rules, sources, channels, aliases)
  - plugin settings
- On upgrade, a `default` space is created and everything that exists today goes into it.
  An install with only one space looks exactly as it does now: the space switcher is
  hidden.

### Members and roles

- Membership is a (user, space, role) row. The role is `admin`, `user` or `viewer`, the
  same roles as today but held per space.
- A **server admin** (the global admin role) sees and manages every space, creates and
  deletes spaces, and assigns members.
- **API tokens are created inside a space** and bound to it. This is how a
  qBittorrent-API client like Sonarr lands in the right space.

### Which space a request is in

The daemon looks in this order:

1. **The token's bound space.** If the URL names a different space, the request gets 403.
2. **The path prefix `/s/<slug>/`.**
   - This covers the web UI, the API and the qBittorrent compat API.
   - Sonarr-style apps already have a "URL base" setting, so they can use it.
3. **A hostname mapped to a space** in the daemon's settings. This is optional.
4. **The user's default space.** This is the only space they belong to, or the last one
   they used.

A user who isn't a member of the resolved space gets 404, so space slugs don't leak.

### One torrent in two spaces

The engine keeps **one torrent per infohash**. Each space that has the torrent holds an
**entry**, with its own:

- name
- labels
- save folder
- seed limits
- tracker list
- added time
- state the user sees

**Files:**
- Each entry's files sit in its own space's storage.
- The content library places them by hardlink, then reflink, then copy, as it does
  today.
- Deleting an entry, with or without its data, only touches that entry's placement.
- The swarm keeps running while any entry remains.

**Swarm traffic:**
- Each peer connection belongs to exactly one entry.
- A peer from a tracker belongs to the entry whose tracker gave it.
- DHT, PEX and incoming peers belong to the first entry, which is the oldest that is
  still running.
- That entry's limits, statistics and tracker reports count the connection.

**Data from another space's copy is internal:**
- It is never added to any tracker's `downloaded` or `uploaded`.
- An entry whose files came wholly from another space announces `left=0` from the start.

**Trackers:**
- Each entry announces to its own tracker list.
- Identical announce URLs are announced once.
- Private torrents keep PEX and DHT off, as now.

Private trackers usually put a `source` field in the info dict. Then the same release
from two trackers has two infohashes, so it becomes two torrents that share files through
the content library. That already works.

### The rest of the system, per space

- **Sync socket and torrent list:** scoped to the request's space. A server admin can ask
  for all spaces.
- **Automation:** rules, sources, channels, aliases and subscriptions belong to a space.
- **Plugins:**
  - A plugin's settings and KV store are per space, for plugins that declare themselves
    space-aware. Others are global and admin-only.
  - `qbittorrent-api` serves the request's space.
- **Web UI:**
  - a space switcher in the header (only when there are several spaces)
  - `/s/<slug>/` URLs
  - an admin page for spaces and their members

### Deluge import into spaces

- `apitorrentd import deluge --config <dir> --space <slug>` imports into that space, and
  creates it if it's missing.
- A `--dry-run` prints a **plain-text report** made to be pasted back into chat:
  - counts
  - every save path and how it maps to a storage root
  - labels
  - per-torrent and global limits
  - anything that can't be carried over, and why
- Importing twice is safe: torrents already present are skipped and reported.

## Plan / steps

1. Write this design up (this file). Update `docs/architecture.md` as the work lands.
2. Data model and migration: the space table, a `default` space, and a `space_id` on
   everything that becomes per space.
3. Auth: memberships, per-space roles, space-bound tokens, and request resolution
   (token, path, hostname, default).
4. Entries: one engine torrent with several space entries; peer attribution; internal
   transfers kept out of tracker stats.
5. Limits: the space level in the limits hierarchy.
6. Automation, plugins and the qBittorrent compat API, scoped per space.
7. Web UI: the switcher, `/s/<slug>/` routing, the admin page.
8. The Deluge importer: `--space` and the `--dry-run` text report.
9. Tests at each step, and a two-space end-to-end test with the same torrent in both.

## Implementation notes (agent on branch `t3/implement-spaces`)

Worktree: `C:\Users\camer\.t3\worktrees\apitorrent\t3-6cf1fe26`. Builds run with
`CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0` through
`srmctl run --slots 4 --low --label apitorrent-spaces`, because C: had about 5 GB free when
the work started (see Findings).

### How the design maps onto the code

- **Ids.** A space has an integer id (`default` is 1) and a slug. A torrent keeps its
  info-hash id inside a space, so URLs, qBittorrent hashes and the web UI's keys don't
  change. The core keys entries by `(space id, torrent id)`.
- **Engine.** One engine torrent per info-hash, carrying one or more *entries*
  (`apitorrent_engine::EntryKey`). Each entry has its own announcer (tracker list), rate
  buckets (session → space → group → entry), connection count, pause flag and transfer
  counters. Each peer connection is tagged with one entry. Tracker peers go to the entry
  whose tracker returned them; DHT, PEX, LSD and incoming peers go to the oldest running
  entry. An announce URL two entries share is announced by the older entry only. `left` is
  the swarm's own (so an entry added after the data is complete announces `left=0`), and
  `downloaded`/`uploaded` are the entry's own peer traffic only, so data from another
  space's copy never reaches a tracker.
- **Files.** The engine torrent's storage is the oldest entry's placement (the
  *primary*). Every other entry gets its files in its own save path by the content
  library's hardlink → reflink → copy, file by file as each wanted file completes.
  Removing an entry removes only its own files. Removing the primary moves the swarm to
  the next entry's placement: the engine torrent is restarted there and checks those files.
- **Spaces table** in `apitorrent.db` (slug, name, description, settings JSON: storage,
  limits, labels). Memberships, token binding and each user's last space are in `auth.db`.

### Choices the plan didn't cover

- **Which entry is primary after a restart:** only the primary saves pieces and
  placements (`have` stays NULL for the others), and restore starts entries with saved
  pieces first, then by age. So the swarm comes back on the same files.
- **File selection across spaces:** each entry keeps its own priorities; the swarm
  downloads the union (the highest priority any entry gives a file). A file only another
  space wants still lands in the primary's save path; deselected files are the primary's
  only to hold or delete.
- **Removing the primary** restarts the swarm on the next entry's files with a full
  check (not the old bitfield), since that entry may have only some files placed. Paths
  two entries share (two spaces saving to one folder) are never deleted.
- **Groups stay instance-wide** (label → group limits apply in every space); each space
  adds its own level above them.
- **Queue:** each space's slots are planned first (0 = no limit of its own), then the
  instance's across all spaces.
- **Candidates:** a plugin that isn't space-aware offers candidates to every space's
  rules (`space_id` 0); a space-aware instance's and a subscription's are its space's only.
- **Channel ids stay instance-wide** (they name feeds), so two spaces can't both use one.
- **Deleting a space** is refused while it has torrents, rules, channels or
  subscriptions; `default` can't be deleted.
- **A space's default folder:** its `downloadDir`; else the instance's download
  directory for `default`, the first storage root, or `<download dir>/<slug>`.
- **Deluge settings into another space:** rates, connections and slots go to the
  space's settings; Deluge's global stop-at-ratio becomes each imported torrent's own
  seed limits there (seed limits have no space level). Into `default` it stays as today.
- **Existing API tokens** are bound to `default` on upgrade.
- **A space-aware plugin's `default` instance** keeps the plugin's existing settings, KV
  and schedule (stored under the bare id), so upgrading changes nothing for one space.
  Global (not space-aware) plugins work on `default` and stay server-admin only.
- **`--plugin-listen`** ports serve the `default` space (or a `--space-host` mapping);
  a client for another space uses `/s/<slug>/api/v1/plugins/qbittorrent-api` as its URL
  base.

## Progress log

- [x] Design written (2026-11-09)
- [x] Engine: entries on one torrent, peer attribution, per-entry announce and stats
      (`409c94e`; tests in `crates/apitorrent-engine/tests/entries.rs`)
- [x] Core: spaces table and migration (10), entries keyed by `Key { space, id }`, shared
      swarms (`spawn_entry`, `place_secondaries`, `move_swarm`), space limits and queue,
      `spaces.rs` (CRUD, save-path rules). Automation scoped (automation.db migration 2).
      (`bf3cd1a`)
- [x] Auth: memberships (`space_members`), per-space roles (`Auth::space_role`),
      space-bound tokens (`api_tokens.space_id`, `create_token_in`), last space;
      auth.db migration 2. `crates/apitorrent-auth/src/spaces.rs`.
- [x] Server: `/s/<slug>/` rewrite before routing, resolution (token → prefix →
      `--space-host` → last/first membership → `default` for server admins), 404 for
      non-members, `Here` extractor; `/space`, `/spaces…`, `/users`
      (`space_routes.rs`); sync and list per space (`?space=all` for server admins);
      instance settings and bans server-admin only; import and channels/subscriptions
      need the space's admin role.
- [x] Automation scoped (rules/channels/subscriptions/candidates per space).
- [x] Plugins: manifest `spaceAware`; the host runs an instance per space for those
      (`<id>@<space>` slots, kv and schedule keys; `space_settings` table in plugins.db);
      plugin routes, settings and logs take the request's space; `tokens:verify` only
      accepts a token in the instance's space. `rss` (1.2.0) and `qbittorrent-api`
      (1.1.0) are space-aware. Server calls `spaces_changed` after creating/deleting a
      space.
- [x] Web UI: switcher (hidden with one space), `/s/<slug>/` pages and API calls, the
      Spaces page (`/spaces`, from Settings) with members; rules follow the space role;
      tokens show their space (`1a2f038`)
- [x] Deluge importer: `apitorrentd import deluge --config --space [--dry-run] [--map]
      [--settings] [--paused]`, offline (rows for the next start); plain-text report;
      idempotent; the web import goes into the request's space (`c51cf6b`)
- [x] Two-space end-to-end test (`crates/apitorrent-core/tests/spaces.rs`), the server's
      (`crates/apitorrent-server/tests/spaces.rs`), auth roles, the offline import
      (`crates/apitorrent-core/tests/deluge_offline.rs`) (`34b8039`, `c51cf6b`)
- [x] docs/architecture.md ("Spaces", "Deluge migration", limits, auth) and README
      (`26b36db`)
- [x] Side fix: pack supersession decided from the check's bitfield (`61c3f4f`; it was
      ISSUES.md's flaky `a_repack…` test)
- [x] Smoke test of the real daemon (headless, loopback, no auth): `/space`, creating a
      space, `/s/tv/…`, 404 for an unknown slug, the UI at `/s/tv/rules`, one magnet in two
      spaces listed apart.

## Not done (follow-ups)

1. **Users other than the first admin can't be created yet** (invitations were already
   "still to come"). Members can be assigned only from existing users, so "different
   people in different spaces" waits for that feature; space-bound tokens (one per app)
   work today.
2. **A space's tracker list can't be edited**: each entry announces to the trackers of
   the `.torrent` (or magnet) it was added with in that space.
3. **The Plugins page is for server admins only in the UI.** The API already lets a
   space's admins read and change a space-aware plugin's settings and logs for their
   space; the page doesn't offer it to them.
4. **`--plugin-listen` serves `default`** (or a `--space-host` mapping). A client of
   another space uses the `/s/<slug>/` URL base. `--plugin-listen <id>@<slug>=<addr>`
   would be a small addition.
5. **Another space's copy appears per completed file**, not while downloading (hard links
   made at add time would show it at once on one volume).
6. **The web UI wasn't looked at in a browser** (typecheck, unit tests and build only; no
   foreground windows on this machine). The desktop thin client's `/s/<slug>/` paths are
   untested.

## Findings / gotchas

- 2026-11-09 5:30 PM PST (01:30Z): C: had 4.9 GB free. The only apitorrent build dir,
  `~/.cargo/build/be/4d2286c9a7a918`, had a 9.6 GB `debug/incremental` idle since
  12:16 PM PST; it was deleted (as the bootstrap plan's "Disk full" entry did before),
  leaving 15 GB.
- About 6:00 PM PST (02:00Z) another agent's apitorrent worktree
  (`~/.cargo/build/ba/280ad6abe847eb`) started a big build and free space fell to 719 MB.
  This worktree's own build dir (`~/.cargo/build/84/cdeb52836e5cf7`, 4.8 GB) was deleted
  to keep C: from filling; it had only finished the engine step's build. Afterwards:
  build only the crates being changed (`-p …`), and check free space before writing
  files (a full disk truncates whole-file writes).
- `cargo clippy --workspace` includes the Tauri desktop app, which embeds the web UI
  (`apitorrent-webui` with `embed`) and so needs `web/dist` built first. It is a
  multi-GB build: run it last, when there is room.
- About 7:55 PM PST (03:55Z) C: hit 0 MB free (not this worktree's doing; no cargo was
  running here). This worktree's build dir was deleted again; free space came back to
  only ~1 GB. Uncommitted work was snapshotted with a temporary index into
  `refs/wip/implement-spaces` (no working-tree or branch change).
- Engine `Shutdown` replied before unregistering the torrent from the session, so adding
  the same info-hash again right after (moving a swarm) could fail as a duplicate. It
  now unregisters first.
- Saving resume data while a torrent is still checking stored an empty bitfield, so the
  next start trusted that nothing was there and re-downloaded. It now stores none (the
  next start checks). Found by the end-to-end test's restart after a swarm move.
- After removing the primary, the store still shows the old "Seeding" summary until the
  next one-second refresh: tests must ask `detail()` (live) before restarting.
- A peer link closes once both ends are seeds, so "which entry owns this connection"
  must be sampled during the download (the engine test throttles its seeder for that).
- Windows reserves some ports (18742 was): use another for a smoke test.
- The only other drives are network shares (`P:`, `S:`, `T:`, `W:` on uberfall); not
  used for builds.

## Open questions for the user

1. The peer-attribution rule is the design's own call; say if it should be different.

Answered 2026-11-10:

- **User invitations come next:** yes. Being built: see "Invitations" below.
- **Space admins get the Plugins page:** yes. Plugins are either per space (space-aware: one
  instance per space, settings and data per space) or shared (one instance for the whole
  daemon, server admins only). A space admin sees and manages their space's space-aware
  plugins; shared plugins stay with server admins.

## Invitations (next, 2026-11-10)

- **Who invites:** a server admin can invite into any space. A space admin can invite
  only into their own space.
- **The link:** an invitation is a one-time enrollment link that expires, by default after
  7 days. It can be revoked. It carries the spaces and roles the new user gets.
- **Enrolling:** the person opening the link picks a username and registers a passkey. A
  password is allowed only when the daemon allows passwords.
- **Managing users (server admins):**
  - list users
  - disable or delete a user
  - change instance roles
  - issue a recovery link for a user who lost their credentials, built the same way as an
    invitation

## Things not to do

- Don't give a space its own port, DHT node or peer ID (user decision).
- Don't double-count internal transfers in tracker statistics.
- Don't SSH to the seedbox for the import.

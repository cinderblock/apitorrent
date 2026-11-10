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

## Progress log

- [x] Design written (2026-11-09)
- [ ] Implementation: delegated to a background agent in a worktree

## Open questions for the user

None right now. The peer-attribution rule above is my own call; say if it should be
different.

## Things not to do

- Don't give a space its own port, DHT node or peer ID (user decision).
- Don't double-count internal transfers in tracker statistics.
- Don't SSH to the seedbox for the import.

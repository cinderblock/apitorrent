# apitorrent

A new BitTorrent client that takes the best ideas from the clients people already love
(Deluge, qBittorrent, Transmission and others) and builds on them, with features of its own
for the communities that use them. It has its own Rust engine, a native desktop app, a full
web UI with passkey login, and sandboxed TypeScript plugins.

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

Most of this is built.

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
  - Signed updates: the daemon and the desktop app install only builds signed with
    apitorrent's key, never an older version, and only when you say so.
- **Skipped files stay out of your folders.** A deselected file is never in the save
  directory, not even partly: bytes it shares with wanted files go to an internal part
  store, and a file deselected after downloading moves to a held area (or is deleted), so
  selecting it again needs no download.
- **Files in order, watched while they download, moved when done.** A torrent's files can
  download in an order (by name, by size, or the torrent's own): a strong preference,
  not a rule, so peers without the first file's pieces still supply the next ones. A
  file marked "download in order" comes front to back, its start and end first (where
  players look for a video's header and index), so it can be played as it arrives.
  "First and last pieces first" fetches every wanted file's ends early, for previews. A
  completed file can move on its own (to a public folder, say) before the rest of its
  torrent is done, or every file once the whole torrent is, by patterns such as
  `*.mkv`; it keeps seeding from where it went. Set for everything, a group, or one
  torrent.
- **Limits that nest.** Rates, connections and memory are capped for the whole session,
  for groups of torrents (chosen by label), and per torrent. A Deluge-style queue limits
  how many torrents download and seed at once, and can let slow torrents stop holding a
  slot. Seeding stops at a ratio or a seeding time (for everything, a group or one
  torrent), then pauses or removes the torrent. A torrent can have its own number of
  upload slots, and the first seed of a new torrent can super-seed (BEP 16): peers see a
  piece or two at a time, so the first full copy gets out with less upload.
- **Edit many torrents at once.** Select torrents (a checkbox per row, or Ctrl- and
  Shift-click) and Edit sets their label, rate and connection limits, seed limits,
  trackers (add, remove, replace a URL), deselected-file policy and location for all of
  them, showing where they differ. Moving files runs in the background, one torrent at a
  time, and keeps what was downloaded. Recheck verifies the files on disk again.
- **Knows whether peers can reach it.** The status bar says whether the peer port is open,
  judged from connections that come in on their own (over TCP and uTP, and DHT queries
  from nodes it never contacted), with no outside service. It shows the public address
  that peers, DHT nodes and trackers see, and notices when another NAT sits in front of
  the router, where a port forward on the router can't help.
- **Bad peers are banned on evidence.** Corrupt data is traced to the peer that sent it,
  block by block, before anyone is banned. IP blocklists load from common formats.
- **A content library.** A new torrent reuses files other torrents already have (hard
  links, so no extra space), instead of downloading them again. A season pack picks up
  the single episodes you have; they keep seeding until their tracker drops them, then
  retire on their own. A tracker-adapter plugin can tell what "dropped" means on its
  tracker, and fetch the pack that replaced a deleted single. Torrents running at the
  same time share too: two downloading the same file download it once, and a file one
  finishes is linked into another still waiting for it. Each file shows where its bytes
  came from. Library folders (a media folder you already have) are candidates too, read
  but never changed, and identical files that were downloaded separately are merged
  into links to free the space, soon after a torrent finishes. Files of imported torrents
  are only reported until you say to merge them. Shared files are copy-on-write, so one torrent never changes another's data.
- **Automation.** Plugins watch sources such as feeds and Ubuntu releases and offer what
  they find. Rules decide what to fetch (the latest N, with a label, a folder and only the
  files you want) and what to retire, keep a stable "latest" path, and signed channels push
  it to your other servers.
- **Spaces.** One daemon can be several separate setups, each with its own storage,
  limits, labels, rules, plugin settings and members, behind one peer port. The same
  torrent in two spaces is downloaded once and seeded as one swarm; each space keeps its
  own files and its own tracker statistics.
- **Moving from Deluge.** Import a Deluge config directory: torrents keep their files,
  progress, labels, queue order, limits and totals, with no rehash and no re-download.
  A wizard previews and filters what to bring over. Deluge can keep running while you
  move a few at a time: a torrent is paused in Deluge (through its daemon) before it
  starts here, and private trackers keep seeing Deluge. Two Deluge instances can become
  two spaces.
- **Client identity per tracker.** For trackers that only accept clients on their list,
  a torrent can announce as Deluge, qBittorrent or Transmission (peer id, user agent and
  announce format), chosen by tracker host. Changing it applies within a second: the
  tracker sees the old client stop, then a new one start, with nothing shared between
  them and no bytes reported twice. Many private trackers forbid this: check the
  tracker's rules first.
- **qBittorrent Web API compatibility** (a plugin), so Sonarr, Radarr, Prowlarr, autobrr
  and cross-seed work unchanged, signing in with apitorrent API tokens.

## Installing

**The desktop app**: the installers on the [latest
release](https://github.com/cinderblock/apitorrent/releases/latest) (`.exe`/`.msi`,
`.dmg`, `.deb`/`.rpm`/`.AppImage`). It runs the engine itself, or connects to a daemon.

**The daemon on a Linux server**, as a systemd service:

```sh
curl -fsSL https://github.com/cinderblock/apitorrent/releases/latest/download/install.sh | sudo sh
```

It installs `apitorrentd` in `/opt/apitorrent`, keeps its data in `/var/lib/apitorrent`,
runs it as a new system user `apitorrent`, starts it, and prints the link for creating
the first admin. Settings go in `/etc/apitorrent/apitorrentd.env` (any `APITORRENT_*`
variable from `apitorrentd --help`). The API stays on `127.0.0.1:8742`: open it through
an SSH tunnel, `ssh -L 8742:localhost:8742 your-server`, at `http://localhost:8742`
(passkeys work there), and point the desktop app at the same address. To reach it
without a tunnel, see "Reaching it from other machines" below.

- `--user deluge` runs it as an existing user instead. Moving from Deluge, use Deluge's
  user: apitorrent then reads Deluge's config and shares its files.
- `--data-dir`, `--download-dir`, `--version vX.Y.Z`, `--binary ./apitorrentd` (a build
  of your own); `--uninstall` removes the program and keeps the data. `--help` lists
  them all.
- `--remote` listens on all addresses with built-in HTTPS (a self-signed certificate;
  the desktop app pins its key by the code the installer prints), so the app connects
  without a tunnel.
- It updates itself (Settings → Updates). Running the script again also upgrades, and
  keeps the choices made the first time.

Elsewhere, run the `apitorrentd` executable from the release directly.

## Development

You need Rust (stable) and Bun. CI (`.github/workflows/ci.yml`) runs the same checks on
Linux, Windows and macOS; `release.yml` builds the release binaries and installers from a
version tag into a draft release.

The reflink tests (`cargo test -p apitorrent-core -p apitorrent-engine --lib reflink`) only
prove cloning on a volume that clones. Set `APITORRENT_REFLINK_DIR` to a directory on
Btrfs, XFS, APFS or ReFS, and they fail unless a clone succeeds there. CI's reflink job
does this on loop-mounted Btrfs and XFS.

```sh
bun install                            # plugin SDK and first-party plugins (root workspace)
cargo test                             # Rust tests, all but the desktop app (the plugin
                                       # end-to-end test runs Bun)
cargo test -p apitorrent-desktop       # the desktop app: build web/ first, it embeds the UI
cd web && bun install && bun test      # web UI tests (web/ has its own lockfile)
bun run test:plugins                   # plugin SDK and plugin tests, from the root
bun run build:first-party              # the bundles a release daemon ships
cargo build -p apitorrentd --features first-party-plugins   # ...embedded, offered at start

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
- `plugins/qbittorrent-api`: qBittorrent's Web API v2 (see below).
- `plugins/cloudflare-dns`: ACME DNS-01 records through Cloudflare's API (see "HTTPS").
- `plugins/remote-add`: add a torrent by opening a link with a key in it (see below).

Release builds ship these first-party plugins: on start the daemon installs any it hasn't
offered before, and each waits on the Plugins page for an admin to approve what it asks
for. One you uninstall isn't offered again. To build and install one by hand:

```sh
cd plugins/rss && bun test && bun run build   # then install dist/rss-1.1.0.plugin.json
```

Plugins live in `<data dir>/plugins`; `--no-plugins` turns them off. See "Plugins" in
[`docs/architecture.md`](docs/architecture.md) for the design and the full API.

### qBittorrent clients (Sonarr, Radarr, Prowlarr, autobrr, cross-seed)

Approve and enable `qbittorrent-api` on the Plugins page, then add apitorrent to the client
as a qBittorrent download client:

- **Host and port**: apitorrent's, and **URL base** `/api/v1/plugins/qbittorrent-api`
  (for another space, `/s/<slug>/api/v1/plugins/qbittorrent-api`, and a token made in
  that space). A client that can't take a URL base gets a port of its own:
  `--plugin-listen qbittorrent-api=127.0.0.1:8080` serves the API at that port's root
  (the same rules as `--api` apply to where it may listen), for `default`;
  `--plugin-listen qbittorrent-api@tv=127.0.0.1:8081` serves space `tv` there.
- **Username**: anything. **Password**: an apitorrent API token (Account page, API
  tokens). A `read` token can watch but not change anything. Revoking the token signs
  the client out at once. There is no other password: the token is the credential.
- **Category**: a label. A category's folder must be inside the download directory, as
  must any save path a client asks for; tags are kept by the plugin.

It reports itself as qBittorrent 4.6.7 (Web API 2.9.3). What differs from qBittorrent:
`recheck` and `reannounce` succeed without doing anything (apitorrent checks data when a
torrent is added and announces on its own schedule), "force start" resumes, completion
times are approximate, and `sync/maindata` always sends a full update. Adding by `.torrent`
URL works for the hosts listed in the plugin's "download hosts" setting (a Prowlarr or
Jackett address, say); magnet links and uploaded files always work.

### Adding from a link (`remote-add`)

For links you inject into a tracker's pages with your own JavaScript there: opening
`/api/v1/plugins/remote-add/add?key=<key>&url=<.torrent link>` makes apitorrent fetch the
`.torrent` and add it, so the file never passes through your browser. Such a script can
only open a URL (it can't send an apitorrent token in a header to another site), so the
key rides in the link, where proxy logs and the tracker's staff can see it. It is narrow
on purpose:

- **The key only adds.** It's the plugin's own setting (random, at least 32 characters),
  not an apitorrent API token.
- **Only from the hosts you list** (your tracker), redirects included, so a leaked key
  can't make apitorrent fetch anything else. No magnet links.
- **Per space**: in another space use `/s/<slug>/api/v1/plugins/remote-add/add` with that
  space's key. Label, folder and "add paused" are settings too.

The reply is a short page for a popup (plugin pages can't run script, so it can't close
itself), or, with `&reply=image`, a 1×1 GIF when the torrent was added or already there and
an error status otherwise, so an `<img>`'s `load`/`error` events show the result on the
tracker's page without a popup:

```js
const img = new Image();
img.onload = () => mark(link, "added");
img.onerror = () => mark(link, "failed"); // open the URL without &reply=image to see why
img.src = `${BASE}/add?key=${KEY}&url=${encodeURIComponent(torrentUrl)}&reply=image`;
```

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
runs its own rules over the entries. Rules, channels and subscriptions belong to a space;
channels and subscriptions are for the space's admins. Rules and everything they took live
in `<data dir>/automation.db`. See "Automation" in
[`docs/architecture.md`](docs/architecture.md).

## Spaces

A space is a separate setup inside one daemon: its own torrents, storage roots and default
folder, folders by label, rate, connection and queue limits, labels, rules, plugin
settings and members. Every install starts with one, `default`, and looks and works as it
always did until a server admin adds another (Settings → Spaces).

- **Pages and API** of a space are under `/s/<slug>/`: `/s/tv/` in a browser, and
  `/s/tv/api/v1/…` for scripts. A header switcher appears once you have more than one.
- **Members** hold a role per space: viewer, user or admin. Server admins (the
  instance's admins) use every space. Someone who isn't a member gets 404, as if the
  space didn't exist. A space's admins invite people into it and manage its members
  (Settings → Spaces, Members and invitations), and manage its per-space plugins'
  settings on the Plugins page.
- **API tokens** work in the space they were made in (switch to it on the Account page
  first), so a Sonarr or Radarr signed in with one lands in its space.
- **The same torrent in two spaces** is one swarm: downloaded once, with each space's
  copy of the files placed by hard link (a clone or a copy across disks). Removing it
  from one space, with its files or not, leaves the other's files and seeding alone.
  Each space announces to its own trackers and reports only its own transfers: data one
  space brought in is never reported to another space's trackers. A space's users edit
  its tracker list in the details' Trackers tab (add, remove and reorder tiers); a
  tracker two spaces list is announced to once.
- **Tracker passkeys.** Settings → Tracker passkeys lists the passkeys private trackers
  put in this space's announce URLs (`/<passkey>/announce`, `?passkey=…`, `authkey`,
  `torrent_pass`, `pid`), with the hosts and how many torrents use each. After rolling
  one at the tracker, enter the new one there and every torrent that used the old one
  gets it. Passkeys are shown masked and kept out of the logs.
- **One network identity**: a single peer port, DHT node and peer id for every space
  (a client identity rule gives the torrents on its trackers a peer id of their own).
- `--space-host torrents-tv.example.com=tv` puts requests for a host name in a space.

### Importing Deluge into a space

With the daemon stopped:

```sh
apitorrentd import deluge --config ~/.config/deluge --space tv --dry-run
apitorrentd import deluge --config ~/.config/deluge --space tv
```

`--dry-run` changes nothing and prints a plain-text report (counts, every save path and
the storage root it falls in, labels, limits, and what can't be carried over). The import
creates the space if it's missing; the torrents start when the daemon does. `--map
FROM=TO` rewrites save paths, `--settings` takes Deluge's limits, `--paused` adds
everything paused. Running it again skips what the space already has.

To take only some torrents, filter them (each flag repeats; a torrent must match every
kind given, and any one value of a kind): `--label tv`, `--exclude-label music`,
`--tracker example.org` (and its subdomains), `--save-path /srv/torrents/tv`,
`--only-complete` or `--only-incomplete`, `--only-paused` or `--only-active` (as in
Deluge), `--name "words in the name"`, `--hash <info-hash>`. The report says how many
the filter left out.

Everything about each torrent comes over unless you leave it out: `--no-labels`,
`--rename-label FROM=TO` (an empty TO drops that label), `--no-queue-order`,
`--no-added-dates`, `--no-totals` (downloaded/uploaded and seeding time),
`--download-skipped`, `--original-trackers` (the .torrent's, not Deluge's edited lists),
`--no-limits` (rates, connections and upload slots), `--no-seed-limits`,
`--no-move-completed`, `--no-sequential`, `--no-first-last`, `--no-super-seed`. `--identity deluge|none|qbittorrent|transmission` sets
what private trackers see (`--no-identity-for HOST` skips one). `--settings` takes all of
Deluge's own settings; `--setting NAME` takes one (`download-rate`, `upload-rate`,
`connections`, `slots`, `slow-torrents`, `stop-at-ratio`, `encryption`). The report lists each choice,
and which of Deluge's settings were changed from Deluge's defaults.

With the daemon running, Settings → Deluge → "Import from Deluge…" does the same as a
wizard. It starts with the Deluge installations it finds on the server (running
`deluged`s and their `--config`, systemd units, `~/.config/deluge`, Debian's
`/var/lib/deluged`), each a click away with a space of its own. It lists the chosen
installation's torrents, with the same filters and a tick box each, then imports the
ticked ones into a space you choose, or into a new one it creates (server admins), with
the outermost save paths as its storage roots. Before importing, "What comes over" lists
every option the ticked torrents carry (files and progress, labels, queue order, dates,
totals, skipped files, edited trackers, limits, stop-at-ratio, move when complete,
sequential download, first and last pieces first, super-seeding, the identity private
trackers see, whether running ones start here, and each of Deluge's own settings, marked
when changed from Deluge's default), each
with what it amounts to and a box to leave it out; labels can be renamed on the way in,
and the tracker identity changed. What can't come over is listed too. Afterwards it offers to move them over
from Deluge at once, and to go on to the next installation.

Deluge can keep running while you move over. A torrent runs in only one client at a time:
apitorrent starts an imported torrent only after pausing it in Deluge, through Deluge's
daemon, using the port, certificate and `localclient` password from the config directory
(read each time, never stored), then waits until Deluge itself reports them paused and
its trackers have answered its `stopped` (at most 20 s). The tracker sees Deluge quit,
then apitorrent start from zero: Deluge's imported totals are shown here but never
announced again, so no byte counts twice. If no
daemon answers there and no `deluged` process uses that folder, it just starts; a daemon
that runs but can't be reached leaves the torrent stopped here, with the reason. The import
does this for the torrents it starts; ones imported paused wait, and resuming one takes
it over then. Settings → Deluge → "Take over all" takes over everything still waiting at once.

Private trackers that knew your torrents as Deluge's keep seeing Deluge: the import adds
a client identity rule for each of their hosts (with the running daemon's exact Deluge
and libtorrent versions, when it answers), unless a rule of yours already covers the
host. Public trackers aren't touched.

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
a passkey. That account is the admin. The token works once and expires after ten
minutes; a new one is printed while nobody has used one. Pass `--write-setup-token` to
also write it to `<data dir>/setup-token` (owner-only on Unix), for headless installs. It
is deleted once used.

**A headless server needs no browser at all.** In the desktop app, add the server, choose
"Register with a token", and give it the setup token from the server's console (or its
log, `journalctl -u apitorrentd`) and a username: the app makes the first admin and
becomes that admin's device. To use the web UI too, open the app's Account page and choose
"Open in my browser, signed in": the server makes a new one-use link (ten minutes), the
browser opens it, creates a passkey for the server's address, and is signed in; that
passkey signs it in from then on. Once the server is set up, a new device registers with a token from the server's command line, which works once
for ten minutes:

```sh
sudo -u <the daemon's user> apitorrentd register-device --data-dir /var/lib/apitorrent [--user NAME]
```

It isn't printed at every start, because a server's log is often readable by more people
than its data directory. A device registered this way works in every space its user is
in.

- **Passkeys** are the sign-in method. Browsers only allow them on `https://` or
  `http://localhost`, so open the UI as `http://localhost:8742`, not
  `http://127.0.0.1:8742`. Each passkey works on the host it was made on.
- **Passwords** are off by default. `--passwords fallback` lets a password sign in on its
  own (for hosts without passkey support); `--passwords second-factor` asks for it after
  the passkey.
- **API tokens** for scripts are made on the Account page, read-only or full, and sent
  as `Authorization: Bearer <token>`.
- **Paired devices**: the desktop app pairs instead of taking a token. It shows a short
  code and opens the server's `/pair` page in your browser, where you approve it (or enter
  the code under Account → Paired devices), or it registers with a token from the
  server's console (above). Each device is listed there and can be removed on its own.
  A device may also be allowed to manage the account (the approval's checkbox; devices
  registered from the console always are): it can then open the web UI signed in on a
  browser, which creates a passkey there first, and make API tokens.

### Inviting people

Everyone after the first admin joins with an **invitation**: a link that works once and
expires (after 7 days unless you choose otherwise). It carries the spaces and roles the
new user gets; they open it, pick a username and create a passkey (or a password, with
`--passwords fallback`). The secret is in the link's `#` part, so it never reaches a
server log. Send it privately.

- **Server admins** invite from Settings → Users, into any spaces and with any server
  role; a **space's admins** invite into their own space from Settings → Spaces.
- **Someone who already has an account** opens the link while signed in and picks "Join
  as …": the invitation's spaces are added to their account (a higher role they already
  have stays), and no new user is made. That is how a space's admins add existing users.
  Joining never changes the server role, so an invitation that makes a server admin only
  creates a new account.
- A space's **last admin** can't leave it or step down; make someone else its admin
  first. Server admins don't count as its admins, and can still change anything.
- The **Users** page (server admins) lists everyone; changes server roles; disables a
  user (signed out everywhere at once, API tokens paused until enabled again) or deletes
  one; and issues a **recovery link** for someone who lost their passkey. Opening it
  replaces their passkeys and password with a new one and signs out their devices. You
  can't disable, demote or delete yourself, and the last server admin stays.
- Waiting invitations can be revoked; used, revoked and expired ones are listed for a
  month.
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
another CA, such as a private step-ca.

**DNS-01 and wildcards.** With `--acme-challenge dns-01` the CA checks a TXT record instead
of connecting to the daemon, so no port needs to be reachable, and wildcard names work.
A `dns-provider` plugin publishes the records: approve `cloudflare-dns`, give it an API
token limited to the zone (Zone > DNS > Edit, and Zone > Zone > Read unless you also set
the zone ID), enable it, and name it:

```sh
apitorrentd --api 0.0.0.0:443   --acme-domain example.com --acme-domain '*.example.com'   --acme-challenge dns-01 --acme-dns-plugin cloudflare-dns
```

After the plugin publishes a record, the daemon waits until every authoritative name server
of the zone serves it before the CA looks (up to `--acme-dns-timeout`, 300 s). Where
outbound DNS is blocked, or for split-horizon DNS, name resolvers to ask instead with
`--acme-dns-resolver`. A wildcard isn't a host name the server answers to: name the hosts
you use under it with `--public-url`.

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

### Downloading files over the web

If a web server already lists the download folder, such as nginx with `autoindex`, the web UI
can link to it. A server admin adds the folder's address in Settings → Files → Storage roots,
next to any `\\nas\…` or `smb://` entries:

```text
https://my-files.server.tld/path/to/downloads/
```

Each finished file then gets a **Download** link in the Files tab, and the save path a
**Browse** link to its folder's listing. The first web address of a folder is the one used.
apitorrent doesn't serve the files itself; the web server does, with its own access rules:

```nginx
location /path/to/downloads/ {
    alias /srv/torrents/;   # the daemon's download folder
    autoindex on;
    auth_basic "Downloads"; # optional; never put the password in the address
    auth_basic_user_file /etc/nginx/downloads.htpasswd;
}
```

Saving the address (or **Check web address**) has the daemon fetch the folder's
`.apitorrent-root` marker through it, to confirm it is the right folder. The check is only
advice: links show either way, since the daemon may not reach an address your browser can, or
be let past a sign-in. nginx's listing hides dotfiles but serves them; a rule that denies
`/\.` paths makes the check fail with HTTP 403.

## Updates

The daemon looks for a new published release once a day and tells admins in the web UI
(a button in the toolbar, and Settings → Updates). Install it from there, or on the
command line:

```sh
apitorrentd update --check   # is there a newer release?
apitorrentd update           # install it; restart the daemon afterwards
```

An update installs only if it is signed with apitorrent's update key, is newer than the
running version, and matches the SHA-256 in the signed manifest. The daemon replaces its
own executable, so it needs write access to the executable's directory; if it was
installed by a package manager, update it that way instead. After an install from the
web UI it restarts by itself: under systemd it exits and lets the unit restart it (use
`Restart=on-failure`), otherwise it starts itself again. Settings → Updates can turn the
daily check off, or install new releases automatically.

Packages that keep apitorrent up to date themselves run it with `--no-update-check` (or
`APITORRENT_NO_UPDATE_CHECK=true`): no daily check, no update endpoints and no update
buttons. `apitorrentd update --check` still says what's new; `apitorrentd update` refuses
to install.

The desktop app checks too, and offers "Install and restart". In "This computer" mode
that also updates the engine running inside it.

Builds without `release/update-signing.pub` (forks, or before the key is made) say when
a new release exists but never install it. See `release/README.md`.

## The desktop app

The desktop app keeps a list of **connections**, each one of two kinds:

- **A server**: a thin client to a daemon (a seedbox, a NAS). The server's certificate is
  trusted in one of three ways, and never skipped:
  - it's from a public CA, or one this computer already trusts;
  - it's from your own CA (import its certificate into the connection);
  - **pairing**: enter the code the server shows (the SHA-256 of its public key), and the
    app pins that key.

  Then **pair this device**: the app shows a short code and opens the server's pairing
  page in your browser, where you're signed in with your passkey; check the code and
  approve. The app is then a device on your Account page, removable on its own, and with
  a pinned key it works only while the server keeps that key. Passkeys can't sign in the
  app's own window (they belong to the server's web address), which is why it pairs. An
  **API token** pasted into the connection still works instead. Either way the credential
  is kept in the system keychain.
- **This computer**: the engine runs inside the app. It can also serve the web UI to your
  network, with the same rules as the daemon (sign-in required; plain HTTP only behind a
  trusted proxy or if you accept the risk).

**Path mappings** let a server connection open downloads in your file manager: map a
folder on the server (`/srv/torrents`) to where this computer sees it (`\\nas\torrents`,
`/mnt/torrents`). A mapping is used only after the app reads the server's
`.apitorrent-root` marker through it, so "Show in folder" never opens the wrong place.
"Suggest from the server" fills them in from the **share locators** a server admin sets in
Settings → Files → Storage roots (`\\nas\torrents`, `smb://nas/torrents`,
`nfs://nas/srv/torrents`): on Windows the share is opened by its UNC path, and on macOS and
Linux the app finds where it is mounted. Folders on this computer (mappings, and the
folders of "This computer" connections) can be picked with the system's folder picker.

Closing the window keeps apitorrent in the tray (downloads go on, and a notification says
when one finishes); quit from the tray menu. Clicking a `magnet:` link or opening a
`.torrent` file brings up the Add dialog; installers register both.

## Licence

MIT. See [LICENSE](LICENSE).

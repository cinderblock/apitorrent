# Deluge import: show every option that comes over, and let the user drop or convert it

## Goal

Asked 2026-11-20: during an import from Deluge, the user should see every option the import
carries over, with a way to leave some out or convert them, so it is clear what is coming
over. Today the wizard shows a torrent table, one "also use Deluge's limits" box and one
"add paused" box; labels, totals, limits, file priorities, edited trackers, stop-at-ratio
and the client-identity rules all come over silently, and what can't come over is only in
the CLI report.

## Environment / context

- Repo `C:\Users\camer\git\apitorrent`, `master`. Builds on `plans/deluge-coexistence.md`
  (step 5 wizard, step 7 identity rules).
- Server: `crates/apitorrent-core/src/deluge_import.rs` (`plan`, `report`, `run_offline`),
  `Core::preview_deluge` / `Core::import_deluge` in `crates/apitorrent-core/src/lib.rs`.
  Types in `crates/apitorrent-types/src/lib.rs` (ts-rs → `web/src/api/generated`).
  CLI `crates/apitorrentd/src/import.rs`.
- Web: `web/src/components/ImportDeluge.tsx` (wizard), `delugeImport.ts` (tested logic).

## Decisions (made here; the user can overrule)

- **One options model, `DelugeImportOptions`, for API and CLI**, on `ImportDelugeRequest`
  with `serde(default)`; its default is today's behaviour, so old callers don't change.
- Per-torrent options, each on by default: labels (keep, rename or drop each one), queue
  order, added dates, totals and seeding time, skipped files, Deluge's edited tracker
  lists, per-torrent speed/connection limits, per-torrent stop-at-ratio.
- Client identity for private trackers: Deluge (default, versions from the daemon),
  apitorrent (add no rules), qBittorrent or Transmission; hosts can be unticked one by one.
- Deluge's global settings: one tick per setting (download rate, upload rate, connections,
  active slots, slow torrents, stop at ratio), replacing `importSettings: bool`. CLI
  `--settings` still takes them all; `--setting NAME` takes one.
- Always carried, shown but not optional: files where Deluge left them, progress, renamed
  files (they are how the files are found).
- What can't come over (move completed, sequential, super-seeding, upload slots, label
  options…) is listed in the wizard, grouped with counts.
- Counts in the wizard follow the ticked torrents.

## Plan / steps

1. [x] Types: `DelugeImportOptions`, `DelugeSettingsPick`, `ImportIdentity`,
       `LabelRename`; preview additions (per torrent: private, limits, stop-at-ratio,
       skipped/renamed files, totals, seeding time, edited trackers, not carried; preview:
       Deluge settings, label options, Deluge identity, hosts already with a rule).
2. [x] Core: apply options in `plan`, settings, identity; `report` gets an OPTIONS section.
3. [x] CLI flags.
4. [x] Web: "What comes over" section in the wizard; logic + tests in `delugeImport.ts`.
5. [x] Tests, clippy, typecheck; docs (architecture Deluge section, README if it lists flags).
6. [ ] ← Click through the wizard against a real Deluge dir (with the rest of
       `plans/deluge-coexistence.md` step 11, on pokol).

### Round 2: what still can't come over (asked 2026-11-22)

The user went through the "can't come over" list. Status of each:

- Move completed and sequential download: already come over (`dc20978`, another thread).
- Per-torrent upload slots: **add** (engine has only a session-wide `upload_slots`, 8).
- First and last pieces first: **add** (to the picker, next to file order).
- Super-seeding (BEP 16): explained, awaiting a yes/no.
- Ports: keep apitorrent's own; nothing to do.
- Encryption: **import** Deluge's `enc_in_policy`/`enc_out_policy`/`enc_level` into
  `Settings::encryption`, and highlight Deluge settings that differ from Deluge's defaults.
- Proxy: user wants it offered; apitorrent has no peer/tracker proxy yet, so it needs the
  feature first. Awaiting go-ahead on scope.
- DHT, LSD and UPnP: DHT and LSD are daemon flags (`--no-dht`, `--no-lsd`), not settings;
  UPnP/NAT-PMP don't exist. Importing them needs them as settings first.
- Active downloads/seeds limits: already come over (the "active slots" setting); the
  earlier reply listing them as not read was wrong.
- Cache: Deluge's `cache_size`/`cache_expiry` are global (not per torrent, as said
  earlier); apitorrent has no disk-cache knob to map them onto.
- Plugins (AutoAdd, Scheduler, Execute, Blocklist, Notifications): out of scope.

User's answers (2026-11-22): super-seeding yes; proxy yes, in its own thread
([Proxy support, imported from Deluge](t3-thread://v2/ab3781d7-a1dd-4bbb-b3e9-6331e19b7569/mcp:launch:0ac27f799bcaee8be66e0299e8379bf8d1a8a0c5a806ebc493c7f4bef767ae9c),
branch `proxy-support`); DHT, LSD and UPnP/NAT-PMP: support them as settings and import
them, even though pokol doesn't need them.

Round 2 steps:

7. [x] Encryption import + "changed in Deluge" marks (`ca6ba0a`).
8. [x] Engine: per-torrent upload slots, first and last pieces first, super-seeding
       (libtorrent's non-strict mode, 2 offers per peer); core/API
       (`PUT /torrents/{id}/first-last`, `/super-seed`, `upload_slots` column, migration
       18); detail pane switches; import of all three (`--no-first-last`,
       `--no-super-seed`).
9. [x] DHT, LSD, UPnP and NAT-PMP as settings (`Settings.discovery`, applied at start by
       `ports::apply_discovery`; `--no-dht`/`--no-lsd` still win); new crate
       `apitorrent-portmap` (NAT-PMP per RFC 6886 first, then UPnP via `igd-next`; one-hour
       leases renewed at half; removed at shutdown); status bar shows the forward; Deluge's
       `dht`/`lsd`/`upnp`/`natpmp` import as one setting (`--setting discovery`).
10. [x] Proxy: built in the other thread (`plans/proxy-support.md`), cherry-picked onto
        master here. Force proxy now also keeps UPnP and NAT-PMP off (at start, and the
        forwards are removed when it is turned on while running); the discovery settings
        only ever turn DHT/LSD off, so they can't bring them back while forced.

## Findings / gotchas

- Bringing `proxy-support` over: conflicts only where round-2 fields sat next to the
  proxy ones (`DelugeSettings`, `DelugeSettingsPick`/`View`, `settingRows`), plus
  reachability's `Dialer.reach` and `route_incoming` argument next to the proxy's.

- The preview asks Deluge's daemon for its versions (for the identity choice's label),
  capped at 3 s so a hung daemon doesn't stall the preview; the import asks again.
- `importSettings: bool` is gone from `ImportDelugeRequest`, replaced by
  `options.settings` (an old client sending it gets no settings taken).
- Super-seeding stalled a swarm in the loopback test: apitorrent (like many clients)
  leaves out `have`s for pieces it thinks the peer already has, so the super-seed never
  heard an offered piece arrived. Fixed as libtorrent does: NOT_INTERESTED from a
  super-seeded peer means it has what it was shown, and it is shown more; a `shown` set
  keeps any piece from being offered twice.
- This machine's build budget was held for 30+ minutes by another thread's desktop build;
  Linux checks ran on sentinel (`rbuild`), Windows-only ones (desktop, portmap) locally.
- Deluge copies its global stop-at-ratio into every torrent; `plan` gives a torrent its
  own seed limits only when it differs (or always, in a non-default space taking the
  setting, since spaces have no seed-limit setting).

## Progress log

- [x] 2026-11-20: plan written.
- [x] 2026-11-22: round 2 steps 7–9 built; clippy, every Rust test and the web checks
      pass on sentinel.
- [x] 2026-11-20: built. Rust unit tests (`deluge_import`: defaults, every option off,
      label renames, settings one by one), offline and lifecycle Deluge tests, clippy,
      web typecheck and `delugeImport.test.ts` pass. The options panel was rendered to
      static HTML with sample data and looked at; not yet clicked through in the running
      app.

## Open questions for the user

(none yet)

## Things not to do

- Don't make renamed files optional: dropping them would point torrents at missing files.

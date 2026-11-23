# Issues

Problems found while working on something else. Each entry is enough to start a fix.

## The peer port listens on IPv4 only by default

Found 2026-11-22. `apitorrentd --peer-listen` defaults to `0.0.0.0:0`
(`crates/apitorrentd/src/main.rs`), so TCP and uTP take connections over IPv4 only, while
the DHT binds both families. Reproduce: start the daemon with defaults and list its
listening sockets (`ss -ltnu` / `netstat -ano`): no IPv6 entry on the peer port. Why it
matters: IPv6 peers can't connect in, which hurts most where IPv4 sits behind a
carrier-grade NAT and IPv6 is the only way in; the status bar's reachability shows no IPv6
TCP or uTP line.

## qBittorrent plugin: recheck does nothing, no set location

Found 2026-11-20. `plugins/qbittorrent-api/src/index.ts` answers `torrents/recheck` with
`ok()` and has no `torrents/setLocation`; the README says recheck "succeeds without doing
anything". The daemon now has both (`POST /torrents/{id}/recheck`, and `PATCH /torrents`
with `savePath`), but the plugin API (`apitorrent-plugin-api`, `torrents:control`) has no
way to call them. Why it matters: Sonarr/Radarr and other qBittorrent clients can't
recheck or relocate. To start: expose recheck and move in the plugin API, then wire the
two endpoints and update the README's list of differences.

## Web: native `title=` tooltips left over

Found 2026-11-20. The browser's own tooltips are slow, look out of place and never show on
touch screens. Still there: `web/src/components/DetailPane.tsx` (the pop-out and close
buttons). Replace them with `components/Tip.tsx` or visible text.

## Fuzz target `pickle` runs out of memory

Found 2026-11-22. CI run https://github.com/cinderblock/apitorrent/actions/runs/45102024794
(`2925b9a`): `libFuzzer: out-of-memory (used: 2068Mb; limit: 2048Mb)` after about 40 s of
the `pickle` target; the input was saved as `oom-19597dc2a899f6249eb65d0cf58bc6d15d0d8161`
(only on the runner). Earlier runs passed. Reproduce: `cargo +nightly fuzz run pickle --
-max_total_time=600 -rss_limit_mb=2048` in `fuzz/`. Why it matters: a crafted Deluge state
file can make the import use gigabytes, and the Fuzz CI job goes red.

## Desktop app: a release build without `tauri/custom-protocol` is a black window

Found 2026-11-14 1:30 PM PST (21:30Z). `cargo build --release -p apitorrent-desktop`
without `--features tauri/custom-protocol` builds an app whose window loads Vite's
`http://localhost:5173`; release builds' `on_navigation` (`is_app_url`,
`apps/desktop/src-tauri/src/lib.rs`) refuses it, so the window stays black
(`ERR_ABORTED` in WebView2's `Breadcrumbs`). With this machine's shared cargo
`build-dir`, such a build also replaces, through hard links, an exe another `--target-dir`
built with the feature. Why it matters: easy to ship or launch a dead app. To start:
an app feature that is on by default for release (Tauri's `custom-protocol` pattern), or a
build-time error when a release build has no frontend.

## Desktop app: API requests to a remote daemon hung once, then never again

Seen in the first GUI run of the desktop app (2026-11-09, about 11:48 AM–11:51 AM PST
(19:48–19:51Z)), Remote profile against `apitorrentd --tls-self-signed` on
`127.0.0.1:8798`. After one successful add, every `api_request` (GETs too) stayed
pending; the UI showed "Adding…" for minutes. The sync socket kept working (a torrent
added with curl appeared), and the daemon answered curl. `netstat` showed one connection
between the two in `FIN_WAIT_1` on each side, as if one end had stopped reading. A
restart of both cleared it; idle gaps of up to 130 s, POST-then-GET, deep links and
disconnect/connect didn't bring it back.

Why it matters: the app looks frozen. `apitorrent-client`'s `REQUEST_TIMEOUT` is 300 s,
so a stuck request sits for five minutes before any error. To start: run the GUI with
`APITORRENT_WEBVIEW_DEBUG_PORT=9333` (see `apps/desktop/src-tauri/src/lib.rs`) and
`RUST_LOG=reqwest=debug,hyper_util=debug`, and probe `api_request` in a loop; consider a
shorter timeout for UI calls either way.

## Space storage roots aren't storage roots for the desktop app

Found 2026-11-10 while adding share locators (branch `t3/365da07f`). `GET /api/v1/roots`
lists only the instance's download directory (`apitorrent-core/src/roots.rs`, built once in
`Core::start`). A space's own `settings.storage_roots` (`apitorrent-types/src/spaces.rs`)
get no `.apitorrent-root` marker, no root id and no share locators, so the desktop app
can't verify a path mapping for a torrent saved there and "Show in folder" stays
unavailable for it. Why it matters: with spaces that keep their downloads elsewhere, the
thin client can't open their files. A fix makes every space storage root a root (id and
marker when the space settings change, listed per the request's space).

## Engine loopback tests time out under heavy load

Seen 2026-11-10 about 5:40 PM PST (01:40Z) while stress-testing the link tests: six
copies of `cargo test -p apitorrent-engine --test loopback` at once, at idle priority, on a
machine already at 100% CPU. In 18 suite runs, 6 failed: `hybrid_transfer` five times
("seeder check timed out", `tests/loopback.rs:92`, a 30 s limit on hashing the seed),
`v2_magnet_chunked_layers` once ("v2 seeder check timed out", line 1081, still
`Checking` with 0 of 640 pieces) and `disk_error_stops_the_torrent` once ("torrent never
reported the disk error", line 302).

Why it matters: the load was extreme, but CI runners are slow too, and wall-clock limits on
CPU-bound work (debug-build hashing) are the likely next flakes. To start: reproduce by
running several suites at once at idle priority, then decide whether those limits should
scale or wait on progress instead of time.

## Magnet metadata fetch leaves a ghost peer on trackers

`crates/apitorrent-engine/src/metadata.rs` `announce()` sends `started` under the fetch's
own identity (a fresh peer id when a client profile applies), repeats `started` every
announce, and never sends `stopped`. Once the metadata arrives, `torrent.rs` resolves a new
identity and starts again under another peer id. A private tracker sees two peers from one
client, the first left to time out. No stats are reported by it (always 0/0), so ratios
are safe, but it breaks "one client, one session".

## Hybrid torrents report the same bytes under both info hashes

`crates/apitorrent-engine/src/announce.rs`: a v1+v2 hybrid announces in two lanes (v1 hash
and truncated v2 hash), and both lanes report the entry's whole uploaded/downloaded. A
tracker that tracks both hashes and credits them to one account counts every byte twice.
Most private trackers register only the v1 hash, so the v2 announce fails there, but this
should be checked before it matters: ratios must never be double-counted.

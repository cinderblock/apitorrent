# Proxy support

## Goal

Peer, web seed and tracker connections through a SOCKS4, SOCKS5 or HTTP (CONNECT) proxy,
with Deluge's options (hostname, port, username, password, resolve names through the
proxy, proxy peers, proxy trackers, force proxy). Then bring Deluge's `proxy` section of
`core.conf` over in the Deluge import (wizard tick box, `apply_instance_settings`, CLI
`--setting proxy`). I2P is out of scope.

Asked by the parent thread on master (Deluge import round 2, see
`plans/deluge-import-options.md`). Branch `proxy-support`; commit at logical steps; don't
push, don't merge.

## Environment

- Worktree `C:\Users\camer\.t3\worktrees\apitorrent\proxy-support-2a7bf6d1263beb64`,
  branched from `9c58b0d` (master has moved on since).
- The parent thread is changing `Settings`, `DelugeSettings`, `DelugeSettingsPick` and the
  wizard at the same time (upload slots, first/last pieces, super-seeding, encryption
  import, DHT/LSD/UPnP/NAT-PMP settings, non-default marks). Changes here are additive:
  new fields at the ends of structs, new rows at the end of `settingRows`.

## Decisions

- New crate `apitorrent-proxy`: the seam. `Proxy` (kind, host, port, credentials,
  remote DNS) with `connect(target)` for TCP, and a SOCKS5 UDP association for UDP
  trackers. SOCKS4/4a and SOCKS5 through `tokio-socks`; HTTP CONNECT and the SOCKS5 UDP
  association are small tested implementations in the crate (`tokio-socks` has no UDP).
- Trackers over HTTP(S): reqwest's own proxy support (`socks` feature), rebuilt when the
  setting changes. UDP trackers: SOCKS5 UDP ASSOCIATE (fresh connection id per request,
  since a remotely resolved name has no address to cache under). Through SOCKS4 or HTTP,
  UDP trackers fail with `UdpNotProxied` while tracker connections are proxied.
- Peers: proxied peer connections are TCP only (no uTP). Web seeds count as peers.
- Force proxy: peers and trackers always go through the proxy; listener closed, DHT shut
  down, LSD stopped, incoming uTP refused, at once. Turning it off brings them back on the
  next start; a daemon starting with it stored starts without DHT, LSD and uTP.
- Force proxy with type "none" does nothing (as in Deluge).
- Password: write-only like plugin secrets. `GET /settings` uses `Settings::redacted()`
  (`passwordSet`); `Core::set_settings` keeps the stored password when `password` is
  absent and removes it on `""`. Stored in the settings row, like plugin secrets in theirs.
- Web: a Proxy card in the existing Network section (no new page).
- Deluge: proxy row only for imports into `default` (it is the instance's, like "slow
  torrents"). I2P, no host/port, and `anonymous_mode` go to "Can't come over"
  (`label_options`, whose doc now says it holds Deluge settings too). A Deluge proxy the
  settings would refuse is skipped with a warning rather than failing the import.

## Steps

1. [x] `apitorrent-proxy` crate + unit tests (`cc7d392`).
2. [x] Tracker client: proxy for HTTP(S), SOCKS5 UDP for UDP trackers (`47c78fa`).
3. [x] Engine: `Session::set_proxy`, dialer, web seeds, force mode; engine tests through
       local SOCKS5 and HTTP proxies (`51c57f0`).
4. [x] Types + core: `ProxySettings` in `Settings`, validation, redaction, apply
       (`cf8cfaa`).
5. [x] Web settings UI (`47f8e6a`).
6. [x] Deluge: read `proxy`, wizard tick box, apply, CLI `--setting proxy` (`ee3f28b`).
7. [x] README/docs (`ce3cfe4`).
8. [x] Final full-workspace clippy (`--exclude apitorrent-plugins`) and tests: all pass
       but the two Windows-only `discover` tests below.

## Findings / gotchas

- reqwest 0.13's `socks` feature supports `socks4`, `socks4a`, `socks5`, `socks5h`.
- reqwest sends an HTTP proxy's URL user info still percent-encoded (`pa%3Ass` came
  through as a 407); credentials go in with `Proxy::basic_auth` instead.
- reqwest/hyper send `proxy-authorization` in lower case; the test proxy compares header
  names case-insensitively.
- `localhost` resolves to `::1` first on this machine; SOCKS4 can't carry IPv6 targets
  ("Address type not supported"), so tests bind where `localhost` resolves.
- `watch::Receiver::wait_for` returns a guard that isn't `Send`; `session::until_forced`
  drops it before the await ends.
- The uTP socket can't be closed while it runs (it keeps serving open streams), so under
  force proxy incoming uTP is refused stream by stream until the next start.
- The engine test's seeder announce raced the leecher's; with force proxy (no incoming)
  the leecher then never found it. The test waits until the tracker knows the seeder.
- `apitorrent-plugins` fails clippy here (`large_enum_variant` on `ToJs`); master fixed it
  in `7acec1a`, not on this branch's base. Clippy for this branch runs with
  `--exclude apitorrent-plugins`.
- `apitorrent-deluge`'s `discover::tests::config_arguments` and
  `a_daemon_without_config_uses_its_users_home` fail on Windows (Unix paths); unrelated.
- `cargo test -p apitorrent-types` rewrites every generated binding with LF; only the
  files with real changes are committed.

## Progress

- 2026-11-22: steps 1–8 done and committed. Web typecheck and `bun test` (132) pass.

## Open questions

(none)

## Things not to do

- Don't read proxies from the environment (`HTTP_PROXY`); only the explicit setting.
- Don't let any connection fall back to direct when the proxy fails.
- Don't send the proxy password in any API response or preview.

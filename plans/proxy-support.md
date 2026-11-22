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

- Worktree `C:\Users\camer\.t3\worktrees\apitorrent\proxy-support-2a7bf6d1263beb64`.
- The parent thread is changing `Settings`, `DelugeSettings`, `DelugeSettingsPick` and the
  wizard at the same time (upload slots, first/last pieces, super-seeding, encryption
  import, DHT/LSD/UPnP/NAT-PMP settings, non-default marks). Keep changes additive.

## Decisions

- New crate `apitorrent-proxy`: the seam. `Proxy` (kind, host, port, credentials,
  remote DNS) with `connect(target)` for TCP, and a SOCKS5 UDP association for UDP
  trackers. SOCKS4/4a and SOCKS5 through `tokio-socks` (the widely used crate; reqwest's
  SOCKS support is the same protocol in hyper-util). HTTP CONNECT is a small tested
  implementation in the crate (no maintained standalone crate worth a dependency).
- Trackers over HTTP(S): reqwest's own proxy support (`socks` feature), rebuilt when the
  setting changes. UDP trackers: SOCKS5 UDP ASSOCIATE; through SOCKS4 or HTTP proxies,
  which can't carry UDP, UDP trackers are skipped while tracker connections are proxied
  (otherwise they would leak the real address).
- Peers: proxied peer connections are TCP only (no uTP). Web seeds count as peers.
- Force proxy: peers and trackers always go through the proxy; no incoming connections,
  no DHT, no LSD, no uTP. Applies at once (listener closed, DHT shut down, LSD stopped);
  turning it off brings them back on the next restart.
- Password: never sent by `GET /settings` (`passwordSet` says whether one is stored); a
  `PUT` without `password` keeps the stored one, `""` clears it. Same rule as plugin
  secrets (`writeOnly`). Stored in the settings row like plugin secrets are stored in
  theirs.

## Steps

1. [ ] `apitorrent-proxy` crate + unit tests (mock SOCKS4/5/HTTP servers in tests).
2. [ ] Tracker client: proxy for HTTP(S), SOCKS5 UDP for UDP trackers.
3. [ ] Engine: `Session::set_proxy`, dialer, web seeds, force mode; engine test through a
       local SOCKS5 server.
4. [ ] Types + core: `ProxySettings` in `Settings`, validation, redaction, apply.
5. [ ] Web settings UI.
6. [ ] Deluge: read `proxy`, wizard tick box, apply, CLI `--setting proxy`.
7. [ ] README/docs.

## Findings / gotchas

- reqwest 0.13's `socks` feature supports `socks4`, `socks4a`, `socks5`, `socks5h`
  (`reqwest-0.13.5/src/proxy.rs`).
- `strict_http_client_builder` says "don't add a proxy"; the proxy is added in a separate,
  explicit builder path.

## Progress

## Open questions

(none)

## Things not to do

- Don't read proxies from the environment (`HTTP_PROXY`); only the explicit setting.
- Don't let force proxy fall back to a direct connection on proxy failure.

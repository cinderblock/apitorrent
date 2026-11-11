# Issues

Problems found while working on something else. Each entry is enough to start a fix.

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

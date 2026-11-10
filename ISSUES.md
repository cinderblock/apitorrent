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

## Web UI: toolbar labels wrap at 1280 px

In the desktop window (1280×800) the toolbar's "Pause / Resume" button and the profile
button ("Loopback daemon") wrap onto two lines while the search box has room to give.
Cosmetic; the toolbar should shrink the search field first.

## uTP test `transfer_through_a_lossy_reordering_proxy` reset once on Linux CI

Seen once on 2026-11-10 at about 4:30 PM PST (00:30Z), in GitHub Actions run
[42915843916](https://github.com/cinderblock/apitorrent/actions/runs/42915843916), job
"Rust (ubuntu-latest)": `crates/apitorrent-utp/tests/socket.rs:49` unwrapped
`Custom { kind: ConnectionReset, error: Reset }`, so the test's join at line 53 panicked.
The other 10 uTP socket tests passed, and the same commit passed on Windows and macOS.

Why it matters: unlike a timing flake, a reset while data crosses a lossy, reordering
link could be a real protocol bug (a state the socket treats as fatal that it should
recover from). To start: run the test in a loop on Linux with the proxy's seed logged,
and find which side sends the reset and in what state.

## Engine test `default_links_are_encrypted_utp` fails under load

Seen once on 2026-11-09 at about 6:20 PM PST (02:20Z), in a full `cargo test -p
apitorrent-engine` run while other builds were compiling:
`crates/apitorrent-engine/tests/loopback.rs:1430` (`assert_eq!(views, Some(((true, true),
(true, true))))`). It passed alone three times and in two more full runs of the loopback
suite. Most likely the uTP connect timed out under CPU load and the link fell back to TCP.

Why it matters: a flaky test hides real failures in CI. To start: log the link the
leecher ended up with when the assertion fails, and consider waiting for the uTP attempt
(or retrying the link check) instead of taking the first connection.

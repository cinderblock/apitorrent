# Reachability: can peers on the internet connect in?

## Goal

Tell the user whether the peer port (TCP and uTP) and the DHT port are open to the
internet, and what our public address is, without a third-party service:

1. **Passive inference.** An incoming connection from a public address proves a port is
   open. None for a long while after the swarms learned our address suggests it's closed.
2. **External address.** What peers (BEP 10 `yourip`), the DHT (BEP 42 votes), HTTP
   trackers (BEP 24 `external ip`) and the router (NAT-PMP/UPnP) say our public IP is.
   A router whose public IP differs from what the internet sees means another NAT sits in
   front (often the ISP's), so a forward on the router can't open the port.

3. **Check button** (asked 2026-11-22: "build both. a user gets a button 'check my port is
   open with...'"): the daemon asks Transmission's port test or ifconfig.co to connect
   back; the answer feeds the same model.

Later, not now: our own check-back service (#4) as a third backend.

## Decisions already made

- Build #1 and #2 (user, 2026-11-22). #4 (own check-back service: takes only a port,
  connects back to the requesting IP) maybe later.
- Work in worktree `C:\Users\camer\.t3\worktrees\apitorrent\reachability`, branch
  `reachability`; cherry-pick onto master when done (master's tree is shared with other
  threads that have uncommitted generated types).
- UI: extend the status bar's "Peer port" item (the piece that already shows port
  forwarding). No new page.
- Check button: in the status bar beside the peer port, a "Check port with…" menu of the
  two services. The daemon makes the request (from the browser it would test the
  browser's network, and CORS blocks it anyway).

## Design

- `apitorrent-engine/src/reachability.rs`: `Reachability` in `Shared`. Records incoming
  TCP/uTP handshakes by family, uTP dials (to discount NAT pinholes), external-IP reports
  from peers and trackers, and when we first advertised ourselves (a successful announce).
  `Session::reachability()` returns a snapshot with `Instant`s; core turns it into
  `apitorrent_types::Reachability` (unix seconds) on `SessionStats`.
- Proof rules:
  - TCP: a completed BitTorrent handshake on an accepted connection from a public IP. A
    TCP SYN never passes a NAT or stateful firewall without a forward, so this is clean.
  - uTP: the same, but not from an IP we dialed over uTP in the last 20 minutes (our own
    packets open a pinhole for that peer).
  - DHT: a query from an IP we haven't sent anything to in the last 10–20 minutes
    (rotating windows, keyed by IP to cover address-restricted NATs).
- States: `open` (proof seen since the last address change), `waiting`, `probablyClosed`
  (no proof 30 minutes after we were first advertised: a tracker or DHT announce for TCP/uTP,
  a non-empty routing table for the DHT).
- External IP per family: DHT vote, else peers' `yourip` majority (≥ 3 distinct peers),
  else the latest tracker report. A change of it restarts the wait.
- Port check (`apitorrent-engine/src/port_check.rs`): `PortChecker` enum is the seam (URL +
  answer parser per service; #4 would be a third variant). One request per family the TCP
  listener covers, bound to `0.0.0.0` / `::` (hyper-util then dials only that family),
  never through the proxy; refused under force proxy. 30 s cooldown per service. The
  newer of the check and the last incoming connection decides TCP's state: `open` or a
  new `closed`. An address change drops the check like any proof.
  `POST /api/v1/port-check {service}` → per-family `{open, error}`.

## Progress

- [x] Plan
- [x] Tracker: parse `external ip`
- [x] DHT: unsolicited-query counters (simulation test with one node behind a NAT)
- [x] Engine: reachability module, hooks (accept, uTP dial, yourip, announce)
- [x] Types + core + status bar (ⓘ tip with the details)
- [x] Tests (unit: voting, states, self-connections; web helpers)
- [x] ISSUES.md: peer port is IPv4-only by default
- [x] README highlight, architecture paragraph
- [x] clippy, tests (sentinel), web tests and typecheck; commits `050bed7`, `a6f201b`,
  `7a412d0`, fast-forwarded onto master
- [x] Pushed (on origin/master by 2026-11-22)
- [x] Check button: engine `port_check`, `closed` state, core, route, types, status bar
  menu, docs; clippy + unit tests (sentinel), web tests + typecheck; live test of both
  services over IPv4 and IPv6 on Windows (`live_port_check`, ignored by default: port 1
  reads closed from all four); on master
- [x] Local desktop app rebuilt at `5cd9189` and restarted (2026-11-22 8:46 PM PST,
  04:46Z; PID 37324; old exe `.bak-20261122-2047`).
- [x] pokol already has the port check: another thread deployed `0a0cd13` at 8:22 PM PST
  (see `plans/file-order-and-move.md`). The note above that pokol ran `56a4105` was wrong.
- [x] Local desktop app rebuilt at master `80c92ec` and restarted 2026-11-22 10:14 PM PST
  (06:14Z Nov 23; PID 15748; old exe `.bak-20261122-2214`)
- [ ] Seen in a browser: the status bar's peer port should read "open" once a peer
  connects in, and the check button should answer

## Findings

- Public check-back services exist and work over IPv4 and IPv6 (tried 2026-11-22):
  `https://portcheck.transmissionbt.com/<port>` (body `1`/`0`) and
  `https://ifconfig.co/port/<port>` (JSON `reachable`). Both connect back to the
  requester's IP. Built in later as a user-pressed button (the user asked), never automatic.
- `apitorrentd --peer-listen` defaults to `0.0.0.0:0`: TCP and uTP listen on IPv4 only.
  The DHT binds both families. Logged in ISSUES.md.
- The engine has no guard against dialing itself (trackers may list us), so a TCP
  connection whose local IP equals its remote IP, or that comes from our known public IP,
  isn't counted as proof.
- master's `web/src/styles.css` had an unclosed `.jobs-tray-head {` from merge `6c87afc`
  (prettier: `CssSyntaxError: Unclosed block`); fixed in its own commit.
- `cargo fmt --all` reformats `deluge_import.rs` and `apitorrent-deluge/src/lib.rs` on
  master (another thread's); left alone here.

## Things not to do

- Don't count LAN or loopback peers (LSD, tests) as proof.
- Don't call a third-party checker on our own: only when the user presses the button.
- Don't send the check through the proxy: it would test the proxy's address.

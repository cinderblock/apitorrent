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

Later, not now: an active connect-back check (our own service, or a public one) feeding
the same model as another kind of evidence.

## Decisions already made

- Build #1 and #2 (user, 2026-11-22). #4 (own check-back service: takes only a port,
  connects back to the requesting IP) maybe later.
- Work in worktree `C:\Users\camer\.t3\worktrees\apitorrent\reachability`, branch
  `reachability`; cherry-pick onto master when done (master's tree is shared with other
  threads that have uncommitted generated types).
- UI: extend the status bar's "Peer port" item (the piece that already shows port
  forwarding). No new page.

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
- [ ] Push (waiting on the user: the repo's CLAUDE.md says nothing about pushing)
- [ ] See it on pokol after a deploy: the status bar's peer port should read "open" once a
  peer connects in

## Findings

- Public check-back services exist and work over IPv4 and IPv6 (tried 2026-11-22):
  `https://portcheck.transmissionbt.com/<port>` (body `1`/`0`) and
  `https://ifconfig.co/port/<port>` (JSON `reachable`). Both connect back to the
  requester's IP. Not built in: they're someone else's infrastructure.
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
- Don't call a third-party checker from the daemon.

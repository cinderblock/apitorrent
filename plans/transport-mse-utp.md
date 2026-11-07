# Transport crates: `apitorrent-mse` and `apitorrent-utp`

## Goal

Two self-contained transport crates for the engine, built in a background worktree while
the main thread works on the engine (see `plans/apitorrent-bootstrap.md`, "Delegated
streams"). The main thread wires them in; this stream does not touch the engine.

- `apitorrent-mse`: Message Stream Encryption / Protocol Encryption. A sans-IO handshake
  for both sides, plus `MseStream<S>` over any `AsyncRead + AsyncWrite`.
- `apitorrent-utp`: uTP (BEP 29) with LEDBAT. `UtpSocket` / `UtpStream`, with a way to
  share the UDP port with DHT.

## Environment / context

- Worktree `C:\Users\camer\.t3\worktrees\apitorrent\t3-988562c6`, branch
  `t3/mse-and-utp-transport-crates`, based on `518a31b`. Local only: never push.
- Heavy builds: `~/.srm/bin/srmctl run --slots 4 --low --label apitorrent-transport -- cargo …`.
- Only add the two crates and their root `Cargo.toml` entries. Don't edit other crates.

## Decisions already made (don't re-ask)

- **uTP: write our own, don't wrap `librqbit-utp` 0.7.0.** Reasons in the bootstrap plan's
  Findings. Headline: it implements CUBIC, not LEDBAT, and does not retry SYNs.
- **MSE crypto**: RustCrypto `rc4` 0.2 and `sha1` 0.10 (already in the workspace), and
  `crypto-bigint` 0.7 for a constant-time 768-bit modular exponentiation (fixed width, no
  heap). `num-bigint` is only a dev-dependency, as an independent cross-check.
- **MSE policy**: `Disabled`, `Enabled(Preference)`, `Required`. The initiator offers both
  methods under `Enabled` and only RC4 under `Required`. Falling back to plaintext after a
  failed encrypted attempt is the engine's call (`Policy::retry_plaintext`).

## Plan / steps

1. MSE crate: crypto, sans-IO initiator and receiver, `MseStream`, tests, KAT from an
   independent Python reference. Done, `c59737a`.
2. uTP crate: packet codec, sans-IO connection (SACK, LEDBAT, RTO), multiplexer, tokio
   driver, tests with a simulated network and a lossy proxy. Done, `7733245`.
3. Record findings in the bootstrap plan, tick its progress line, commit, report. Done.

Wiring into the engine is the main thread's job. What it needs:

- MSE: `MseStream::connect(tcp, info_hash, &our_handshake, MseConfig::new(policy))` for
  outgoing; `MseStream::accept(tcp, &skey_table, config)` for incoming, then read the
  BitTorrent handshake from the stream as now. Keep an `SkeyTable` (or implement
  `SkeyLookup` over the torrent map). Split with `into_parts` plus `MseReadHalf` and
  `MseWriteHalf` over `TcpStream::into_split`.
- uTP: one `UtpSocket` on the listen port; `connect`/`accept` give `UtpStream`s, which
  split with `into_split`. MSE wraps a `UtpStream` the same way it wraps TCP.
- `docs/architecture.md` still places MSE in `apitorrent-wire` and uTP in the engine; the
  crate table needs the two new crates (left to the main thread, which owns the doc).

## Findings / gotchas

All recorded in the bootstrap plan's Findings: the `librqbit-utp` verdict, the uTP STATE
sequence-number trap, LEDBAT's slow linear decrease, path MTU and the don't-fragment bit,
Windows UDP `ConnectionReset`, port sharing, the MSE crypto choices and the handshake
deadlock.

## Progress log

- [x] Evaluate `librqbit-utp` (rejected)
- [x] MSE crate (`c59737a`)
- [x] uTP crate (`7733245`)
- [x] Bootstrap plan updated

## Things not to do

- Don't edit `crates/apitorrent-engine` or any other existing crate.
- Don't push, don't add a remote.

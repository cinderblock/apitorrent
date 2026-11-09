# Built-in TLS (rustls): bring-your-own certificates, self-signed with pairing, ACME

## Goal

Let the daemon serve HTTPS itself, for direct exposure without a reverse proxy. This
implements "Built-in TLS" in the "Security model / Transport" section of
`docs/architecture.md`, and item 3 of "Client trust" (a pinned key set up by pairing).
Delegated stream from `plans/apitorrent-bootstrap.md`.

## Environment / context

- Worktree `C:\Users\camer\.t3\worktrees\apitorrent\t3-ae0674d1`, branch `t3/ae0674d1`
  (off `0f4d23b`). Local only: never push, never add a remote. The main thread
  cherry-picks onto `master`, where the plugin host and the desktop app are being built at
  the same time, so changes to shared files stay small (`apitorrentd/src/main.rs`,
  `apitorrent-server/src/lib.rs`, the root `Cargo.toml`, `web/src/**`).
- Heavy builds: `~/.srm/bin/srmctl run --slots 4 --low --label apitorrent-tls -- cargo …`.
- Pebble (ACME test server) and `pebble-challtestsrv` (its DNS) are installed in
  `~/go/bin` (from pebble v2.10.x). Both run headless on Windows and bind to 127.0.0.1.

## Decisions already made (don't re-ask)

- **New crate `apitorrent-tls`** holds everything that isn't HTTP: the certificate store
  and SNI resolver, PEM loading, the bring-your-own watcher, self-signed generation, the
  pairing code, and the ACME manager. `apitorrent-server` adds the TLS listener, the
  HTTP-01/redirect listener and the pairing endpoint, in its own module `tls.rs`.
- **ring, not aws-lc-rs.** The workspace already standardised on ring (tracker TLS): it
  builds on Windows MSVC with no CMake or NASM, and the provider is always passed
  explicitly (`builder_with_provider`), so we never depend on or change rustls's
  process-wide default. aws-lc-rs would need CMake (and NASM for speed) on Windows, and a
  second provider in the tree makes `ClientConfig::builder()` panic for anyone who relies
  on the default. `cargo tree -i aws-lc-sys` must stay empty.
- **ACME through `instant-acme` 0.8.5**, with `default-features = false,
  features = ["ring"]` and our own `HttpClient` over the workspace's reqwest (strict
  platform verifier, no proxies, no redirects). Evaluation below.
- **Server API shape**: `ServerConfig` is unchanged (other threads construct it). TLS is a
  separate entry point, `serve_tls(core, config, tls, shutdown)`, so the plain path and
  all existing callers stay as they are.
- **Pairing code**: SHA-256 of the leaf certificate's SubjectPublicKeyInfo (DER). Shown
  in full as lowercase hex, and as a short code: the first 20 characters of its RFC 4648
  base32 encoding (100 bits), in groups of four: `ABCD-EFGH-IJKL-MNOP-QRST`. 100 bits
  makes a key with a matching code a 2^100 second-preimage search. Clients compare
  case-insensitively, ignore `-` and spaces, and may map `0→O`, `1→I`, `8→B`.
- **Self-signed key persists** (`<data>/tls/self-signed/`), so the pin survives restarts
  and certificate renewals. The certificate is reissued with the same key when it nears
  expiry or the host names change.
- **ACME keys rotate** on every issuance (fresh P-256 key per order). Pins on ACME or
  bring-your-own certificates are therefore not stable; clients should trust those
  through the platform verifier. Pairing is for self-signed LAN use.
- **HSTS** (`max-age=31536000`, no `includeSubDomains`, no `preload`) on TLS responses,
  except when the Host is a loopback name or an IP literal: HSTS is host-wide, not
  per-port, so sending it for `localhost` would force every other local dev server to
  HTTPS.
- **Passkey relying parties follow TLS**: each ACME domain, and each non-wildcard DNS
  name in a bring-your-own certificate at startup, becomes an `https://name[:port]`
  public URL unless `--public-url` already names that host. Self-signed adds none,
  because browsers refuse WebAuthn on a page with a certificate error.
- **Never an "ignore certificate errors" option.** A private ACME CA is trusted by giving
  its root explicitly (`--acme-directory-ca`), which is the "private CA" trust model,
  not a bypass.

## Plan / steps

1. Plan doc, crate skeleton.
2. `apitorrent-tls`: store/resolver, PEM loading, cert inspection (x509-parser),
   pairing code, BYO watcher, self-signed, unit tests.
3. ACME: schedule (pure, unit-tested), account and certificate persistence, HTTP-01 and
   TLS-ALPN-01, DNS-01 trait, reqwest `HttpClient`.
4. Server: TLS listener (handshakes off the accept loop), h2 + http/1.1 ALPN, WebSocket
   over h1 and h2 (RFC 8441), HSTS, secure cookies, listen rules, pairing endpoint,
   HTTP-01/redirect listener.
5. Daemon flags.
6. Tests: BYO served then reloaded; WS over TLS; ACME against Pebble.
7. Docs: architecture transport section, README usage, bootstrap plan line.

All steps are done. See "Follow-ups" for what's left.

## instant-acme evaluation

`instant-acme` 0.8.5 (maintained by djc, a rustls and quinn maintainer; Apache-2.0).
I read its source (`account.rs`, `order.rs`, `types.rs`, about 3,000 lines) before
adopting it.

- **For:** full RFC 8555 client (accounts, orders, authorizations, all challenge types,
  `finalize_csr`, polling with `Retry-After`, ARI, key rollover, contact update,
  revocation, EAB). Serializable `AccountCredentials`. `ring` feature, so no aws-lc-rs. A
  pluggable `HttpClient` trait, so our strict reqwest does the HTTPS and no second HTTP
  stack or default-provider dependency comes in (`default-features = false`). Small
  dependency set: base64, http, httpdate, serde, thiserror, async-trait, tokio (time).
- **Against / gotchas:** `AccountCredentials` isn't `Clone` (we re-parse it). Its
  `Order::finalize()` needs the `rcgen` feature and makes its own key; we use
  `finalize_csr` with our own rcgen CSR instead. `RetryPolicy` defaults to a 30 s timeout,
  too short for real CAs under load; we pass our own (1 s, x1.5, 3 min).
- Writing our own was not worth it: JWS signing, nonce handling and badNonce retries are
  exactly where hand-rolled clients go wrong, and the crate is small enough to audit.
- `cargo tree -i aws-lc-sys` and `-i openssl-sys`: no matches.

## Findings / gotchas

- axum's `ConnectInfo<SocketAddr>` is only implemented for `TcpListener` and for
  `TapIo<L, _>` (with `L::Addr`). Orphan rules forbid implementing it for our own
  listener, so the TLS listener is wrapped in `.tap_io(|_| {})` to pick up the blanket
  impl.
- axum's `serve` enables HTTP/2 extended CONNECT (RFC 8441), so a browser may open the
  sync WebSocket as `CONNECT` over h2. The route must accept `CONNECT`, and the auth and
  CSRF checks must treat it as a WebSocket (there's no `Upgrade` header in h2).
- axum's `http2` feature isn't on by default; the server crate turns it on.
- rustls's `CertifiedKey::keys_match` can't check a TLS-ALPN-01 challenge certificate:
  webpki rejects its unknown critical `acmeIdentifier` extension
  (`InvalidCertificate(Other(UnsupportedCriticalExtension))`). The challenge cert is
  built with `CertifiedKey::new` directly.
- rcgen's default validity runs to the year 4096; tests that check expiry must set
  `not_after`.
- HSTS is host-wide, not per port: sending it for `localhost` would push every local dev
  server to HTTPS in Chrome. It is skipped for loopback names and IP literals.
- ACME must start only after the listeners are bound (`AcmeManager::new`, then `start`
  after `TlsServer::bind`), or the first TLS-ALPN-01 or HTTP-01 validation fails and backs
  off for 5 minutes.
- Pebble and the test DNS server are spawned with `CREATE_NO_WINDOW` on Windows, so no
  console window appears.

## Progress log

- [x] 1 plan and skeleton
- [x] 2 `apitorrent-tls` core: store and SNI resolver, PEM loading with key-match check,
  `CertInfo` (x509-parser), pairing code, BYO watcher, self-signed with a persistent key
- [x] 3 ACME: `AcmeManager` (validate, load stored, start after bind), account and
  certificate storage, HTTP-01, TLS-ALPN-01, DNS-01 trait, reqwest `HttpClient`,
  schedule and backoff (unit tests)
- [x] 4 server: `TlsServer`/`serve_tls`, handshakes off the accept loop, h2 + http/1.1,
  WebSocket over h2 extended CONNECT, HSTS, secure cookies, listen rules,
  `GET /api/v1/tls`, HTTP-01/redirect listener
- [x] 5 daemon flags (`crates/apitorrentd/src/tls.rs`)
- [x] 6 tests: `tests/tls.rs` (BYO served, reloaded, broken and mismatched replacements
  refused, untrusted client refused, HSTS, Secure cookie, redirects, WS over h1 and h2,
  self-signed pinning); `tests/acme_pebble.rs` against a real local Pebble: both
  challenges issue, storage reused, renewal scheduled at 2/3. **Passed on 2026-11-08**
  (the test is `#[ignore]`d by default because it needs Pebble installed; run it with
  `--ignored`)
- [x] 7 docs: architecture "Transport" and "Client trust", README "Built-in HTTPS"

## Follow-ups (not in this task's scope)

- A `dns-provider` plugin host binding for `DnsProvider` (the plugin host is being built
  in parallel), then a `--acme-challenge dns-01` flag.
- ACME Renewal Information (ARI, RFC 9773): instant-acme supports it behind its `time`
  feature; we renew at 2/3 of the lifetime instead.
- Web UI: show the pairing code and ACME status from `GET /api/v1/tls` on the settings
  page. The TypeScript types are generated (`TlsStatus`, …); no UI was added.
- Running the Pebble test in CI (needs Go or the Pebble release binaries).
- OCSP stapling (Let's Encrypt has ended OCSP, so low value).

## Open questions for the user

*(none yet)*

## Things not to do

- Don't contact real ACME servers (Let's Encrypt staging included): they need a public
  domain and DNS. Pebble only.
- Don't add aws-lc-rs or OpenSSL.
- Don't add an "ignore certificate errors" option, or `danger_*` verifiers.
- Don't bind tests to anything but 127.0.0.1 (Windows Firewall prompts).

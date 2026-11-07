# M4: authentication

## Goal

Real authentication for the API and web UI, so the daemon may listen on non-loopback
addresses. Implements the "Authentication" section of `docs/architecture.md` (passkeys
first, optional passwords, bootstrap token, sessions, scoped API tokens, rate limits).
Delegated stream from `plans/apitorrent-bootstrap.md`.

## Environment / context

- Worktree `C:\Users\camer\.t3\worktrees\apitorrent\t3-4e40f8c3`, branch `t3/4e40f8c3`
  (off `518a31b`). Local only: never push, never add a remote.
- The main thread is changing `apitorrent-core` in parallel: don't touch core's database or
  migrations. Auth has its own SQLite file, `auth.db`, in the data directory.
- Heavy builds: `~/.srm/bin/srmctl run --slots 4 --low --label apitorrent-auth -- cargo …`.
- ts-rs exports to `web/src/api/generated` (see `.cargo/config.toml`).

## Decisions already made (don't re-ask)

- **webauthn-rs 0.6.2**, `conditional-ui` feature for discoverable login. 0.6 moved to
  RustCrypto (`crypto-glue`), so no OpenSSL enters the tree (checked with `cargo tree`).
- **Passkeys are discoverable** (resident key *required* at registration). Login is
  usernameless, so the login endpoints never reveal which usernames exist. Re-auth for
  sensitive actions uses the current user's credential list.
- **One relying party per configured origin**: `localhost` (any port, http or https) plus
  one per `--public-url`. The request's `Host` picks the RP. A passkey stores its RP ID and
  only works on that host. `127.0.0.1` and other hosts without an RP get password/token
  auth only (an IP address can't be an RP ID).
- **Ceremony state stays in server memory** (keyed by a random ceremony id, 5-minute
  expiry, capped count). No `danger-allow-state-serialisation`.
- **Passwords**: argon2id (m=64 MiB, t=3, p=1), PHC strings, 12–1024 chars, at most 2
  concurrent hashes. Instance-wide mode `off` (default) / `fallback` (password alone signs
  in) / `second-factor` (users who set one must enter it after their passkey).
- **Sessions**: 256-bit random id, SHA-256 at rest, idle 7 d / absolute 30 d defaults,
  new id on every login, re-auth and second-factor completion. `last_seen` written at most
  once a minute.
- **Cookie naming**: `__Host-apitorrent-session` (Secure) when the request is HTTPS (a
  trusted proxy says `X-Forwarded-Proto: https`). Plain HTTP uses `apitorrent-session`
  without Secure, because Safari refuses Secure cookies on `http://localhost` and the
  prefix adds nothing on loopback. Each mode reads only its own cookie name.
- **Recent-auth ("sudo") window**: 10 minutes after a login or re-auth for adding/removing
  passkeys, setting a password, and creating API tokens.
- **API tokens**: `apt_` + 256-bit base64url, SHA-256 at rest, scopes `read` / `full`,
  optional expiry. Tokens never manage credentials (account endpoints need a cookie session).
- **Setup token**: 160-bit, printed to stderr with a `/setup#token=…` URL (fragment, so it
  never reaches access logs), 1-hour lifetime, rotated and re-printed when it expires while
  no user exists, invalidated after 10 wrong guesses. `--write-setup-token` also writes it
  to `<data>/setup-token` (0600 on Unix).
- **CSRF**: cookie and anonymous requests keep the Origin and Sec-Fetch-Site checks,
  tightened: Origin must equal the request's own origin (port included), and
  `Sec-Fetch-Site: same-site` is refused too (other localhost ports are same-site).
  Requests with `Authorization: Bearer` skip CSRF; a bad bearer is a 401, never a cookie
  fallback.
- **Non-loopback listen** requires auth enabled and either `--trusted-proxy` or
  `--allow-insecure-lan`; plain HTTP on non-loopback logs a loud warning.
- **`--insecure-no-auth-loopback`**: dev only, refused with a non-loopback listen, and only
  loopback peers with loopback Host headers get the synthetic local admin.

## Plan / steps

All steps are done. Remaining ideas are under "Follow-ups".

1. `apitorrent-auth` crate: store + migrations, clock, secrets, users, passkeys, passwords,
   sessions, tokens, setup token, rate limiter, ceremonies, audit events.
2. Auth DTOs in `apitorrent-types` (ts-rs).
3. Server: client-IP/trusted proxies, auth middleware, cookie handling, auth routes, WS
   auth with periodic re-validation, guard tightening, serve() rules.
4. Daemon flags; print setup token.
5. Rust tests: soft authenticator, full flow, expiry, scopes, guards, lockout.
6. Web UI: status gate, setup page, login page, account page; bun tests.
7. Docs: architecture "Authentication", README bootstrap, bootstrap plan M4 line.

## Findings / gotchas

- webauthn-rs `start_passkey_registration` sets `require_resident_key(false)`; we patch
  the returned options to `residentKey: required` so login can be usernameless.
- webauthn-rs `start_discoverable_authentication` forces `mediation: conditional`; the
  button flow sends only the inner `publicKey` options, which drops it.
- webauthn-rs 0.6's JSON shapes match `@simplewebauthn/browser` 14 directly (it accepts
  `clientExtensionResults`); checked in headless Chrome with a CDP virtual authenticator.
- There is no OpenSSL-free software authenticator crate for tests
  (`webauthn-authenticator-rs` needs OpenSSL), so `crates/apitorrent-server/tests/common/
  authenticator.rs` is a small one: ES256 via `p256` 0.14 (already in the tree), `none`
  attestation, hand-rolled CBOR. Negative tests (no UV, replay, wrong origin, forged
  signature) prove the server really verifies.
- Argon2 at 64 MiB is slow unoptimised, so `[profile.dev.package.argon2]` (and `blake2`)
  build at `opt-level = 3`. Tests use a cheap `PasswordCost`.
- The bash tool's wrapper choked on some quoted heredocs here; patch scripts went through
  files in the scratchpad instead.
- Binding `0.0.0.0` in tests would raise a Windows Firewall prompt, so the listen-rule
  tests only cover the refusals (which return before binding).
- Vite's dev proxy keeps `Host: localhost:5173`, so dev sign-in works unchanged: Origin
  matches Host, and the `localhost` RP allows any port.

## Endpoints (all under `/api/v1/auth`)

- `GET status`
- `POST setup/start`, `setup/finish`, `setup/password`
- `POST login/passkey/start`, `login/passkey/finish`, `login/password` (also the second
  factor), `logout`
- `POST reauth/passkey/start`, `reauth/passkey/finish`, `reauth/password`
- `GET passkeys`; `POST passkeys/start`, `passkeys/finish`; `PATCH|DELETE passkeys/{id}`
- `PUT|DELETE password`
- `GET|POST tokens`; `DELETE tokens/{id}`
- `GET sessions`; `DELETE sessions/{id}`

## Follow-ups (not in M4's scope)

- Inviting more users (admin creates a user plus a one-time enrollment link), and role
  management. Roles are stored and viewers are read-only already.
- Finer token scopes (`torrents:write`, `admin`) and IP-range binding.
- Conditional-UI (autofill) passkey sign-in.
- An audit-log viewer (events are recorded in `auth_events`).
- Device pairing for the desktop thin client.

## Progress log

- [x] 1 auth crate (`49e00e3`)
- [x] 2 types (`49e00e3`)
- [x] 3 server: middleware, routes, cookies, proxies, guard, WS re-checks (`269cde6`)
- [x] 4 daemon flags and setup banner (`269cde6`)
- [x] 5 Rust tests: 18 auth unit tests, 11 server integration tests over HTTP with a
  software authenticator (`269cde6`)
- [x] 6 web UI and 6 bun tests; headless Chrome check with a virtual authenticator
  (`8bc15b7`)
- [x] 7 docs: architecture "Authentication", README "Signing in", bootstrap plan

## Open questions for the user

*(none yet)*

## Things not to do

- Don't touch `apitorrent-core`'s database or migrations.
- Don't add OpenSSL (webauthn-rs 0.5 and `webauthn-authenticator-rs` pull it in).
- Don't put tokens in URLs (query strings land in logs).

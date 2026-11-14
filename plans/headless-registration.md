# Headless setup and registration: no web UI needed

## Goal

Set up a headless daemon and connect the desktop app to it remotely without ever opening
the web UI: the daemon prints a short-lived registration token on its console, and the
app uses it. Asked by the user 2026-11-13 ("Do not require the web UI to set up the backend
and connect a head remotely ... temporary registration tokens that are generated randomly
at startup and expire after ~10 minutes").

## Decisions already made (don't re-ask)

- Tokens are random, printed on the console (journal) at startup, valid ~10 minutes.
- The desktop app ("head") registers with one; no browser, no passkey needed to start.

## Design

- **Reuse device pairing** (`apitorrent_auth::devices`): `POST /auth/devices/requests`
  gains an optional `registration` (token, plus a username when the server has no users).
  A valid token approves the request on the spot; the app's first poll gets the device
  credential, exactly as after browser approval. Key pinning and binding are unchanged.
- **No users yet:** the token is the setup token, now valid 10 minutes (was an hour),
  printed at startup and again whenever it expires, until the first admin exists. It works
  in the app (makes the first admin, with no passkey or password, plus this device) or on
  the web `/setup` page as before.
- **Users exist:** `apitorrentd register-device [--user NAME]`, run on the server, prints a
  one-use token for that user's next device, valid 10 minutes, stored hashed in `auth.db`.
  Not printed at every startup: the journal is readable by the `adm` group, and a token
  there would give any such user an admin's device.
- Remote use needs TLS: `--tls-self-signed` (the app pins the key by the printed pairing
  code), a certificate, or an SSH tunnel to `http://localhost`.

## Plan / steps

1. [x] Auth: registration on device requests; setup token TTL 10 min; registration-token
   table and `create_registration_token`. Tests.
2. [x] Types, server route, error mapping; server tests.
3. [x] Daemon: console text; `register-device` subcommand.
4. [x] Desktop app: token (and username) fields in the server connection's pairing.
5. [x] Docs: README "Signing in", architecture security model.
6. [x] On pokol: the user chose a real name over tunnels and `--remote`
   (2026-11-14). DNS `apitorrent.isozilla.com` CNAME to pokol, not proxied: ops
   `6cf8da78`, applied by CI. The user sets up nginx and the certificate by hand;
   apitorrentd then needs `APITORRENT_PUBLIC_URLS=https://apitorrent.isozilla.com` and
   `APITORRENT_TRUSTED_PROXIES=127.0.0.1`. Then the app registers at that address with a
   `register-device` token (user `cameron` already exists, made in the browser
   2026-11-13 7:59 PM PST).

7. [x] **Browser sign-in from the app** (asked 2026-11-14): a device with the
   new `account` permission asks for a fresh random one-use link (a third enrollment kind,
   `browser`, ten minutes; never the registration token). The app opens
   `<server>/enroll#token=…`; the page makes the user create a passkey for that address,
   and only then signs the browser in. Nothing else about the account changes.
   - `account` on devices (auth migration 6): console-registered devices get it; a
     browser approval may grant it (off by default). It lets a full-scope device make
     browser sign-in links and API tokens without a fresh browser sign-in.
   - Migration 6 also rebuilds `enrollments` (its kind CHECK) together with
     `enrollment_spaces`: dropping `enrollments` with foreign keys on would cascade-delete
     the spaces of pending invitations.

## Findings / gotchas

- A paired device was bound to one space (like an API token), so the app couldn't open a
  second space, which the two-Deluge setup needs. Registered devices now have no bound
  space (migration 5 makes `devices.space_id` nullable); browser-approved ones still
  pick one.
- An admin made from the app has no passkey or password, so the web UI can't sign them
  in. A browser sign-in handed over from the app would fix it (open question 1).
- `users` has no `disabled` column (it's derived elsewhere): filter with `UserRow`.

## Progress log

- [x] Step 7 (2026-11-14): auth migration 6 (test that invitation spaces survive the
  rebuild), `create_browser_link`, `browser` enrollment kind, account permission on
  devices; server test `an_app_signs_its_user_in_on_a_browser_with_a_new_passkey` (new
  link each time, password refused, passkey sign-in afterwards, no permission → 403);
  enroll page, approval checkbox, app Account page button, desktop `browser_sign_in`.
  Clippy clean (workspace on sentinel, desktop locally), web typecheck and tests pass.
- [x] Deployed `f4d476d` to pokol with the user's yes (2026-11-14 1:36 AM PST, 09:36Z; auth
  migration 6 ran). Registered this PC (device NOOOK, account permission, all spaces)
  with a `register-device` token over HTTPS, stored the credential in Credential Manager
  (service `apitorrent-desktop`, key `profile:<id>:api-token`, same keyring 4.2.0 as the
  app) and wrote `%APPDATA%pitorrent\config\desktop\profiles.json` (there was none);
  the new app (`target/desktop-next`) signed in at 1:38 AM PST (09:38Z).
- [x] Deployed `bca57c2` (stricter Deluge take-over) to pokol with the user's yes
  (2026-11-14 9:42 AM PST, 17:42Z). Both Deluge daemons listen dual-stack (`*:58846`,
  `*:58847`), so `::1` reaches `delugex`.
- [ ] ← current The user: Account → "Open in my browser, signed in"; then the import
  wizard for `deluged` and `delugex`.

- [x] Plan written (2026-11-13)
- [x] Steps 1-5: auth (36 tests), server test
  `the_app_sets_up_a_headless_server_with_console_tokens`, daemon `register-device`, app
  form, installer `--remote` and messages, README, architecture. Clippy clean (workspace
  on sentinel, desktop locally); web typecheck and 83 tests pass.

## Open questions for the user

(none)

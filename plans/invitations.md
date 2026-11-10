# Invitations, user management, and plugins for space admins

## Goal

Let more people than the first admin use apitorrent, which is what "different people in
different spaces" (`plans/spaces.md`) has been waiting for:

1. **Invitations**: one-time enrollment links that create a user with the spaces and roles
   the link carries.
2. **User management** for server admins: list, disable, delete, change instance roles,
   and recovery links for users who lost their credentials.
3. **Space admins** get the Plugins page for their space's space-aware plugins.
4. **Web UI**: invitations and members on the Spaces page, a Users page, and the
   enrollment page the link opens. It must work at 390 px.

## Environment / context

- Worktree `C:\Users\camer\.t3\worktrees\apitorrent\t3-5f448d09`, branch `t3/5f448d09`,
  off `33030f7`. Local only: no push, no remotes, no servers.
- Builds: `~/.srm/bin/srmctl run --slots 4 --low --label apitorrent-invites -- cargo …`,
  `-p` the changed crates until the end. Check C: free space first; delete this worktree's
  `debug/incremental` if it falls under 20 GB.
- Web: Bun. ts-rs writes `web/src/api/generated` when the types crate's tests run.
- Checks: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked
  -- -D warnings`, `cargo test --locked`; in `web/`: `bun run typecheck`, `bun test`,
  `bun run build`; at the root: `bun run typecheck:plugins`, `bun run test:plugins`.

## Decisions already made (don't re-ask)

From the user, 2026-11-10 (`plans/spaces.md`, "Invitations"):

- A server admin can invite into any space. A space admin only into their own space.
- An invitation is a one-time link that expires (7 days by default), can be revoked, and
  carries the spaces and roles the new user gets. Only a hash of its secret is stored.
- The person opening it picks a username and registers a passkey. A password only when
  the daemon allows passwords.
- Server admins: list users, disable or delete one (never the last server admin, never
  yourself), change instance roles, issue recovery links built like invitations.
- Revoking or disabling ends that user's sessions and tokens at once.
- Space admins get the Plugins page for their space's space-aware plugins; shared plugins
  stay server-admin only (shown read-only, labelled "shared").

## Design

### Storage (`auth.db` migration 3)

- `users.disabled_at`: set while a user is disabled.
- `enrollments`: one row per invitation or recovery link. `kind` (`invite` or
  `recovery`), `secret_hash` (SHA-256 of the link's 256-bit secret), `created_by`,
  `created_at`, `expires_at`, `role` (the instance role an invitation gives), `user_id`
  (the recovery link's user; for an invitation, the user it created), `note`, `used_at`,
  `revoked_at`.
- `enrollment_spaces (enrollment_id, space_id, role)`: the spaces an invitation grants.
  Deleting a space removes its rows (`forget_space`).

### The link

`<origin>/enroll#token=<secret>`. The secret sits in the fragment, as the setup token
does, so it never reaches a server or access log; the page reads it and removes it from
the address bar. The origin is the first `--public-url` when one is configured (passkeys
work there), else the origin the admin is using.

### Endpoints

Public (rate-limited per address like setup and sign-in; a wrong, used, revoked or expired
secret counts as a failed attempt and is audited):

- `POST /auth/enroll/info {token}`: what the link grants (kind, spaces and roles, the
  username for recovery, expiry), so the page can say so.
- `POST /auth/enroll/start {token, username, displayName}` → passkey creation options.
- `POST /auth/enroll/finish {ceremonyId, credential, passkeyName}` → signed in.
- `POST /auth/enroll/password {token, username, displayName, password}`: only with
  `--passwords fallback` (a password that can't sign in alone is no way to enroll).

Invitations (a browser session with recent authentication, as creating an API token):

- `POST /invitations {role, spaces: [{slug, role}], expiresInDays, note}`.
- `GET /invitations` (server admins: all, recovery links too).
- `GET /spaces/{slug}/invitations` (that space's admins: the invitations into it).
- `DELETE /invitations/{id}`: server admins, or an admin of every space it grants.

Users (server admins):

- `GET /users`: every user with role, state, spaces, passkey and token counts.
- `PATCH /users/{id} {role?, disabled?}`; `DELETE /users/{id}`;
  `POST /users/{id}/recovery {expiresInDays}`. All need recent authentication.

Members: `GET /spaces/{slug}/members` and changing or removing an existing member are
open to that space's admins too (adding an existing user stays with server admins, who
can see the user list).

## Plan / steps

1. ~~Plan doc~~
2. ~~Auth crate: migration 3, enrollments, enroll ceremonies, user management, disabled
   users refused everywhere, unit tests.~~
3. ~~Types (ts-rs) and server routes; permission checks~~; server tests for every edge.
   ← current (running)
4. ~~Plugins: list/detail/settings/logs for space admins.~~
5. Web UI: ~~enrollment page, invitations + members on the Spaces page, Users page,
   Plugins page for space admins~~; bun tests; 390 px check in headless Chrome.
6. Docs (architecture "Authentication", "Spaces"; README), `plans/spaces.md` follow-ups.
7. Full checks, commit.

## Choices the decisions didn't cover

- **Disabling suspends tokens rather than deleting them**: sessions are deleted at once,
  tokens are refused while the user is disabled and work again once enabled (so an app's
  token survives a temporary suspension). Deleting the user removes everything.
- **Recovery replaces credentials**: the user's passkeys and password are removed and all
  sessions end when the link is used; API tokens stay. A new recovery link revokes the
  previous waiting one. A disabled user's link is refused until they're enabled.
- **Space admins manage their space's existing members** (change role, remove), not only
  invite. Adding an *existing* user to a space stays with server admins, since only they
  can list accounts.
- **Instance role on invitations**: server admins choose `admin`, `user` or `viewer`;
  space admins can only give `user`.
- **Creating invitations and recovery links, and managing users, need a browser session
  with recent authentication** (like creating API tokens). Revoking an invitation needs
  only the authority to.
- **Password enrollment only in `fallback` mode** (as setup): in `second-factor` mode a
  password alone can't sign in.
- **Nobody manages themselves**: changing your own role, disabling or deleting yourself
  is refused (403), on top of the last-admin rule (409).
- **Links are kept 30 days after they stop working** (used, revoked, expired), then
  pruned when the next one is issued.
- **`/spaces…`, `/invitations…`, `/users…` are space-independent** in `authn`: they check
  rights on what they name, so a space admin can manage tv's members from a page in a
  space where they are only a viewer.
- **Wrong links answer `401 invalid_link`** without saying whether the link was unknown,
  used, revoked or expired (the page says all four).

## Findings / gotchas

- The bash tool's heredoc wrapper fails on longer quoted heredocs here (as `m4-auth.md`
  noted): patch scripts go through files in the scratchpad.

## Progress log

- [x] Plan written (2026-11-10)
- [x] Auth crate (`enroll.rs`, migration 3; 8 new unit tests)
- [x] Types (`apitorrent-types/src/users.rs`) and server (`user_routes.rs`)
- [x] Plugins for space admins (list/detail; settings/logs already allowed)
- [ ] Server tests (`tests/invitations.rs`, plugins test) passing
- [ ] Web UI
- [ ] Docs
- [ ] Full checks

## Open questions for the user

*(none yet)*

## Things not to do

- Don't put enrollment secrets in query strings or paths (they land in logs).
- Don't push, add remotes or touch servers.
- Don't open foreground windows: headless browsers only.

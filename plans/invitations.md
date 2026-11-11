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
3. ~~Types (ts-rs) and server routes; permission checks; server tests for every edge.~~
4. ~~Plugins: list/detail/settings/logs for space admins.~~
5. ~~Web UI: enrollment page, invitations + members on the Spaces page, Users page,
   Plugins page for space admins; bun tests; 390 px check in headless Chrome.~~
6. ~~Docs (architecture "Authentication", "Spaces"; README), `plans/spaces.md` follow-ups.~~
7. ~~Full checks, commit.~~ All done 2026-11-10.

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

- Windows reserves ports 17691 to 19290 here (Hyper-V/WSL): the smoke daemon used 27431.
- A headless Chrome started with `spawn` leaves its child processes when only the parent
  is killed: clean up by the profile directory in the command line.
- `cargo test` stops at the first failing test binary; use `--no-fail-fast` to see all.
- DHT loopback test flaked once under load (ISSUES.md).

- The bash tool's heredoc wrapper fails on longer quoted heredocs here (as `m4-auth.md`
  noted): patch scripts go through files in the scratchpad.

## Progress log

- [x] Plan written (2026-11-10)
- [x] Auth crate (`enroll.rs`, migration 3; 8 new unit tests)
- [x] Types (`apitorrent-types/src/users.rs`) and server (`user_routes.rs`)
- [x] Plugins for space admins (list/detail; settings/logs already allowed)
- [x] Server tests (`tests/invitations.rs`: 4 tests; plugins: space admins) passing
- [x] Web UI (`web/src/users/`, `auth/EnrollPage.tsx`, Spaces and Plugins pages)
- [x] 390 px check: headless Chrome (DevTools protocol, no window) against a real
      daemon on loopback, driven through setup, two spaces, a server admin's and a
      space admin's invitations, enrollment and disabling. Enrollment, setup, Users,
      Spaces (space admin, members and invite form open) and Plugins all fit; the
      auth card's two-column row overflowed and was fixed.
- [x] Docs (architecture, README, `plans/spaces.md`)
- [x] Full checks: fmt, workspace clippy, `cargo test --locked` (all suites), web
      typecheck/test/build, `typecheck:plugins`, `test:plugins`

## Not done (follow-ups)

1. ~~**Joining with an existing account.**~~ Done 2026-11-10 ("Follow-ups round 2").
2. **Passkey enrollment wasn't clicked through in a browser**: the passkey ceremony is
   covered by the server tests' software authenticator, and the page was rendered
   headless; the password path was driven over the API.
3. ~~**A space admin can remove themselves**~~ Done 2026-11-10: the last admin of a space
   can't leave or step down ("Follow-ups round 2").

## Follow-ups round 2 (worktree `t3/9c1d89aa`, 2026-11-10)

Worktree `C:\Users\camer\.t3\worktrees\apitorrent\t3-9c1d89aa`, branch `t3/9c1d89aa`, off
`e2bbdc1`. Local only. Builds: `srmctl run --slots 4 --low --label apitorrent-follow`.

### Tasks

1. **Joining with an existing account.** A signed-in user who opens an invitation link
   accepts it into their own account: `POST /auth/enroll/accept {token}`. Same rate
   limit, one-time use, expiry and audit (`joined`). The enrollment page offers
   "Join as <name>" beside creating a new account. This is how a space admin adds
   someone who already has an account.
2. **The last admin of a space can't leave or demote themselves** (409). Server admins
   don't count as the space's admins, and they can still do anything.
3. **Editable tracker lists per space entry**: `PUT /torrents/{id}/trackers {tiers}`
   (the space's users), stored per entry, and an editor in the detail pane's Trackers
   tab. The engine retargets the entry's announcer without restarting tiers that keep
   a tracker; the dedup rule (one announce per URL across entries) is unchanged.
4. **`--plugin-listen <id>@<slug>=<addr>`**: the listener serves that space.
5. **`--no-update-check` / `APITORRENT_NO_UPDATE_CHECK`**: no updater, so no daily check,
   no update endpoints (`updates_unavailable`) and no update UI.

### Choices the requests didn't cover

- **Joining never changes the instance role.** A viewer cap set by a server admin stays,
  even when a space admin's invitation (role `user`) is accepted. An invitation that
  makes a server admin can't be joined with an existing account (409); a server admin
  promotes existing users on the Users page.
- **Joining never lowers a space role**: an existing membership keeps the higher of its
  role and the invitation's.
- **Joining needs a browser session** (not an API token), but not a recent sign-in: the
  link is the authority, and nothing about the account's credentials changes.
- **An invitation that would grant nothing new** (already a member at that role or
  above everywhere) is refused with 409 and stays usable, so it can go to someone else.
- **Recovery links can't be joined.**
- **The tracker editor rejects a magnet still fetching metadata with 400** (the core has
  no 409 for it), not 409 as first planned.
- **"Last admin"** counts enabled members with the space's `admin` role whose instance
  role isn't `admin`. Only a space admin acting on themselves is checked (anyone else
  acting on a space's admin is either a server admin or another admin of it).
- **Edited trackers** live in a new `torrents.trackers` column (JSON tiers; NULL = the
  `.torrent`'s own). A magnet still waiting for its metadata can't be edited (409).
  Tiers are trimmed, empty tiers dropped, a URL listed twice in one entry is refused;
  only `http`, `https` and `udp` URLs.
- **`update` subcommand under `--no-update-check`**: `--check` still reports; installing
  is refused ("update it the way it was installed").
- **Joining lands in the first space it gave** (a full page load at `/s/<slug>/`), and
  makes that the user's last space.
- **The Trackers tab edits tiers as text boxes** (one URL per line) with up, down and
  remove per tier and "Add tier"; viewers don't get the button. Shown only once the
  metadata is in.

### Progress

All done 2026-11-10 (times US Pacific).

- [x] 1 and 2. Joining with an existing account, and the last space admin (`910dc03`):
      auth unit tests `existing_users_join_invitations`, `the_last_space_admin_stays`;
      server tests `existing_users_join_with_their_account`,
      `the_last_space_admin_cant_leave`
- [x] 5. `--no-update-check` (`b3509bd`): a parse test; the server's
      `updates_unavailable` path was already tested
- [x] 4. `--plugin-listen <id>@<slug>` (`40644ba`): parse and path tests, and the
      qBittorrent end-to-end test serves space `tv` on its own listener (tv's token
      works, default's gets 403)
- [x] 3. Editable trackers (`2f1cd2c`): announcer retarget unit tests, the engine
      test `an_entrys_trackers_change_while_it_runs` (real HTTP trackers: no
      stop/start for kept trackers, `stopped` for dropped ones, hand-over on pause),
      the core's list cleaning and db round-trip, the server test
      `a_spaces_users_edit_their_entrys_trackers` (viewers and read tokens refused,
      other spaces untouched, kept across a restart), and web tests for the tier helpers
- [x] Docs (`c4fc83e`): architecture, README, `plans/spaces.md` and
      `plans/update-signing.md` follow-ups
- [x] Full checks: fmt, workspace clippy, `cargo test --locked` (737 passed),
      web typecheck/test/build, `typecheck:plugins`, `test:plugins`
- [x] 390 px check, 6:08 PM PST (02:08Z): headless Chrome (playwright-core, a
      headless shell) against the debug daemon on 127.0.0.1:27431 with
      `--no-update-check`. Bob, signed in, joined a second invitation with "Join as
      Bob" and landed in `/s/default/` as a viewer, keeping tv. The Trackers tab's
      editor was used to add a tier and move it up, then saved. All three screens fit
      at 390 px.
- [x] Rebased onto `master` (`9ec0b99`, the web UI review). The only conflict was
      `DetailPane.tsx`: master added a `readOnly` prop for viewers, so the Trackers
      editor now follows it instead of its own space-role check. No Rust source
      changed in the rebase. Web typecheck, tests and build, and the 390 px check, were
      run again on the rebased branch; a viewer gets no "Edit trackers".

### Findings

- The bash tool's heredoc wrapper still fails on long quoted heredocs: write patch
  scripts to the scratchpad with the Write tool.
- With the auth tests' manual clock, links made in one test share `created_at`, so
  "newest first" can't pick one: find a record by id.
- `cargo` puts the linked daemon in the worktree's `target/debug`, not in the shared
  build dir.
- The Trackers tab lists each tier's trackers in the engine's order, which is shuffled
  once per tier (BEP 12). The editor shows the saved order.

## Open questions for the user

*(none yet)*

## Things not to do

- Don't put enrollment secrets in query strings or paths (they land in logs).
- Don't push, add remotes or touch servers.
- Don't open foreground windows: headless browsers only.

# Remote add: a key-in-the-URL add endpoint for tracker-page links

## Goal

Replace pokol's `http-endpoints` service (`GET https://pokol.isozilla.com/addTorrent/<url>`,
unauthenticated, feeds Deluge) with an apitorrent plugin, so the custom JavaScript on the
user's tracker profile page can send a torrent to apitorrent the same way: a `window.open`
GET, the server fetches the `.torrent`, and the torrent never touches the local machine.

## Environment / context

- Today's sender: custom JS on the user's tracker user page (a tracker setting, not a
  userscript). It rewrites download links into
  `https://pokol.isozilla.com/addTorrent/<encodeURIComponent(link)>[?alternate=true]` and
  opens that in a popup named `isoZillaPopup`. The service answers
  `<script>window.close()</script>`.
- `alternate=true` sends the torrent to a second Deluge (port 58847 instead of 58846).
- Live service on pokol (checked read-only 2026-11-14): `http-endpoints.service` runs
  `/opt/http-endpoints` (drifted from the ops checkout: it has `alternate`), on the unix
  socket `/run/endpoints/request-handler.socket`, reached through nginx `rescue.conf`
  (`pokol.isozilla.com`). `del-add.isozilla.com` points at `localhost:4000`, where nothing
  listens: dead.
- Plugin SDK: `packages/plugin-sdk`; first-party plugins in `plugins/*`, embedded by the
  `first-party-plugins` feature (`bun run build:first-party`).

## Decisions already made (don't re-ask)

- **A plugin, not a core endpoint.** Core auth stays header/cookie only; core never fetches
  URLs. The token-in-URL risk stays opt-in inside one plugin.
- **GET with the key in the query string.** Page JS on the tracker can't send an
  `Authorization` header cross-origin (apitorrent never grants CORS preflights), and a
  cross-site form POST is refused by the CSRF guard (`guard.rs`), which stays as is.
- **The server fetches the `.torrent`.** The user wants the torrent never on their machine.
- **Add-only keys owned by the plugin**, not apitorrent API tokens (those are `read` or
  `full`; a leaked `full` token could delete torrents). The key is visible to tracker staff
  (it's in profile JS) and in proxy logs, so a key may only add.
- **Fetch only from allow-listed hosts** (`networkFromSettings`), so a leaked key can only
  add torrents from the user's tracker.
- **No magnets.** The user doesn't want them.
- **Keys are generated in the UI** (the user's "sounds good" to the recommendation): a
  `format: "token"` secret gets a Generate button; stored as a write-only plugin setting.
- **No proxy change**: apitorrent is already public at `https://apitorrent.isozilla.com`
  (nginx `apitorrent.conf` on pokol), so the links point there.
- **`alternate` maps to a second space**: the plugin is space-aware, each space instance has
  its own key and settings, URL base `/s/<slug>/api/v1/plugins/remote-add/add`.

## Plan / steps

1. ✅ Plugin `plugins/remote-add` (`09d3071`).
2. ✅ Bun tests (9) with the SDK harness.
3. ✅ Rust end-to-end test `crates/apitorrentd/tests/remote_add.rs`: cross-site GET,
   auth on, no sign-in, IP-granted local "tracker".
4. ✅ Docs: README section, architecture.md plugin list and settings schema.
5. ✅ Generate button: schema `format: "token"` (secrets only), Plugins page (`7269135`).
6. ⏭ **Current:** push + deploy to pokol (needs the user's yes; same procedure as
   `plans/settings-page-redesign.md`: build on sentinel from a clean worktree). The
   running daemon would refuse the manifest's `format: "token"` until it's updated.
7. User: approve/enable `remote-add` on the Plugins page, Generate a key, set hosts to
   the tracker's host, switch the tracker snippet (one step at a time).
8. Afterwards: retire pokol's `http-endpoints.service` (ops repo, per-change approval).

## Findings / gotchas

- Plugin responses are served with `sandbox; default-src 'none'` CSP, and plugins can't
  override it, so the old `<script>window.close()</script>` popup can't close itself.
  Answer: `reply=image` (GIF on success, error status otherwise) for an `<img>` on the
  tracker page; the popup page stays for showing errors.
- `"auth": "plugin"` routes need the `api-compat` role (validate.rs), even with nothing to
  provide: `api.roles.provide("api-compat", {})`.
- A cross-site **GET** passes the CSRF guard (it only checks state-changing methods); the
  end-to-end test sends `Origin`/`Sec-Fetch-Site: cross-site` to prove it.
- Plugin fetches drop loopback/private answers for host *names*; an IP grant
  (`http://127.0.0.1:port`) works, which the end-to-end test uses.
- apitorrent's `TraceLayer` puts the full URI (with the key) in spans at DEBUG level only.

## Progress log

- [x] Plugin (`09d3071`)
- [x] Tests: 9 Bun, 1 Rust end-to-end; plugin-api schema tests; web settingsSchema test
- [x] Docs
- [x] Generate button (`7269135`)
- [ ] Push + deploy to pokol (waiting on the user's yes)
- [ ] Enable + key + hosts + tracker snippet (user)
- [ ] Retire `http-endpoints.service` on pokol (ops, per-change approval)

## Open questions for the user

1. Push `master` and deploy to pokol? (Recommended; needed before the plugin installs.)
2. Which apitorrent space stands in for `alternate` (the second Deluge, `delugex`)? Does
   one exist on pokol yet, and what's its slug?

## Things not to do

- Don't loosen the CSRF guard or add CORS to core for this.
- Don't accept apitorrent API tokens as the key.
- Don't touch pokol's nginx/Caddy or Cloudflare without per-change approval.

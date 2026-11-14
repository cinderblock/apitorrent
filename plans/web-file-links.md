# Web file links: HTTP(S) share locators

## Goal

Let a storage root be reached over plain HTTP(S), such as an nginx `autoindex` listing of
the download directory: `https://my-files.server.tld/path/to/downloads/`. With such a
locator set, the web UI (in any browser, and in the desktop app) links each file to its URL
on that server and the save folder to its directory listing, so files can be fetched
without SMB/NFS or the desktop app.

Asked 2026-11-14 ~2 PM PST (22:00Z).

## Environment / context

- Share locators: `crates/apitorrent-types/src/share.rs` (parse, normalise), stored by
  root id (`apitorrent-core/src/roots.rs`), set in Settings → Files → Storage roots
  (`web/src/components/StorageRootsSection.tsx`, `PUT /api/v1/roots/{id}/locators`),
  listed by `GET /api/v1/roots` (any signed-in user).
- Desktop side: `apps/desktop/src-tauri/src/shares.rs` turns locators into local path
  candidates for path mappings; `mapping.rs` suggests them.
- File rows and the save path: `web/src/components/DetailPane.tsx`, next to the desktop's
  `PathActions`.
- Desktop webview: plain navigation to an `http(s)` URL is refused and opened in the
  default browser (`on_navigation` in `apps/desktop/src-tauri/src/lib.rs`).

## Decisions already made (don't re-ask)

- **An HTTP(S) URL is a new kind of share locator** (`ShareProtocol::Http`/`Https`), in
  the same list as UNC/SMB/NFS, so admins set it in the same place. Credentials, queries
  and fragments are refused as for the others (locators are visible to every user). Its
  path may be empty (a whole host), and it is written with a trailing slash.
- **The first web locator of a root is the one links use.** Admins order the list.
- **Links are built in the browser** from `GET /roots` (root path prefix → relative
  components, each percent-encoded), refusing `.`/`..`, the same rules as the desktop's
  `RemotePath`. Windows-style roots compare case-insensitively.
- **Checking is advisory, done by the daemon** (`POST /roots/{id}/locators/check`, server
  admins): it fetches `<base>/.apitorrent-root` with the strict HTTP client and compares the
  id. A browser can't check cross-origin without CORS headers on the file server. Links show
  whether or not the check passed: the server may not reach its own public URL (hairpin
  NAT), or the files may sit behind a sign-in. A served marker with another id is reported
  as wrong.
- **Desktop app**: web locators give no local path candidates. Links in the desktop open
  in the default browser (no `target=_blank` there; navigation is intercepted).

## Plan / steps

1. Plan doc
2. `share.rs`: HTTP(S) locators, tests.
3. Desktop `shares.rs`: no candidates for web locators.
4. Core + server: locator check endpoint, test.
5. Web: link helper + tests, links in the details pane, check results in Settings.
6. Docs: architecture "Remote file access", README.
7. Checks: clippy, tests, bun typecheck/test/lint. Commit.

## Findings / gotchas

- Links show only for finished files: a partial file would download with holes (sparse
  zeros) and look fine.
- Spaces' own storage roots aren't roots yet (ISSUES.md "Space storage roots aren't storage
  roots for the desktop app"), so torrents saved there get no web links either.
- nginx `autoindex` hides dotfiles from the listing but serves them on request, so the
  marker check works unless the config denies `/\.` paths (a common hardening rule); the
  check's message says so on a 403/404.

## Progress log

All steps done 2026-11-14 (afternoon PST).

- [x] 1 plan
- [x] 2 share.rs: `Http`/`Https`, `web_base`, `web_marker_url`; web paths dedupe
  case-sensitively; tests `web_urls`, refusals, normalising
- [x] 3 desktop `shares.rs`: no candidates (`web_servers_give_no_local_paths`)
- [x] 4 `Core::check_root_locator`, `roots::check_web_locator`,
  `POST /roots/{id}/locators/check`; test `admins_check_web_locators` (a tower-http
  `ServeDir` stands in for nginx)
- [x] 5 web: `components/webLinks.ts` (+ tests), `WebLink.tsx`; Download per finished file,
  Browse on the save path; Settings checks web locators on save and on a button
- [x] 6 docs: README "Downloading files over the web", architecture "Remote file access"
- [x] 7 clippy, fmt, Rust tests (types, core roots, server roots, desktop), bun typecheck,
  test, build; committed
- [ ] Not done: a live run of the UI against a real nginx; logic is covered by tests only

Side fix, own commit `f72ee3c`: the desktop test `the_file_format_is_stable` still expected
the old 6881 default port.

## Open questions for the user

(none yet)

## Things not to do

- Don't put credentials in locators to make a protected file server work: every signed-in
  user can read them.

# First-class dark mode

## Goal

Dark mode that is a real, chosen theme rather than a side effect of the OS setting:

- A per-device choice of **System / Light / Dark**, remembered, reachable from the UI.
- The first frame is already the right colour: no white flash in the browser or the
  desktop window, in production and in `vite dev`.
- Native chrome follows the choice too: form controls and scrollbars (`color-scheme`), the
  mobile browser bar (`theme-color`), and the desktop window's title bar and background.

## Environment / context

- Web UI: `web/` (React + Vite, its own `bun.lock`). Styles in `web/src/styles.css` plus
  per-feature sheets (`desktop/`, `plugins/`, `rules/`).
- Before this work, dark mode existed only as `@media (prefers-color-scheme: dark)` token
  overrides in `styles.css`, plus `<meta name="color-scheme" content="light dark">`.
- CSP, in both the daemon (`crates/apitorrent-server/src/lib.rs`, `CONTENT_SECURITY_POLICY`)
  and Tauri (`apps/desktop/src-tauri/tauri.conf.json`), is `script-src 'self'; style-src
  'self'`: **no inline `<script>` or `<style>`**.
- The daemon serves every file in `web/dist` (embedded by `crates/apitorrent-webui/build.rs`);
  files outside `/assets/` are `Cache-Control: no-cache` with an ETag.
- Desktop: Tauri 2.12.5. The main window is built in `apps/desktop/src-tauri/src/lib.rs`.

## Decisions already made (don't re-ask)

- **Preference is per device**, in `localStorage` (`apitorrent.theme`), not a server setting:
  different screens want different themes, and it must apply before sign-in.
- **No inline script/style, no CSP hashes.** The early pieces are same-origin, render-blocking
  files in `<head>`, which the existing CSP already allows:
  - `web/public/theme-boot.js`: a classic (non-module) script that reads the preference and
    sets `data-theme` on `<html>` and the `color-scheme` meta before the body parses.
  - `web/src/theme.css`: the colour tokens, `color-scheme` and the `html` background,
    linked straight from `index.html` so it is render-blocking even in `vite dev` (where
    JS-imported CSS arrives late).
- **Tokens use `light-dark()`**, driven by `color-scheme`, so each colour is written once and
  `data-theme` only has to flip `color-scheme`.
- **Desktop mirrors the choice natively**: a `theme_set` command stores it in the desktop
  config dir and sets the window theme (title bar, WebView2's `prefers-color-scheme`) and
  background colour; at startup the window is built with them, so it never opens white.

## Plan / steps

1. Move tokens to `theme.css` with `light-dark()`; `data-theme` overrides. ✅
2. `theme-boot.js` in `public/`; wire both into `index.html`; `theme-color` metas. ✅
3. `web/src/theme.ts` (read/write/apply/subscribe) + toolbar picker. ✅
4. Desktop: stored preference, window theme + background at build, `theme_set` command,
   follow OS changes when on System. ✅
5. Verify: typecheck, tests, build output, cargo check/clippy/test, look in a browser. ✅
6. Not done: watch the desktop window open in each theme (needs the foreground, or the
   WebView2 automation another thread is adding).

## Findings / gotchas

- **Vite merges `theme.css` into the one CSS bundle** in production (a render-blocking
  `<link>` in `<head>`), so the "early" part there is that all CSS blocks first paint and
  `theme-boot.js` runs before it. In `vite dev` the separate `<link>` matters: `styles.css`
  arrives through JS, but `theme.css` already paints the `html` background.
- **Vite (lightningcss) downlevels `light-dark()`** for the es2022 target into
  `--lightningcss-light/dark` toggles, and translates the `[data-theme]` `color-scheme`
  overrides correctly (checked in the built CSS), so it works on older WebKitGTK too.
- Headless Chromium against the built UI under the daemon's exact CSP, app JS blocked:
  `html` and `body` backgrounds correct for OS light/dark × stored none/light/dark, no CSP
  errors. Full app on a throwaway `--insecure-no-auth-loopback` daemon: the picker applies
  at once, persists across reload, and System clears the stored value.
- The Python Playwright install has only Chromium browsers matching its version; Firefox and
  WebKit runs failed to launch (missing executables), so only Chromium was checked.
- The desktop window was **not** launched for a visual check (it would take the
  foreground). Covered by `cargo clippy`/`cargo test` (theme round-trip, wire names, and a
  test that the window background colours match `--bg` in `theme.css`).

## Progress log

- [x] Plan written
- [x] theme.css / boot script / index.html
- [x] theme.ts + picker
- [x] Desktop window theme + background
- [x] Checks and browser verification
- [ ] Desktop window seen opening in dark/light (not run: would take the foreground)

## Things not to do

- Don't add inline `<style>`/`<script>` to `index.html`: both CSPs block them.
- Don't hardcode the light/dark background hexes anywhere but `theme.css` without a test
  tying them together (the desktop window colours are checked by a Rust test).

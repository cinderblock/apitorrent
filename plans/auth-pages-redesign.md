# Auth pages redesign

## Goal

The first-run setup page (and the sign-in, enroll and desktop connect cards that share its
styles) looked cramped and unfinished: 13px text, a misaligned two-column row (the
Display name input stretched taller than Username because of the hint below it), a small
right-aligned submit button, no brand mark. Make them look deliberate.

## Environment / context

- Code: `web/src/auth/{SetupPage,LoginPage,EnrollPage,AuthGate}.tsx`,
  `web/src/desktop/DesktopGate.tsx`, styles in `web/src/styles.css` ("Sign-in, setup and
  account" section).
- Brand mark: the desktop icon, `apps/desktop/src-tauri/icons/icon.svg`.
- Checks: in `web/`, `bun run typecheck`, `bun test`, `bun run build`.
- Screenshots: Playwright (Python) headless, page served by the debug daemon in a fresh
  data dir so it shows the setup page.

## Decisions

- Shared `AuthHeader` (mark + "apitorrent" + page title + optional lead) for every card.
- Page title is the `h1`; the brand is not a heading.
- Full-width primary button; "or" divider before the password fallback.
- Setup token explanation moves under its input as a hint.

## Progress

- [x] AuthHeader component and styles
- [x] Setup, Login, Enroll, AuthGate, DesktopGate, DesktopProblem use it
- [x] Screenshots light/dark, desk/phone: 9 states (setup, error, no passkeys + insecure,
  sign-in variants, second factor, enroll), API mocked in Playwright, served from `web/dist`
- [x] Checks pass, commit

## Findings

- `.field-row` children are grid items that stretched to the tallest sibling, so a hint
  under one input made the neighbouring input taller. Fixed for every form with
  `align-content: start`.
- A desktop-only `gap` on `.auth-card .field-row` has to sit in a `min-width` media query;
  the phone rule earlier in the file loses on source order otherwise.
- Not checked in the real desktop window: the Connections card (`.profiles-card`) now picks
  up the 14px card type and field styles.

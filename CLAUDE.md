# Agent notes

## Where it runs

Production apitorrent runs on a headless seedbox, not on the Windows development machine.
Design defaults (remote access, web UI settings, headless operation) around a headless
server.

On the development machine, short real-world interop tests are fine, but leave nothing
behind: no daemons still running, no firewall rules, no seeding. Anything long-running
belongs on the seedbox. If an interop test needs an inbound Windows Firewall rule, add it
in its own group (`apitorrent-testing`) and remove it when the test ends.

## How to describe it

apitorrent is something new, not a replacement for Deluge or any other client. It takes
the best features of Deluge, qBittorrent, Transmission and others, isn't held back by
their old decisions, and adds features for the communities that use them. Speak well of
other clients, and never say what another client "can't do". Deluge import is an
onboarding feature, not a pitch.

This applies to READMEs, docs, UI copy, release notes, commit messages, and briefs for
other agents.

## The local desktop app

The user runs the desktop app on the development machine from
`target/desktop-app/run/apitorrent-desktop.exe`. After a change lands on master, always
rebuild it and restart it, without asking (the user, 2026-11-22: "yes. always"):

1. Build from a clean worktree at that commit, so other threads' uncommitted work stays
   out: `bun run build` in `web/`, then `cargo build --release -p apitorrent-desktop
   --features tauri/custom-protocol`. Without `custom-protocol` the window points at the
   Vite dev server and shows a black window.
2. Stop the running app by its PID (never by name), copy the new exe over the one in
   `target/desktop-app/run/` (copy, don't link), and start it again.
3. Check that it came up, and say which commit it now runs.

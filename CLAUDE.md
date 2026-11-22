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

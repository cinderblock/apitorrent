# Issues

Problems found while working on something else. Each entry is enough to start a fix.

## The web UI has no phone layout

At 390 px wide, the toolbar runs off the right edge (the queue buttons, Settings, Search
and the rate readout are cut off), the 12rem sidebar takes half the screen, and the torrent
table shows about one column. Seen on 2026-11-08 at 10:50 PM PST (06:50Z), with a headless
Chromium at a 390×800 viewport against `apitorrentd --web-root web/dist`.

It matters because the seedbox's web UI is the main way in, often from a phone. Needs: a
collapsible sidebar (a drawer), a toolbar that wraps or folds into a menu, and a list or
card view of torrents in place of the wide grid on narrow screens.

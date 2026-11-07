"""Write tests/fixtures/torrents.state: a pickle shaped like Deluge 2's.

Deluge's TorrentManager saves a TorrentManagerState whose `torrents` list holds
TorrentState objects (deluge/core/torrentmanager.py), with pickle protocol 2. The classes
are faked here so no Deluge install is needed. Run with any Python 3:

    python tests/fixtures/make_state.py
"""

import os
import pickle
import sys
import types

for name in ("deluge", "deluge.core", "deluge.core.torrentmanager"):
    sys.modules[name] = types.ModuleType(name)
tm = sys.modules["deluge.core.torrentmanager"]


class TorrentState:
    def __init__(self, **kw):
        defaults = dict(
            torrent_id=None,
            filename=None,
            trackers=None,
            storage_mode="sparse",
            paused=False,
            save_path=None,
            max_connections=-1,
            max_upload_slots=-1,
            max_upload_speed=-1.0,
            max_download_speed=-1.0,
            prioritize_first_last=False,
            sequential_download=False,
            file_priorities=None,
            queue=None,
            auto_managed=True,
            is_finished=False,
            stop_ratio=2.00,
            stop_at_ratio=False,
            remove_at_ratio=False,
            move_completed=False,
            move_completed_path=None,
            magnet=None,
            owner=None,
            shared=False,
            super_seeding=False,
            name=None,
        )
        defaults.update(kw)
        self.__dict__.update(defaults)


class TorrentManagerState:
    def __init__(self):
        self.torrents = []


for cls in (TorrentState, TorrentManagerState):
    cls.__module__ = "deluge.core.torrentmanager"
    setattr(tm, cls.__name__, cls)

state = TorrentManagerState()
state.torrents.append(
    TorrentState(
        torrent_id="a" * 40,
        filename="ubuntu.iso.torrent",
        trackers=[
            {"url": "https://torrent.ubuntu.com/announce", "tier": 0},
            {"url": "https://ipv6.torrent.ubuntu.com/announce", "tier": 1},
        ],
        paused=False,
        save_path="/srv/torrents/iso",
        max_download_speed=500.0,
        max_connections=40,
        file_priorities=(4,),
        queue=1,
        is_finished=True,
        name="ubuntu.iso",
    )
)
state.torrents.append(
    TorrentState(
        torrent_id="b" * 40,
        filename="pack.torrent",
        paused=True,
        save_path="/srv/torrents/tv",
        file_priorities=(4, 0),
        queue=0,
        name="pack",
    )
)

out = os.path.join(os.path.dirname(__file__), "torrents.state")
with open(out, "wb") as f:
    pickle.dump(state, f, protocol=2)
print("wrote", out)

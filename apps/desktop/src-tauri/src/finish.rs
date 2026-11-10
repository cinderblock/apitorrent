//! Spotting torrents that finish downloading, for a desktop notification.

use std::collections::HashMap;

use apitorrent_types::{SyncMessage, TorrentState, TorrentSummary};

/// Follows the sync stream and reports torrents that went from "still downloading" to
/// "every wanted piece is here". Torrents first seen already complete aren't reported, and
/// neither are checks (a recheck of a complete torrent isn't news).
#[derive(Debug, Default)]
pub struct FinishWatcher {
    /// Last known "finished" per torrent. Absent until first seen in a settled state.
    known: HashMap<String, bool>,
}

/// `None` while the state says nothing either way (fetching metadata, checking files).
fn finished(t: &TorrentSummary) -> Option<bool> {
    match t.state {
        TorrentState::Metadata | TorrentState::Checking => None,
        TorrentState::Error => Some(false),
        _ => Some(t.progress >= 1.0),
    }
}

impl FinishWatcher {
    /// Feed one sync message; returns the names of torrents that just finished.
    pub fn feed(&mut self, msg: &SyncMessage) -> Vec<String> {
        match msg {
            SyncMessage::Snapshot { torrents, .. } => {
                // A snapshot after a reconnect: compare with what we knew, then start over
                // from it, so torrents removed in the gap are forgotten.
                let mut done = Vec::new();
                let mut next = HashMap::with_capacity(torrents.len());
                for t in torrents {
                    let before = self.known.get(&t.id).copied();
                    let now = finished(t).or(before);
                    if before == Some(false) && now == Some(true) {
                        done.push(t.name.clone());
                    }
                    if let Some(now) = now {
                        next.insert(t.id.clone(), now);
                    }
                }
                self.known = next;
                done
            }
            SyncMessage::Patch {
                upserted, removed, ..
            } => {
                for id in removed {
                    self.known.remove(id);
                }
                let mut done = Vec::new();
                for t in upserted {
                    let Some(now) = finished(t) else { continue };
                    let before = self.known.insert(t.id.clone(), now);
                    if before == Some(false) && now {
                        done.push(t.name.clone());
                    }
                }
                done
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: &str, state: TorrentState, progress: f64) -> TorrentSummary {
        let json = serde_json::json!({
            "id": id, "name": format!("name-{id}"), "infoHashV1": null, "infoHashV2": null,
            "state": state, "progress": progress, "size": 1, "piecesHave": 0,
            "piecesTotal": 1, "downloaded": 0, "uploaded": 0, "downloadRate": 0,
            "uploadRate": 0, "peers": 0, "seeds": 0, "addedAt": 0, "label": null,
            "savePath": "/x", "private": false, "error": null, "queuePosition": 0,
            "group": null, "supersededBy": null,
        });
        serde_json::from_value(json).expect("a summary")
    }

    fn patch(upserted: Vec<TorrentSummary>) -> SyncMessage {
        SyncMessage::Patch {
            revision: 0,
            upserted,
            removed: vec![],
            removed_spaces: vec![],
        }
    }

    #[test]
    fn reports_the_transition_once() {
        let mut w = FinishWatcher::default();
        let snap = SyncMessage::Snapshot {
            revision: 0,
            torrents: vec![
                t("a", TorrentState::Downloading, 0.5),
                t("b", TorrentState::Seeding, 1.0),
            ],
        };
        assert!(w.feed(&snap).is_empty(), "nothing finished *now*");
        assert!(
            w.feed(&patch(vec![t("a", TorrentState::Downloading, 0.9)]))
                .is_empty()
        );
        assert_eq!(
            w.feed(&patch(vec![t("a", TorrentState::Seeding, 1.0)])),
            ["name-a"]
        );
        assert!(
            w.feed(&patch(vec![t("a", TorrentState::Seeding, 1.0)]))
                .is_empty()
        );
        assert!(
            w.feed(&patch(vec![t("a", TorrentState::Paused, 1.0)]))
                .is_empty()
        );
    }

    #[test]
    fn checks_and_new_complete_torrents_are_quiet() {
        let mut w = FinishWatcher::default();
        w.feed(&SyncMessage::Snapshot {
            revision: 0,
            torrents: vec![t("a", TorrentState::Seeding, 1.0)],
        });
        // A recheck shows progress climbing from zero; that isn't a new download.
        for p in [0.0, 0.5, 1.0] {
            assert!(
                w.feed(&patch(vec![t("a", TorrentState::Checking, p)]))
                    .is_empty()
            );
        }
        assert!(
            w.feed(&patch(vec![t("a", TorrentState::Seeding, 1.0)]))
                .is_empty()
        );
        // Added already complete (from the library, say).
        assert!(
            w.feed(&patch(vec![t("c", TorrentState::Checking, 1.0)]))
                .is_empty()
        );
        assert!(
            w.feed(&patch(vec![t("c", TorrentState::Seeding, 1.0)]))
                .is_empty()
        );
    }

    #[test]
    fn a_reconnect_snapshot_catches_what_finished_meanwhile() {
        let mut w = FinishWatcher::default();
        w.feed(&SyncMessage::Snapshot {
            revision: 0,
            torrents: vec![t("a", TorrentState::Downloading, 0.2)],
        });
        let done = w.feed(&SyncMessage::Snapshot {
            revision: 9,
            torrents: vec![t("a", TorrentState::Seeding, 1.0)],
        });
        assert_eq!(done, ["name-a"]);
    }

    #[test]
    fn removed_torrents_are_forgotten() {
        let mut w = FinishWatcher::default();
        w.feed(&patch(vec![t("a", TorrentState::Downloading, 0.2)]));
        w.feed(&SyncMessage::Patch {
            revision: 1,
            upserted: vec![],
            removed: vec!["a".into()],
            removed_spaces: vec![],
        });
        assert!(
            w.feed(&patch(vec![t("a", TorrentState::Seeding, 1.0)]))
                .is_empty()
        );
    }
}

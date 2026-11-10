//! Rules over release candidates, end to end in the core: taking the latest release with a
//! file selection, a dry run, superseding the older one with seed limits, a stable alias
//! that follows the newest completed release, and removed torrents staying removed.

use apitorrent_types::spaces::DEFAULT_SPACE;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use apitorrent_core::{Core, CoreConfig, Key};
use apitorrent_engine::SessionConfig;
use apitorrent_metainfo::{FileSource, TorrentBuilder, Version};
use apitorrent_plugin_api::{AppError, BoxFuture, SourceFetcher};
use apitorrent_types::automation::{
    AliasMethod, AliasSpec, CandidateFilter, CandidateLinks, DryRunRequest, DryRunVerdict,
    FileSelection, MatchMode, MetadataMatch, ReleaseCandidate, RuleInfo, RuleItemState, RuleSpec,
    SupersededAction, TextMatch, VersionOrder,
};
use apitorrent_types::{SeedLimitAction, SeedLimits, TorrentState};

/// Serves `.torrent` files by URL, as the plugin host would for a source plugin, and
/// remembers who asked.
#[derive(Default)]
struct FakeFetcher {
    files: Mutex<HashMap<String, Vec<u8>>>,
    calls: Mutex<Vec<(String, String)>>,
}

impl SourceFetcher for FakeFetcher {
    fn fetch_as(
        &self,
        plugin_id: &str,
        _space: apitorrent_types::spaces::SpaceId,
        url: &str,
        _max: usize,
    ) -> BoxFuture<Result<Vec<u8>, AppError>> {
        self.calls
            .lock()
            .unwrap()
            .push((plugin_id.to_owned(), url.to_owned()));
        let found = self.files.lock().unwrap().get(url).cloned();
        Box::pin(async move { found.ok_or_else(|| AppError::new("fetch_failed", "404")) })
    }
}

fn config(root: &std::path::Path) -> CoreConfig {
    CoreConfig {
        data_dir: root.join("data"),
        download_dir: root.join("downloads"),
        session: SessionConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            ..SessionConfig::default()
        },
    }
}

/// A release torrent with an ISO and a checksum file, whose data is already on disk (so
/// it is complete once checked).
fn release(root: &std::path::Path, version: &str, fill: u8) -> (Vec<u8>, Vec<u8>) {
    let name = format!("ubuntu-{version}");
    let iso_name = format!("ubuntu-{version}-desktop-amd64.iso");
    let iso: Vec<u8> = (0..200_000u32)
        .map(|i| u8::try_from(i % 251).unwrap() ^ fill)
        .collect();
    let sums = format!("sha256  {iso_name}\n").into_bytes();
    let torrent = TorrentBuilder::new(&name, Version::Hybrid)
        .piece_length(16 * 1024)
        .file(FileSource::from_bytes(&[iso_name.as_str()], iso.clone()))
        .file(FileSource::from_bytes(&["SHA256SUMS"], sums.clone()))
        .build()
        .unwrap();
    let dir = root.join("downloads").join("ubuntu").join(&name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(&iso_name), &iso).unwrap();
    std::fs::write(dir.join("SHA256SUMS"), &sums).unwrap();
    (torrent, iso)
}

fn candidate(version: &str, flavor: &str) -> ReleaseCandidate {
    ReleaseCandidate {
        id: format!("ubuntu-{version}-{flavor}-amd64.iso.torrent"),
        title: format!("Ubuntu {version} {flavor}"),
        version: Some(version.to_owned()),
        published: None,
        links: CandidateLinks {
            torrent: Some(format!(
                "https://releases.example/{version}-{flavor}.torrent"
            )),
            ..CandidateLinks::default()
        },
        metadata: serde_json::json!({ "flavor": flavor, "arch": "amd64" })
            .as_object()
            .unwrap()
            .clone(),
    }
}

fn rule() -> RuleSpec {
    RuleSpec {
        name: "Ubuntu desktop".into(),
        enabled: true,
        filter: CandidateFilter {
            sources: vec!["plugin:ubuntu-releases".into()],
            metadata: vec![MetadataMatch {
                field: "flavor".into(),
                matches: TextMatch {
                    mode: MatchMode::Equals,
                    pattern: "desktop".into(),
                },
            }],
            ..CandidateFilter::default()
        },
        order: VersionOrder::default(),
        keep_latest: 1,
        label: Some("ubuntu".into()),
        save_path: Some("ubuntu".into()),
        files: FileSelection {
            include: vec!["**/*.iso".into()],
            exclude: vec![],
        },
        superseded: SupersededAction::SeedLimits {
            limits: SeedLimits {
                ratio: 5.0,
                seed_minutes: 0,
                action: SeedLimitAction::Remove,
            },
        },
        paused: false,
        alias: Some(AliasSpec {
            path: "library/ubuntu/latest-desktop-amd64.iso".into(),
            file: Some("*.iso".into()),
        }),
    }
}

/// Run the rules until `done` holds for the rule (passes also update aliases).
async fn settle(core: &Core, id: &str, what: &str, done: impl Fn(&RuleInfo) -> bool) -> RuleInfo {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        core.run_rules().await;
        let info = core.rule(DEFAULT_SPACE, id).await.unwrap();
        if done(&info) {
            return info;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}: {info:#?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn active(info: &RuleInfo) -> Vec<String> {
    info.items
        .iter()
        .filter(|i| i.state == RuleItemState::Active)
        .map(|i| i.version.clone().unwrap_or_default())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rules_take_the_latest_supersede_and_keep_an_alias() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path());
    let (t24, iso24) = release(root.path(), "24.04", 0);
    let (t26, iso26) = release(root.path(), "26.04", 0x5a);
    let fetcher = Arc::new(FakeFetcher::default());
    fetcher.files.lock().unwrap().extend([
        (
            "https://releases.example/24.04-desktop.torrent".to_owned(),
            t24,
        ),
        (
            "https://releases.example/26.04-desktop.torrent".to_owned(),
            t26,
        ),
    ]);
    let core = Core::start(cfg.clone()).await.unwrap();
    core.set_source_fetcher(fetcher.clone());

    // Bad rules are refused up front.
    let mut bad = rule();
    bad.filter.title = Some(TextMatch {
        mode: MatchMode::Regex,
        pattern: "(".into(),
    });
    assert!(core.create_rule(DEFAULT_SPACE, bad).await.is_err());
    let mut bad = rule();
    bad.alias.as_mut().unwrap().path = "../outside.iso".into();
    assert!(core.create_rule(DEFAULT_SPACE, bad).await.is_err());

    let out = core
        .submit_candidates(
            None,
            "plugin:ubuntu-releases",
            vec![
                candidate("24.04", "desktop"),
                candidate("24.04", "live-server"),
                ReleaseCandidate {
                    links: CandidateLinks::default(),
                    ..candidate("24.10", "desktop")
                },
            ],
        )
        .await
        .unwrap();
    assert_eq!(out.accepted, 3);
    let info = core.create_rule(DEFAULT_SPACE, rule()).await.unwrap();
    let id = info.id.clone();

    // 24.04 desktop is taken (the server image doesn't match, 24.10 has no link).
    let info = settle(&core, &id, "24.04 taken", |i| active(i) == ["24.04"]).await;
    let t24_id = info.items[0].torrent_id.clone().unwrap();
    assert_eq!(
        fetcher.calls.lock().unwrap()[0],
        (
            "ubuntu-releases".to_owned(),
            "https://releases.example/24.04-desktop.torrent".to_owned()
        ),
        "the link is fetched as the source plugin"
    );
    let detail = core.detail(&k(&t24_id)).await.unwrap();
    assert_eq!(detail.summary.label.as_deref(), Some("ubuntu"));
    assert!(detail.summary.save_path.ends_with("ubuntu"));
    let prios: Vec<(String, u8)> = detail
        .files
        .iter()
        .map(|f| (f.path.clone(), f.priority))
        .collect();
    assert!(
        prios
            .iter()
            .all(|(p, prio)| (*prio > 0) == p.ends_with(".iso")),
        "only the ISO is wanted: {prios:?}"
    );

    // Once it's complete, the alias names it: a hard link on the same volume.
    let alias = cfg
        .download_dir
        .join("library/ubuntu/latest-desktop-amd64.iso");
    let info = settle(&core, &id, "the alias for 24.04", |i| {
        i.alias.as_ref().and_then(|a| a.torrent_id.clone()) == Some(t24_id.clone())
    })
    .await;
    assert_eq!(info.alias.unwrap().method, Some(AliasMethod::HardLink));
    assert_eq!(std::fs::read(&alias).unwrap(), iso24);

    // A dry run of the rule as if new: 26.04 would be taken, 24.04 is older.
    core.submit_candidates(
        None,
        "plugin:ubuntu-releases",
        vec![candidate("26.04", "desktop")],
    )
    .await
    .unwrap();
    let dry = core
        .dry_run(
            DEFAULT_SPACE,
            DryRunRequest {
                rule: rule(),
                rule_id: None,
            },
        )
        .await
        .unwrap();
    let verdicts: Vec<(Option<String>, DryRunVerdict)> = dry
        .entries
        .iter()
        .map(|e| (e.version.clone(), e.verdict))
        .collect();
    assert_eq!(
        verdicts,
        vec![
            (Some("26.04".into()), DryRunVerdict::Take),
            (Some("24.04".into()), DryRunVerdict::Older),
            (Some("24.10".into()), DryRunVerdict::Unusable),
        ]
    );
    assert_eq!(dry.considered, 4);

    // The rule takes 26.04 and supersedes 24.04, which gets the rule's seed limits.
    let info = settle(&core, &id, "26.04 taken", |i| active(i) == ["26.04"]).await;
    let old = info
        .items
        .iter()
        .find(|i| i.version.as_deref() == Some("24.04"))
        .unwrap();
    assert_eq!(old.state, RuleItemState::Superseded);
    let old_detail = core.detail(&k(&t24_id)).await.unwrap();
    assert_eq!(
        old_detail.seed_limits,
        Some(SeedLimits {
            ratio: 5.0,
            seed_minutes: 0,
            action: SeedLimitAction::Remove
        })
    );
    let t26_id = info
        .items
        .iter()
        .find(|i| i.version.as_deref() == Some("26.04"))
        .and_then(|i| i.torrent_id.clone())
        .unwrap();

    // The alias moves on once 26.04 is complete; the old file stays in its torrent.
    settle(&core, &id, "the alias for 26.04", |i| {
        i.alias.as_ref().and_then(|a| a.torrent_id.clone()) == Some(t26_id.clone())
    })
    .await;
    assert_eq!(std::fs::read(&alias).unwrap(), iso26);
    assert_eq!(
        core.store().get(&k(&t24_id)).map(|s| s.state),
        Some(TorrentState::Seeding)
    );

    // Removing the taken torrent by hand: the rule notes it and never takes it again.
    core.remove(&k(&t26_id), false).await.unwrap();
    let info = settle(&core, &id, "26.04 gone", |i| active(i).is_empty()).await;
    assert!(
        info.items
            .iter()
            .any(|i| i.version.as_deref() == Some("26.04") && i.state == RuleItemState::Gone)
    );
    core.run_rules().await;
    assert!(core.store().get(&k(&t26_id)).is_none());
    // The alias outlives the torrent it named.
    assert_eq!(std::fs::read(&alias).unwrap(), iso26);

    // Rules and what they took survive a restart.
    core.shutdown().await.unwrap();
    drop(core);
    let core = Core::start(cfg).await.unwrap();
    let info = core.rule(DEFAULT_SPACE, &id).await.unwrap();
    assert_eq!(info.items.len(), 2);
    assert_eq!(core.rules(DEFAULT_SPACE).await.unwrap().len(), 1);
    core.delete_rule(DEFAULT_SPACE, &id).await.unwrap();
    assert!(core.rule(DEFAULT_SPACE, &id).await.is_err());
    core.shutdown().await.unwrap();
}

/// A torrent in the default space.
#[allow(dead_code)]
fn k(id: &str) -> Key {
    Key::new(DEFAULT_SPACE, id)
}

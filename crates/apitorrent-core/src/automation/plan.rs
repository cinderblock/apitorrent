//! What a rule makes of the current candidates: which match, how they rank, and what
//! should happen to each. Pure: the dry run shows this plan, and a pass carries it out.

use std::collections::HashMap;

use apitorrent_types::automation::{
    DryRunVerdict, ReleaseCandidate, RuleItemState, RuleSpec, StoredCandidate, VersionField,
};

use super::matcher::{Files, Filter};
use super::version::{self, Key};

/// A rule with its patterns compiled.
#[derive(Debug, Clone)]
pub struct Compiled {
    pub spec: RuleSpec,
    pub filter: Filter,
    pub files: Files,
    /// Chooses the alias's file.
    pub alias_file: Option<super::matcher::FileGlob>,
}

impl Compiled {
    pub fn new(spec: &RuleSpec) -> Result<Self, String> {
        let alias_file = match spec.alias.as_ref().and_then(|a| a.file.as_deref()) {
            Some(g) if !g.trim().is_empty() => Some(super::matcher::FileGlob::compile(g)?),
            _ => None,
        };
        Ok(Self {
            filter: Filter::compile(&spec.filter)?,
            files: Files::compile(&spec.files)?,
            alias_file,
            spec: spec.clone(),
        })
    }
}

/// One matching candidate in the plan.
#[derive(Debug, Clone)]
pub struct Planned<'a> {
    pub stored: &'a StoredCandidate,
    /// 1 for the newest; `None` when unusable.
    pub rank: Option<u32>,
    pub verdict: DryRunVerdict,
    pub reason: Option<String>,
}

/// The text a candidate's version is read from.
fn version_text(c: &ReleaseCandidate, field: VersionField) -> Option<&str> {
    match field {
        VersionField::Version => c.version.as_deref(),
        VersionField::Title => Some(c.title.as_str()),
        VersionField::Published => c.published.as_deref(),
    }
    .filter(|t| !t.trim().is_empty())
}

fn usable(s: &StoredCandidate) -> bool {
    s.has_metainfo || s.candidate.links.torrent.is_some() || s.candidate.links.magnet.is_some()
}

/// Plan `rule` over `candidates`, given what it took before (by source and candidate id).
/// Matching candidates come back newest first, unusable ones last.
pub fn plan<'a>(
    rule: &Compiled,
    candidates: &'a [StoredCandidate],
    taken: &HashMap<(String, String), RuleItemState>,
) -> Vec<Planned<'a>> {
    let order = rule.spec.order;
    let mut ranked: Vec<(Key, Option<i64>, &'a StoredCandidate)> = Vec::new();
    let mut unusable: Vec<Planned<'a>> = Vec::new();
    for s in candidates {
        if !rule.filter.is_match(&s.source, &s.candidate) {
            continue;
        }
        let reason = if usable(s) {
            match version_text(&s.candidate, order.field) {
                None => Some(format!(
                    "it has no {}",
                    match order.field {
                        VersionField::Version => "version",
                        VersionField::Title => "title",
                        VersionField::Published => "publication date",
                    }
                )),
                Some(text) => match version::key(order.scheme, text) {
                    Ok(key) => {
                        let published = s
                            .candidate
                            .published
                            .as_deref()
                            .and_then(version::parse_date);
                        ranked.push((key, published, s));
                        None
                    }
                    Err(e) => Some(e),
                },
            }
        } else {
            Some("it has no .torrent or magnet link".to_owned())
        };
        if let Some(reason) = reason {
            unusable.push(Planned {
                stored: s,
                rank: None,
                verdict: DryRunVerdict::Unusable,
                reason: Some(reason),
            });
        }
    }
    // Newest first; equal versions by publication date, then by id, so the order is stable.
    ranked.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| b.2.candidate.id.cmp(&a.2.candidate.id))
            .then_with(|| b.2.source.cmp(&a.2.source))
    });
    let keep = rule.spec.keep_latest;
    let mut out: Vec<Planned<'a>> = ranked
        .into_iter()
        .enumerate()
        .map(|(i, (_, _, s))| {
            let rank = u32::try_from(i + 1).unwrap_or(u32::MAX);
            let within = keep == 0 || rank <= keep;
            let state = taken
                .get(&(s.source.clone(), s.candidate.id.clone()))
                .copied();
            let verdict = match (state, within) {
                (Some(RuleItemState::Gone), _) => DryRunVerdict::Gone,
                (Some(RuleItemState::Active), true) => DryRunVerdict::Keep,
                (Some(RuleItemState::Active), false) => DryRunVerdict::Supersede,
                // Superseded stays superseded, even if what replaced it disappears.
                (Some(RuleItemState::Superseded), _) => DryRunVerdict::Older,
                (Some(RuleItemState::Failed) | None, true) => DryRunVerdict::Take,
                (Some(RuleItemState::Failed) | None, false) => DryRunVerdict::Older,
            };
            Planned {
                stored: s,
                rank: Some(rank),
                verdict,
                reason: None,
            }
        })
        .collect();
    out.extend(unusable);
    out
}

/// Compare for display: rank first, unranked last.
#[cfg(test)]
fn by_rank(a: &Planned<'_>, b: &Planned<'_>) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a.rank, b.rank) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apitorrent_types::automation::{
        CandidateFilter, CandidateLinks, MatchMode, TextMatch, VersionOrder, VersionScheme,
    };

    fn stored(id: &str, version: &str, magnet: bool) -> StoredCandidate {
        StoredCandidate {
            source: "plugin:ubuntu-releases".into(),
            candidate: ReleaseCandidate {
                id: id.into(),
                title: format!("Ubuntu {version} desktop"),
                version: Some(version.into()),
                published: None,
                links: CandidateLinks {
                    magnet: magnet.then(|| "magnet:?xt=urn:btih:00".to_owned()),
                    ..CandidateLinks::default()
                },
                metadata: serde_json::Map::new(),
            },
            has_metainfo: false,
            first_seen: 0,
            last_seen: 0,
        }
    }

    fn rule(keep: u32) -> Compiled {
        Compiled::new(&RuleSpec {
            name: "ubuntu".into(),
            enabled: true,
            filter: CandidateFilter {
                title: Some(TextMatch {
                    mode: MatchMode::Contains,
                    pattern: "desktop".into(),
                }),
                ..CandidateFilter::default()
            },
            order: VersionOrder::default(),
            keep_latest: keep,
            label: None,
            save_path: None,
            files: apitorrent_types::automation::FileSelection::default(),
            superseded: apitorrent_types::automation::SupersededAction::default(),
            paused: false,
            alias: None,
        })
        .unwrap()
    }

    fn verdicts(p: &[Planned<'_>]) -> Vec<(String, DryRunVerdict)> {
        p.iter()
            .map(|x| (x.stored.candidate.id.clone(), x.verdict))
            .collect()
    }

    #[test]
    fn keeps_the_latest_n() {
        use DryRunVerdict::{Older, Take, Unusable};
        let cands = vec![
            stored("a", "24.04.1", true),
            stored("b", "26.04", true),
            stored("c", "25.10", true),
            stored("d", "not a version", true),
            stored("e", "27.04", false),
        ];
        let p = plan(&rule(2), &cands, &HashMap::new());
        assert_eq!(
            verdicts(&p),
            vec![
                ("b".into(), Take),
                ("c".into(), Take),
                ("a".into(), Older),
                ("d".into(), Unusable),
                ("e".into(), Unusable),
            ]
        );
        assert_eq!(p[0].rank, Some(1));
        assert!(p[4].reason.as_deref().unwrap().contains("link"));
    }

    #[test]
    fn a_newer_release_supersedes() {
        let cands = vec![stored("old", "24.04", true), stored("new", "26.04", true)];
        let taken: HashMap<_, _> = [(
            ("plugin:ubuntu-releases".to_owned(), "old".to_owned()),
            RuleItemState::Active,
        )]
        .into();
        let p = plan(&rule(1), &cands, &taken);
        assert_eq!(
            verdicts(&p),
            vec![
                ("new".into(), DryRunVerdict::Take),
                ("old".into(), DryRunVerdict::Supersede)
            ]
        );
        // Removed items are never taken again, and keep-all takes everything else.
        let taken: HashMap<_, _> = [(
            ("plugin:ubuntu-releases".to_owned(), "new".to_owned()),
            RuleItemState::Gone,
        )]
        .into();
        let p = plan(&rule(0), &cands, &taken);
        assert_eq!(
            verdicts(&p),
            vec![
                ("new".into(), DryRunVerdict::Gone),
                ("old".into(), DryRunVerdict::Take)
            ]
        );
    }

    #[test]
    fn dates_and_ties() {
        let mut r = rule(1);
        r.spec.order = VersionOrder {
            scheme: VersionScheme::Date,
            field: VersionField::Published,
        };
        let mut a = stored("a", "x", true);
        a.candidate.published = Some("2026-11-01T00:00:00Z".into());
        let mut b = stored("b", "x", true);
        b.candidate.published = Some("2026-11-08".into());
        let cands = vec![a, b];
        let p = plan(&r, &cands, &HashMap::new());
        assert_eq!(p[0].stored.candidate.id, "b");
        assert!(by_rank(&p[0], &p[1]).is_lt());
    }
}

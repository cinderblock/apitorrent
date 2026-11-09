//! Rules: storing them, the dry run, and the pass that carries out their plans (taking
//! candidates, applying file selections, superseding, and keeping aliases current).

use std::collections::HashMap;
use std::path::PathBuf;

use base64::Engine as _;

use apitorrent_metainfo::{Magnet, Torrent};
use apitorrent_types::automation::{
    AliasStatus, DryRun, DryRunEntry, DryRunRequest, DryRunVerdict, RuleInfo, RuleItem,
    RuleItemState, RuleSpec, StoredCandidate, SupersededAction,
};
use apitorrent_types::{AddSource, AddTorrentRequest, FilePriority, TorrentSummary};

use super::alias::{self, PlaceError};
use super::db::{AliasRow, ItemRow, RuleRow};
use super::plan::{Compiled, Planned, plan};
use super::{MAX_METAINFO, new_id, now_ms};
use crate::{Core, CoreError};

/// A failed take is tried again after this long.
const RETRY_FAILED_MS: i64 = 10 * 60 * 1000;
/// The most rules an instance keeps.
const MAX_RULES: usize = 500;

fn invalid(e: impl Into<String>) -> CoreError {
    CoreError::InvalidRequest(e.into())
}

/// What a take produced.
struct Taken {
    torrent_id: String,
    files_pending: bool,
}

impl Core {
    /// Check a rule before storing it.
    async fn check_rule(&self, spec: &RuleSpec, id: Option<&str>) -> Result<(), CoreError> {
        let name = spec.name.trim();
        if name.is_empty() || name.len() > 200 {
            return Err(invalid("a rule's name must be 1 to 200 characters"));
        }
        if spec.keep_latest > 1000 {
            return Err(invalid("keep at most the latest 1000 (0 keeps all)"));
        }
        if spec.label.as_ref().is_some_and(|l| l.len() > 100) {
            return Err(invalid("the label is longer than 100 characters"));
        }
        if let SupersededAction::SeedLimits { limits } = spec.superseded
            && (!limits.ratio.is_finite() || limits.ratio < 0.0)
        {
            return Err(invalid("the ratio must be a number, 0 or more"));
        }
        self.rule_save_path(spec.save_path.as_deref())
            .map_err(invalid)?;
        Compiled::new(spec).map_err(invalid)?;
        if let Some(a) = &spec.alias {
            let path = alias::resolve(self.download_dir(), &a.path).map_err(invalid)?;
            let path = path.to_string_lossy().into_owned();
            let me = id.map(str::to_owned);
            let taken = self
                .auto_db(move |db| {
                    for r in db.rules()? {
                        if Some(&r.id) == me.as_ref() {
                            continue;
                        }
                        if let Some(a) = db.alias(&r.id)?
                            && a.path == path
                        {
                            return Ok(Some(r.spec.name));
                        }
                    }
                    Ok(None)
                })
                .await?;
            if let Some(other) = taken {
                return Err(invalid(format!(
                    "the rule \"{other}\" already keeps an alias at that path"
                )));
            }
        }
        Ok(())
    }

    async fn rule_info(&self, row: RuleRow) -> Result<RuleInfo, CoreError> {
        let id = row.id.clone();
        let (items, alias) = self
            .auto_db(move |db| Ok((db.items(&id)?, db.alias(&id)?)))
            .await?;
        Ok(RuleInfo {
            id: row.id,
            alias: row.spec.alias.as_ref().map(|_| {
                let a = alias.unwrap_or_default();
                AliasStatus {
                    path: a.path,
                    torrent_id: a.torrent_id,
                    file: a.file,
                    method: a.method,
                    updated_at: a.updated_at,
                    error: a.error,
                }
            }),
            rule: row.spec,
            created_at: row.created_at,
            updated_at: row.updated_at,
            items: items.into_iter().map(|i| i.item).collect(),
            error: row.error,
        })
    }

    pub async fn rules(&self) -> Result<Vec<RuleInfo>, CoreError> {
        let rows = self.auto_db(|db| db.rules()).await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(self.rule_info(r).await?);
        }
        Ok(out)
    }

    pub async fn rule(&self, id: &str) -> Result<RuleInfo, CoreError> {
        let id = id.to_owned();
        let row = self
            .auto_db(move |db| db.rule(&id))
            .await?
            .ok_or(CoreError::NotFound)?;
        self.rule_info(row).await
    }

    /// Store a new rule; it runs straight away.
    pub async fn create_rule(&self, spec: RuleSpec) -> Result<RuleInfo, CoreError> {
        self.check_rule(&spec, None).await?;
        let id = new_id();
        let (id2, spec2) = (id.clone(), spec.clone());
        let alias_path = self.alias_path(&spec);
        self.auto_db(move |db| {
            if db.rules()?.len() >= MAX_RULES {
                return Ok(false);
            }
            db.insert_rule(&id2, &spec2, now_ms())?;
            if let Some(path) = alias_path {
                db.save_alias(
                    &id2,
                    &AliasRow {
                        path,
                        ..AliasRow::default()
                    },
                )?;
            }
            Ok(true)
        })
        .await?
        .then_some(())
        .ok_or_else(|| invalid(format!("at most {MAX_RULES} rules")))?;
        self.auto().wake.notify_one();
        self.rule(&id).await
    }

    fn alias_path(&self, spec: &RuleSpec) -> Option<String> {
        let a = spec.alias.as_ref()?;
        alias::resolve(self.download_dir(), &a.path)
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    }

    /// Replace a rule. Torrents it already took stay; the next pass applies the new rule.
    pub async fn update_rule(&self, id: &str, spec: RuleSpec) -> Result<RuleInfo, CoreError> {
        self.check_rule(&spec, Some(id)).await?;
        let (id2, spec2) = (id.to_owned(), spec.clone());
        let alias_path = self.alias_path(&spec);
        let found = self
            .auto_db(move |db| {
                if !db.update_rule(&id2, &spec2, now_ms())? {
                    return Ok(false);
                }
                match alias_path {
                    None => db.delete_alias(&id2)?,
                    Some(path) => {
                        let mut a = db.alias(&id2)?.unwrap_or_default();
                        if a.path != path {
                            // A new path starts afresh; the old file is left where it is.
                            a = AliasRow {
                                path,
                                ..AliasRow::default()
                            };
                            db.save_alias(&id2, &a)?;
                        }
                    }
                }
                Ok(true)
            })
            .await?;
        if !found {
            return Err(CoreError::NotFound);
        }
        self.auto().wake.notify_one();
        self.rule(id).await
    }

    /// Delete a rule. Its torrents and its alias file stay.
    pub async fn delete_rule(&self, id: &str) -> Result<(), CoreError> {
        let id = id.to_owned();
        if self.auto_db(move |db| db.delete_rule(&id)).await? {
            Ok(())
        } else {
            Err(CoreError::NotFound)
        }
    }

    /// Run every rule now, and wait for the pass to finish.
    pub async fn run_rules(&self) {
        self.rules_pass().await;
    }

    /// What `req.rule` would do with the current candidates. Changes nothing.
    pub async fn dry_run(&self, req: DryRunRequest) -> Result<DryRun, CoreError> {
        let compiled = Compiled::new(&req.rule).map_err(invalid)?;
        let rule_id = req.rule_id.clone();
        let (candidates, items) = self
            .auto_db(move |db| {
                let items = match &rule_id {
                    Some(id) => db.items(id)?,
                    None => Vec::new(),
                };
                Ok((db.candidates(None)?, items))
            })
            .await?;
        let taken = taken_map(&items);
        let considered = candidates
            .iter()
            .filter(|c| compiled.filter.takes_source(&c.source))
            .count();
        let planned = plan(&compiled, &candidates, &taken);
        Ok(DryRun {
            entries: planned.iter().map(dry_entry).collect(),
            considered: u32::try_from(considered).unwrap_or(u32::MAX),
        })
    }

    /// One pass over every enabled rule.
    pub(crate) async fn rules_pass(&self) {
        let _pass = self.auto().pass.lock().await;
        let loaded = self
            .auto_db(|db| {
                db.prune_candidates(now_ms() - super::CANDIDATE_TTL_MS)?;
                Ok((db.rules()?, db.candidates(None)?))
            })
            .await;
        let (rules, candidates) = match loaded {
            Ok(x) => x,
            Err(e) => {
                tracing::error!(error = %e, "rules: could not read automation.db");
                return;
            }
        };
        for rule in rules.into_iter().filter(|r| r.spec.enabled) {
            if let Err(e) = self.run_rule(&rule, &candidates).await {
                tracing::warn!(rule = %rule.id, error = %e, "rule pass failed");
            }
        }
    }

    /// Between passes: apply file selections waiting for metadata, and update aliases.
    pub(crate) async fn light_pass(&self) {
        let _pass = self.auto().pass.lock().await;
        let Ok(rules) = self.auto_db(|db| db.rules()).await else {
            return;
        };
        for rule in rules.into_iter().filter(|r| r.spec.enabled) {
            let Ok(compiled) = Compiled::new(&rule.spec) else {
                continue;
            };
            let id = rule.id.clone();
            let Ok(mut items) = self.auto_db(move |db| db.items(&id)).await else {
                continue;
            };
            let summaries = self.summaries();
            self.settle_items(&rule.id, &compiled, &mut items, &summaries)
                .await;
            self.update_alias(&rule.id, &compiled, &items, &summaries)
                .await;
        }
    }

    fn summaries(&self) -> HashMap<String, TorrentSummary> {
        self.list().into_iter().map(|s| (s.id.clone(), s)).collect()
    }

    async fn run_rule(
        &self,
        rule: &RuleRow,
        candidates: &[StoredCandidate],
    ) -> Result<(), CoreError> {
        let compiled = match Compiled::new(&rule.spec) {
            Ok(c) => c,
            Err(e) => {
                let id = rule.id.clone();
                self.auto_db(move |db| db.set_rule_error(&id, Some(&e)))
                    .await?;
                return Ok(());
            }
        };
        let id = rule.id.clone();
        let mut items = self.auto_db(move |db| db.items(&id)).await?;
        let summaries = self.summaries();
        // Torrents removed since the last pass are gone for good.
        let now = now_ms();
        for row in &mut items {
            let live = matches!(
                row.item.state,
                RuleItemState::Active | RuleItemState::Superseded
            );
            if live
                && row
                    .item
                    .torrent_id
                    .as_ref()
                    .is_some_and(|t| !summaries.contains_key(t))
            {
                row.item.state = RuleItemState::Gone;
                row.updated_at = now;
                row.files_pending = false;
                self.save_item(&rule.id, row).await?;
            }
        }
        let taken = taken_map(&items);
        let planned = plan(&compiled, candidates, &taken);
        let mut by_key: HashMap<(String, String), ItemRow> = items
            .into_iter()
            .map(|r| ((r.item.source.clone(), r.item.candidate_id.clone()), r))
            .collect();
        // Ranks are refreshed for everything; items that stopped matching lose theirs.
        for row in by_key.values_mut() {
            row.rank = None;
        }
        let mut errors = Vec::new();
        for p in &planned {
            let key = (p.stored.source.clone(), p.stored.candidate.id.clone());
            let c = &p.stored.candidate;
            let existing = by_key.remove(&key);
            let mut row = existing.unwrap_or_else(|| ItemRow {
                item: RuleItem {
                    source: p.stored.source.clone(),
                    candidate_id: c.id.clone(),
                    title: c.title.clone(),
                    version: c.version.clone(),
                    torrent_id: None,
                    state: RuleItemState::Failed,
                    error: None,
                    taken_at: now,
                },
                updated_at: 0,
                rank: None,
                files_pending: false,
            });
            let is_new = row.updated_at == 0;
            row.rank = p.rank;
            match p.verdict {
                DryRunVerdict::Take => {
                    if row.item.state == RuleItemState::Failed
                        && !is_new
                        && now - row.updated_at < RETRY_FAILED_MS
                    {
                        self.save_item(&rule.id, &row).await?;
                        continue;
                    }
                    match self.take(&compiled, p.stored).await {
                        Ok(t) => {
                            tracing::info!(rule = %rule.spec.name, candidate = %c.title,
                                torrent = %t.torrent_id, "rule took a release");
                            row.item.torrent_id = Some(t.torrent_id);
                            row.item.state = RuleItemState::Active;
                            row.item.error = None;
                            row.item.taken_at = now;
                            row.files_pending = t.files_pending;
                        }
                        Err(e) => {
                            tracing::warn!(rule = %rule.spec.name, candidate = %c.title,
                                error = %e, "rule could not take a release");
                            errors.push(format!("{}: {e}", c.title));
                            row.item.state = RuleItemState::Failed;
                            row.item.error = Some(e);
                        }
                    }
                    row.updated_at = now;
                    self.save_item(&rule.id, &row).await?;
                }
                DryRunVerdict::Supersede => {
                    self.supersede(&rule.spec, &mut row).await;
                    row.updated_at = now;
                    self.save_item(&rule.id, &row).await?;
                }
                // Untaken candidates leave no trace; taken ones get their new rank.
                _ if !is_new => self.save_item(&rule.id, &row).await?,
                _ => {}
            }
        }
        // Items whose candidates no longer match keep their state, unranked.
        let rest: Vec<ItemRow> = by_key.into_values().collect();
        for row in &rest {
            self.save_item(&rule.id, row).await?;
        }
        let id = rule.id.clone();
        let error = (!errors.is_empty()).then(|| errors.join("; "));
        self.auto_db(move |db| db.set_rule_error(&id, error.as_deref()))
            .await?;
        let id = rule.id.clone();
        let mut items = self.auto_db(move |db| db.items(&id)).await?;
        self.settle_items(&rule.id, &compiled, &mut items, &summaries)
            .await;
        self.update_alias(&rule.id, &compiled, &items, &summaries)
            .await;
        Ok(())
    }

    async fn save_item(&self, rule_id: &str, row: &ItemRow) -> Result<(), CoreError> {
        let (rule_id, row) = (rule_id.to_owned(), row.clone());
        self.auto_db(move |db| db.save_item(&rule_id, &row)).await
    }

    /// Add a candidate's torrent as the rule says.
    async fn take(&self, rule: &Compiled, s: &StoredCandidate) -> Result<Taken, String> {
        let spec = &rule.spec;
        let c = &s.candidate;
        let mut metainfo = if s.has_metainfo {
            let (src, id) = (s.source.clone(), c.id.clone());
            self.auto_db(move |db| db.candidate_metainfo(&src, &id))
                .await
                .map_err(|e| e.to_string())?
        } else {
            None
        };
        // A `.torrent` is preferred over a magnet link: its files are known at once, so the
        // file selection applies when it is added. If fetching fails, the magnet will do.
        if metainfo.is_none()
            && let Some(url) = &c.links.torrent
        {
            match self.fetch_torrent(&s.source, url).await {
                Ok(bytes) => metainfo = Some(bytes),
                Err(e) if c.links.magnet.is_some() => {
                    tracing::info!(candidate = %c.title, error = %e,
                        "using the magnet link instead of the .torrent");
                }
                Err(e) => return Err(e),
            }
        }
        let save_path = self
            .rule_save_path(spec.save_path.as_deref())?
            .map(|p| p.to_string_lossy().into_owned());
        let (source, priorities, files_pending, hashes) = if let Some(bytes) = metainfo {
            {
                let t = Torrent::from_bytes(&bytes).map_err(|e| format!("bad .torrent: {e}"))?;
                let priorities = if rule.files.selects_all() {
                    None
                } else {
                    let p: Vec<u8> = t
                        .info
                        .files
                        .iter()
                        .map(|f| {
                            if !f.is_pad() && rule.files.wants(&f.display_path()) {
                                4
                            } else {
                                0
                            }
                        })
                        .collect();
                    if p.iter().all(|&x| x == 0) {
                        return Err("the file selection matches none of its files".into());
                    }
                    Some(p)
                };
                (
                    AddSource::TorrentFile {
                        data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                    },
                    priorities,
                    false,
                    t.info_hashes(),
                )
            }
        } else {
            {
                let uri = c
                    .links
                    .magnet
                    .as_deref()
                    .ok_or("it has no .torrent or magnet link")?;
                let mut magnet = Magnet::parse(uri).map_err(|e| e.to_string())?;
                if let Some(ws) = &c.links.webseed
                    && !magnet.web_seeds.contains(ws)
                {
                    magnet.web_seeds.push(ws.clone());
                }
                let hashes = magnet.info_hashes;
                (
                    AddSource::Magnet {
                        uri: magnet.to_uri(),
                    },
                    None,
                    !rule.files.selects_all(),
                    hashes,
                )
            }
        };
        let req = AddTorrentRequest {
            source,
            save_path,
            paused: spec.paused,
            label: spec.label.clone().filter(|l| !l.trim().is_empty()),
            file_priorities: priorities,
            unwanted_policy: apitorrent_types::UnwantedPolicy::default(),
        };
        match self.add(req).await {
            Ok(id) => Ok(Taken {
                torrent_id: id,
                files_pending,
            }),
            // Already here (added by hand, or by another rule): the rule adopts it.
            Err(CoreError::Duplicate) => {
                let v1 = hashes.get_v1().map(|h| h.to_hex());
                let v2 = hashes.get_v2().map(|h| h.to_hex());
                self.list()
                    .into_iter()
                    .find(|t| {
                        (v1.is_some() && t.info_hash_v1 == v1)
                            || (v2.is_some() && t.info_hash_v2 == v2)
                            || t.id == hashes.id_string()
                    })
                    .map(|t| Taken {
                        torrent_id: t.id,
                        files_pending: false,
                    })
                    .ok_or_else(|| "it is already added, but couldn't be found".to_owned())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// Fetch a candidate's `.torrent` link as its source plugin.
    async fn fetch_torrent(&self, source: &str, url: &str) -> Result<Vec<u8>, String> {
        let Some(plugin) = source.strip_prefix("plugin:") else {
            return Err(format!("links from {source} can't be fetched"));
        };
        let fetcher = self
            .source_fetcher()
            .ok_or("plugins are off, so the .torrent link can't be fetched")?;
        fetcher
            .fetch_as(plugin, url, MAX_METAINFO)
            .await
            .map_err(|e| format!("fetching {url}: {e}"))
    }

    /// Apply the rule's superseded action to an item that left the latest N.
    async fn supersede(&self, spec: &RuleSpec, row: &mut ItemRow) {
        let Some(id) = row.item.torrent_id.clone() else {
            row.item.state = RuleItemState::Gone;
            return;
        };
        let result = match spec.superseded {
            SupersededAction::KeepSeeding => Ok(()),
            SupersededAction::Stop => self.pause(&id).await,
            SupersededAction::SeedLimits { limits } => {
                self.set_seed_limits(&id, Some(limits)).await
            }
        };
        match result {
            Ok(()) => {
                tracing::info!(rule = %spec.name, torrent = %id, "superseded by a newer release");
                row.item.state = RuleItemState::Superseded;
                row.item.error = None;
            }
            Err(CoreError::NotFound) => row.item.state = RuleItemState::Gone,
            Err(e) => {
                // Still superseded: the action is not retried, but the reason shows.
                row.item.state = RuleItemState::Superseded;
                row.item.error = Some(e.to_string());
            }
        }
    }

    /// Apply file selections to magnet links whose metadata has arrived.
    async fn settle_items(
        &self,
        rule_id: &str,
        rule: &Compiled,
        items: &mut [ItemRow],
        summaries: &HashMap<String, TorrentSummary>,
    ) {
        for row in items.iter_mut().filter(|r| r.files_pending) {
            let Some(id) = row.item.torrent_id.clone() else {
                continue;
            };
            if !summaries.contains_key(&id) {
                continue;
            }
            let Ok(detail) = self.detail(&id).await else {
                continue;
            };
            if detail.files.is_empty() {
                continue; // Still fetching the metadata.
            }
            let updates: Vec<FilePriority> = detail
                .files
                .iter()
                .map(|f| FilePriority {
                    index: f.index,
                    priority: if rule.files.wants(&f.path) { 4 } else { 0 },
                })
                .collect();
            if updates.iter().all(|u| u.priority == 0) {
                row.item.error = Some("the file selection matches none of its files".into());
            } else if let Err(e) = self.set_file_priorities(&id, &updates).await {
                row.item.error = Some(format!("selecting files: {e}"));
            }
            row.files_pending = false;
            row.updated_at = now_ms();
            if let Err(e) = self.save_item(rule_id, row).await {
                tracing::warn!(error = %e, "could not save a rule item");
            }
        }
    }

    /// Point the rule's alias at its newest completed release, if that changed.
    async fn update_alias(
        &self,
        rule_id: &str,
        rule: &Compiled,
        items: &[ItemRow],
        summaries: &HashMap<String, TorrentSummary>,
    ) {
        let Some(spec) = &rule.spec.alias else {
            return;
        };
        let Ok(path) = alias::resolve(self.download_dir(), &spec.path) else {
            return;
        };
        let path_str = path.to_string_lossy().into_owned();
        let rid = rule_id.to_owned();
        let Ok(current) = self.auto_db(move |db| db.alias(&rid)).await else {
            return;
        };
        let mut row = current.unwrap_or_default();
        if row.path != path_str {
            row = AliasRow {
                path: path_str.clone(),
                ..AliasRow::default()
            };
        }
        let rank_of = |source: &Option<String>, cid: &Option<String>| {
            items
                .iter()
                .find(|i| {
                    Some(&i.item.source) == source.as_ref()
                        && Some(&i.item.candidate_id) == cid.as_ref()
                })
                .and_then(|i| i.rank)
        };
        let current_rank = rank_of(&row.source, &row.candidate_id);
        // The newest active release that is complete.
        let mut ranked: Vec<&ItemRow> = items
            .iter()
            .filter(|i| i.item.state == RuleItemState::Active && i.rank.is_some())
            .collect();
        ranked.sort_by_key(|i| i.rank);
        for item in ranked {
            if current_rank.is_some_and(|r| item.rank.is_some_and(|n| n >= r)) && path.exists() {
                return; // The alias already names this release or a newer one.
            }
            let Some(tid) = &item.item.torrent_id else {
                continue;
            };
            if !summaries.get(tid).is_some_and(|s| s.progress >= 1.0) {
                continue;
            }
            let Some((index, file)) = self.alias_file(tid, rule).await else {
                continue;
            };
            let Some(target) = self.file_disk_path(tid, index) else {
                continue;
            };
            self.place_alias(rule_id, &mut row, &target, &path, item, tid, file)
                .await;
            return;
        }
    }

    /// The file an alias names in torrent `id`: the largest wanted, complete file that
    /// matches the alias's glob.
    async fn alias_file(&self, id: &str, rule: &Compiled) -> Option<(usize, String)> {
        let detail = self.detail(id).await.ok()?;
        detail
            .files
            .into_iter()
            .filter(|f| f.priority > 0 && f.progress >= 1.0)
            .filter(|f| f.placement == apitorrent_types::FilePlacement::Save)
            .filter(|f| rule.alias_file.as_ref().is_none_or(|g| g.is_match(&f.path)))
            .max_by_key(|f| f.length)
            .map(|f| (f.index as usize, f.path))
    }

    #[allow(clippy::too_many_arguments)]
    async fn place_alias(
        &self,
        rule_id: &str,
        row: &mut AliasRow,
        target: &std::path::Path,
        path: &std::path::Path,
        item: &ItemRow,
        torrent_id: &str,
        file: String,
    ) {
        let path_str = path.to_string_lossy().into_owned();
        let p = path_str.clone();
        let owned = self
            .auto_db(move |db| db.alias_owns(&p))
            .await
            .unwrap_or(false);
        let (t, a): (PathBuf, PathBuf) = (target.to_path_buf(), path.to_path_buf());
        let placed = tokio::task::spawn_blocking(move || alias::place(&t, &a, owned))
            .await
            .expect("alias task panicked");
        match placed {
            Ok(method) => {
                tracing::info!(alias = %path.display(), target = %target.display(),
                    ?method, "alias updated");
                *row = AliasRow {
                    path: path_str,
                    torrent_id: Some(torrent_id.to_owned()),
                    file: Some(file),
                    source: Some(item.item.source.clone()),
                    candidate_id: Some(item.item.candidate_id.clone()),
                    method: Some(method),
                    updated_at: Some(now_ms()),
                    error: None,
                };
            }
            Err(e) => {
                if !matches!(e, PlaceError::InUse) {
                    tracing::warn!(alias = %path.display(), error = %e, "alias not updated");
                }
                row.error = Some(e.to_string());
            }
        }
        let (rid, row) = (rule_id.to_owned(), row.clone());
        if let Err(e) = self.auto_db(move |db| db.save_alias(&rid, &row)).await {
            tracing::warn!(error = %e, "could not save the alias");
        }
    }
}

fn taken_map(items: &[ItemRow]) -> HashMap<(String, String), RuleItemState> {
    items
        .iter()
        .map(|r| {
            (
                (r.item.source.clone(), r.item.candidate_id.clone()),
                r.item.state,
            )
        })
        .collect()
}

fn dry_entry(p: &Planned<'_>) -> DryRunEntry {
    let c = &p.stored.candidate;
    DryRunEntry {
        source: p.stored.source.clone(),
        candidate_id: c.id.clone(),
        title: c.title.clone(),
        version: c.version.clone(),
        published: c.published.clone(),
        rank: p.rank,
        verdict: p.verdict,
        reason: p.reason.clone(),
    }
}

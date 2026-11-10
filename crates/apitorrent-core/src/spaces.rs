//! Spaces: named partitions of the instance, each with its own storage, limits, labels,
//! automation and members (members live in `auth.db`; see `apitorrent-auth`).
//!
//! A space's torrents are keyed by `(space, torrent id)` (see [`crate::Key`]). The same
//! torrent in two spaces is one swarm in the engine with one entry per space; see
//! `Core::spawn_entry` and `Core::place_secondaries`.

use std::path::{Component, Path, PathBuf};

use apitorrent_engine::limits::GroupLimits;
use apitorrent_types::QueueSettings;
use apitorrent_types::spaces::{
    CreateSpaceRequest, DEFAULT_SPACE, Space, SpaceId, SpaceSettings, UpdateSpaceRequest,
    valid_slug,
};

use crate::{Core, CoreError};

/// A space's rates and connections, as engine limits.
pub fn engine_limits(s: &SpaceSettings) -> GroupLimits {
    GroupLimits {
        download_rate: s.download_rate,
        upload_rate: s.upload_rate,
        max_connections: s.max_connections as usize,
    }
}

/// The queue limits a space's torrents are planned with first (the instance's apply
/// across all spaces after that).
pub fn queue_settings(s: &SpaceSettings, instance: &QueueSettings) -> QueueSettings {
    QueueSettings {
        max_active: s.max_active,
        max_active_downloads: s.max_active_downloads,
        max_active_seeds: s.max_active_seeds,
        ..*instance
    }
}

pub fn queue_limited(s: &SpaceSettings) -> bool {
    s.max_active > 0 || s.max_active_downloads > 0 || s.max_active_seeds > 0
}

/// A path without `..` or `.` parts, so a prefix check means what it says.
fn plain(path: &Path) -> bool {
    path.components()
        .all(|c| !matches!(c, Component::ParentDir | Component::CurDir))
}

fn validate(s: &SpaceSettings) -> Result<(), CoreError> {
    let absolute = |p: &str, what: &str| {
        let path = Path::new(p);
        if path.is_absolute() && plain(path) {
            Ok(())
        } else {
            Err(CoreError::InvalidRequest(format!(
                "{what} must be an absolute path without `..`: {p}"
            )))
        }
    };
    for r in &s.storage_roots {
        absolute(r, "a storage root")?;
    }
    if let Some(d) = &s.download_dir {
        absolute(d, "the download folder")?;
        if !within_roots(&s.storage_roots, Path::new(d)) {
            return Err(CoreError::InvalidRequest(
                "the download folder must be inside one of the space's storage roots".into(),
            ));
        }
    }
    let mut labels = std::collections::HashSet::new();
    for r in &s.folder_rules {
        if r.label.trim().is_empty() || r.folder.trim().is_empty() {
            return Err(CoreError::InvalidRequest(
                "a folder rule needs a label and a folder".into(),
            ));
        }
        if !labels.insert(r.label.as_str()) {
            return Err(CoreError::InvalidRequest(format!(
                "label {:?} has two folder rules",
                r.label
            )));
        }
        if !plain(Path::new(&r.folder)) {
            return Err(CoreError::InvalidRequest(format!(
                "folder {:?} must not contain `..`",
                r.folder
            )));
        }
    }
    Ok(())
}

/// Whether `path` is inside one of `roots` (any path, when there are none).
fn within_roots(roots: &[String], path: &Path) -> bool {
    roots.is_empty() || roots.iter().any(|r| path.starts_with(r))
}

impl Core {
    /// Every space, by id.
    pub fn spaces(&self) -> Vec<Space> {
        self.inner
            .spaces
            .read()
            .expect("spaces lock poisoned")
            .values()
            .cloned()
            .collect()
    }

    pub fn space(&self, id: SpaceId) -> Result<Space, CoreError> {
        self.inner
            .spaces
            .read()
            .expect("spaces lock poisoned")
            .get(&id)
            .cloned()
            .ok_or(CoreError::NotFound)
    }

    pub fn space_by_slug(&self, slug: &str) -> Option<Space> {
        self.inner
            .spaces
            .read()
            .expect("spaces lock poisoned")
            .values()
            .find(|s| s.slug == slug)
            .cloned()
    }

    pub async fn create_space(&self, req: CreateSpaceRequest) -> Result<Space, CoreError> {
        let slug = req.slug.trim().to_owned();
        if !valid_slug(&slug) {
            return Err(CoreError::InvalidRequest(
                "a space's slug is 1 to 40 lowercase letters, digits and dashes".into(),
            ));
        }
        let name = req.name.trim().to_owned();
        if name.is_empty() {
            return Err(CoreError::InvalidRequest("a space needs a name".into()));
        }
        validate(&req.settings)?;
        if self.space_by_slug(&slug).is_some() {
            return Err(CoreError::Duplicate);
        }
        let (s, n, d, settings) = (
            slug.clone(),
            name.clone(),
            req.description.clone(),
            req.settings.clone(),
        );
        let id = self
            .db(move |db| db.insert_space(&s, &n, &d, &settings, crate::now_ms()))
            .await?;
        let space = Space {
            id,
            slug,
            name,
            description: req.description,
            settings: req.settings,
        };
        self.inner
            .spaces
            .write()
            .expect("spaces lock poisoned")
            .insert(id, space.clone());
        self.apply_space_limits();
        tracing::info!(space = %space.slug, "space created");
        Ok(space)
    }

    pub async fn update_space(
        &self,
        id: SpaceId,
        req: UpdateSpaceRequest,
    ) -> Result<Space, CoreError> {
        let mut space = self.space(id)?;
        let name = req.name.trim().to_owned();
        if name.is_empty() {
            return Err(CoreError::InvalidRequest("a space needs a name".into()));
        }
        validate(&req.settings)?;
        let (n, d, settings) = (name.clone(), req.description.clone(), req.settings.clone());
        self.db(move |db| db.update_space(id, &n, &d, &settings))
            .await?;
        space.name = name;
        space.description = req.description;
        space.settings = req.settings;
        self.inner
            .spaces
            .write()
            .expect("spaces lock poisoned")
            .insert(id, space.clone());
        self.apply_space_limits();
        self.apply_queue().await;
        self.refresh();
        Ok(space)
    }

    /// Delete an empty space. The `default` space and spaces with torrents stay.
    pub async fn delete_space(&self, id: SpaceId) -> Result<(), CoreError> {
        self.space(id)?;
        if id == DEFAULT_SPACE {
            return Err(CoreError::InvalidRequest(
                "the default space can't be deleted".into(),
            ));
        }
        if self.inner.automation.space_in_use(id)? {
            return Err(CoreError::InvalidRequest(
                "the space still has rules, channels or subscriptions".into(),
            ));
        }
        if !self.db(move |db| db.delete_space(id)).await? {
            return Err(CoreError::InvalidRequest(
                "the space still has torrents; remove them first".into(),
            ));
        }
        self.inner
            .spaces
            .write()
            .expect("spaces lock poisoned")
            .remove(&id);
        self.inner.session.remove_space(&crate::engine_space(id));
        Ok(())
    }

    /// Where a space's new torrents go by default: its download folder; for `default`,
    /// the instance's download directory; else its first storage root, or a folder named
    /// after it in the instance's download directory.
    pub fn download_dir(&self, space: SpaceId) -> PathBuf {
        let instance = &self.inner.config.download_dir;
        let Ok(s) = self.space(space) else {
            return instance.clone();
        };
        if let Some(d) = &s.settings.download_dir {
            return PathBuf::from(d);
        }
        if space == DEFAULT_SPACE {
            return instance.clone();
        }
        s.settings
            .storage_roots
            .first()
            .map_or_else(|| instance.join(&s.slug), PathBuf::from)
    }

    /// The save path for a new torrent: the one asked for, else the folder its label's rule
    /// names, else the space's download folder. It must be inside the space's storage roots.
    pub fn resolve_save_path(
        &self,
        space: SpaceId,
        requested: Option<&str>,
        label: Option<&str>,
    ) -> Result<PathBuf, CoreError> {
        let s = self.space(space)?;
        let base = self.download_dir(space);
        let path = match requested {
            Some(p) => {
                let p = PathBuf::from(p);
                if !p.is_absolute() {
                    return Err(CoreError::InvalidRequest(
                        "save path must be absolute".into(),
                    ));
                }
                p
            }
            None => label
                .and_then(|l| s.settings.folder_rules.iter().find(|r| r.label == l))
                .map_or(base.clone(), |r| base.join(&r.folder)),
        };
        self.check_save_path(space, &path)?;
        Ok(path)
    }

    /// The spaces whose entries share a torrent's swarm, its primary (whose files the
    /// engine uses) first; empty when no space runs it.
    pub fn swarm_spaces(&self, id: &str) -> Vec<SpaceId> {
        let Some(primary) = self
            .inner
            .swarms
            .lock()
            .expect("swarms lock poisoned")
            .get(id)
            .map(|s| s.primary)
        else {
            return Vec::new();
        };
        let mut others: Vec<SpaceId> = self
            .inner
            .torrents
            .read()
            .expect("torrents lock poisoned")
            .keys()
            .filter(|k| k.id == id && k.space != primary)
            .map(|k| k.space)
            .collect();
        others.sort_unstable();
        std::iter::once(primary).chain(others).collect()
    }

    /// Refuse a save path outside the space's storage roots.
    pub fn check_save_path(&self, space: SpaceId, path: &Path) -> Result<(), CoreError> {
        let s = self.space(space)?;
        if !plain(path) {
            return Err(CoreError::InvalidRequest(
                "save path must not contain `..`".into(),
            ));
        }
        if !within_roots(&s.settings.storage_roots, path) {
            return Err(CoreError::InvalidRequest(format!(
                "{} is outside space {}'s storage roots",
                path.display(),
                s.slug
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apitorrent_types::spaces::FolderRule;

    fn abs(p: &str) -> String {
        if cfg!(windows) {
            format!("C:\\{}", p.replace('/', "\\"))
        } else {
            format!("/{p}")
        }
    }

    #[test]
    fn settings_are_checked() {
        let ok = SpaceSettings {
            storage_roots: vec![abs("tv")],
            download_dir: Some(abs("tv/new")),
            folder_rules: vec![FolderRule {
                label: "x".into(),
                folder: "x".into(),
            }],
            ..SpaceSettings::default()
        };
        assert!(validate(&ok).is_ok());
        for bad in [
            SpaceSettings {
                storage_roots: vec!["relative".into()],
                ..SpaceSettings::default()
            },
            SpaceSettings {
                storage_roots: vec![abs("tv")],
                download_dir: Some(abs("movies")),
                ..SpaceSettings::default()
            },
            SpaceSettings {
                folder_rules: vec![FolderRule {
                    label: "x".into(),
                    folder: "../escape".into(),
                }],
                ..SpaceSettings::default()
            },
        ] {
            assert!(validate(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn roots_contain_paths() {
        let roots = vec![abs("tv")];
        assert!(within_roots(&roots, Path::new(&abs("tv/show"))));
        assert!(!within_roots(&roots, Path::new(&abs("tvx/show"))));
        assert!(within_roots(&[], Path::new(&abs("anything"))));
    }
}

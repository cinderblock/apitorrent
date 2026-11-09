//! Storage roots and their marker files.
//!
//! A root is a directory the instance downloads into. For now that is the download
//! directory; per-root settings come with the content library's roots. Each root gets a
//! random id, stored per path, and a `.apitorrent-root` marker holding it. A desktop client
//! that maps the root to a local path (a network share) reads the marker through that path
//! before it trusts the mapping, so "open folder" never lands in the wrong place.

use std::path::Path;

use apitorrent_types::{RootMarker, StorageRoot};

use crate::db::Database;

fn meta_key(path: &str) -> String {
    format!("storage_root:{path}")
}

/// The root at `dir`, with the id made the first time `dir` was a root. A root moved to a
/// new path is a new root with a new id, so a mapping to the old location stops verifying.
pub(crate) fn root_for(db: &Database, dir: &Path) -> rusqlite::Result<StorageRoot> {
    let path = std::path::absolute(dir)
        .unwrap_or_else(|_| dir.to_path_buf())
        .display()
        .to_string();
    let key = meta_key(&path);
    let id = if let Some(id) = db.meta(&key)? {
        id
    } else {
        let bytes: [u8; 16] = rand::random();
        let id = bytes.iter().fold(String::with_capacity(32), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        });
        db.set_meta(&key, &id)?;
        id
    };
    Ok(StorageRoot { id, path })
}

/// Write the root's marker, unless it already holds this id. A failure is only logged: a
/// read-only download directory shouldn't stop the instance; mappings to it just won't
/// verify.
pub(crate) fn write_marker(root: &StorageRoot) {
    let dir = Path::new(&root.path);
    let marker = dir.join(RootMarker::FILE_NAME);
    if std::fs::read(&marker)
        .ok()
        .and_then(|b| RootMarker::parse(&b))
        .is_some_and(|m| m.id == root.id)
    {
        return;
    }
    let json =
        serde_json::to_vec_pretty(&RootMarker::new(&root.id)).expect("a marker always serialises");
    // Write then rename, so a client never reads half a marker.
    let tmp = dir.join(format!("{}.tmp", RootMarker::FILE_NAME));
    let result = std::fs::write(&tmp, &json).and_then(|()| std::fs::rename(&tmp, &marker));
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        tracing::warn!(
            path = %marker.display(),
            error = %e,
            "could not write the storage root marker; desktop path mappings to this root \
             can't be verified"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_per_path_and_markers_are_written() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open(&tmp.path().join("db")).unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        let ra = root_for(&db, &a).unwrap();
        assert_eq!(root_for(&db, &a).unwrap(), ra);
        let rb = root_for(&db, &b).unwrap();
        assert_ne!(ra.id, rb.id);
        assert_eq!(ra.id.len(), 32);

        write_marker(&ra);
        let marker = std::fs::read(a.join(RootMarker::FILE_NAME)).unwrap();
        assert_eq!(RootMarker::parse(&marker).unwrap().id, ra.id);
        // A wrong marker (a copied directory, say) is corrected.
        std::fs::write(a.join(RootMarker::FILE_NAME), b"{}").unwrap();
        write_marker(&ra);
        let marker = std::fs::read(a.join(RootMarker::FILE_NAME)).unwrap();
        assert_eq!(RootMarker::parse(&marker).unwrap().id, ra.id);
        assert!(!a.join(".apitorrent-root.tmp").exists());
    }
}

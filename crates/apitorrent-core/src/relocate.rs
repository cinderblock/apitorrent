//! Moving a torrent's files to another folder ("move storage"), on a disk thread.
//!
//! Each file is renamed where the volume allows, else copied and the original deleted
//! once the copy is complete. If any move fails, the ones already done are moved back, so
//! a failed move leaves the files where they were.

use std::io;
use std::path::{Path, PathBuf};

/// Move each `(from, to)` that exists. On failure, moves the done ones back and says which
/// path failed and why.
pub(crate) fn move_all(pairs: &[(PathBuf, PathBuf)]) -> Result<(), String> {
    let mut done: Vec<&(PathBuf, PathBuf)> = Vec::new();
    for pair in pairs {
        let (from, to) = pair;
        if std::fs::symlink_metadata(from).is_err() {
            continue; // Not there: nothing to move (a file not downloaded yet, say).
        }
        if let Err(e) = move_path(from, to) {
            for (back_to, back_from) in done.iter().rev().map(|p| (&p.0, &p.1)) {
                if let Err(e) = move_path(back_from, back_to) {
                    tracing::error!(from = %back_from.display(), to = %back_to.display(), error = %e,
                        "could not move a file back after a failed move");
                }
            }
            return Err(format!("could not move {}: {e}", from.display()));
        }
        done.push(pair);
    }
    Ok(())
}

/// Move a file or a directory tree, creating the destination's parent folders.
pub(crate) fn move_path(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::symlink_metadata(to).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", to.display()),
        ));
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    // Another volume (or a rename the platform refuses): copy, then delete.
    copy_tree(from, to).inspect_err(|_| {
        let _ = remove_tree(to);
    })?;
    remove_tree(from)
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(from)?;
    if meta.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to)?;
        // The copy must be on disk before the original goes.
        std::fs::File::open(to)?.sync_all()
    }
}

fn remove_tree(p: &Path) -> io::Result<()> {
    if std::fs::symlink_metadata(p)?.is_dir() {
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p)
    }
}

/// Remove the folders left empty above each of `paths`, up to (not including) `root`.
pub(crate) fn prune_empty_dirs(paths: &[PathBuf], root: &Path) {
    for p in paths {
        let mut dir = p.parent();
        while let Some(d) = dir {
            if d == root || !d.starts_with(root) || std::fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_files_and_trees_and_skips_missing_ones() {
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("old");
        std::fs::create_dir_all(a.join("pack/dir")).unwrap();
        std::fs::write(a.join("pack/one.bin"), b"one").unwrap();
        std::fs::write(a.join("pack/dir/two.bin"), b"two").unwrap();
        std::fs::create_dir_all(a.join("held/x")).unwrap();
        std::fs::write(a.join("held/x/h"), b"held").unwrap();
        let b = t.path().join("new");
        let pairs = vec![
            (a.join("pack/one.bin"), b.join("pack/one.bin")),
            (a.join("pack/dir/two.bin"), b.join("pack/dir/two.bin")),
            (a.join("pack/missing.bin"), b.join("pack/missing.bin")),
            (a.join("held/x"), b.join("held/x")),
        ];
        move_all(&pairs).unwrap();
        assert_eq!(std::fs::read(b.join("pack/dir/two.bin")).unwrap(), b"two");
        assert_eq!(std::fs::read(b.join("held/x/h")).unwrap(), b"held");
        assert!(!a.join("pack/one.bin").exists());
        prune_empty_dirs(&pairs.iter().map(|p| p.0.clone()).collect::<Vec<_>>(), &a);
        assert!(!a.join("pack").exists(), "emptied folders go");
        assert!(a.exists(), "the old save path itself stays");
    }

    #[test]
    fn a_failed_move_puts_everything_back() {
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("old");
        let b = t.path().join("new");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("one"), b"1").unwrap();
        std::fs::write(a.join("two"), b"2").unwrap();
        // Something is already where the second file would go.
        std::fs::write(b.join("two"), b"other").unwrap();
        let pairs = vec![
            (a.join("one"), b.join("one")),
            (a.join("two"), b.join("two")),
        ];
        let err = move_all(&pairs).unwrap_err();
        assert!(err.contains("two"), "{err}");
        assert_eq!(std::fs::read(a.join("one")).unwrap(), b"1");
        assert_eq!(std::fs::read(a.join("two")).unwrap(), b"2");
        assert!(!b.join("one").exists());
        assert_eq!(std::fs::read(b.join("two")).unwrap(), b"other");
    }
}

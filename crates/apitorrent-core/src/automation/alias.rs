//! Stable "latest" aliases: one path that always names a rule's newest completed release.
//!
//! An alias is a **hard link** to the torrent's file when the alias is on the same volume:
//! no extra space, and it keeps working after the torrent it came from is removed (with its
//! data, even). On another volume it is a copy (a copy-on-write clone where the filesystem
//! can). **Symlinks aren't used**: Windows needs a privilege to make them, and a symlink
//! dangles once its target torrent is removed.
//!
//! **Updates are atomic.** The new link is made under a temporary name in the alias's
//! folder, then renamed over the alias, so a reader sees the old file or the new one,
//! never a missing or partial one. On Windows a rename can't replace a file another program
//! holds open without delete sharing; that is reported as [`PlaceError::InUse`] and the
//! caller tries again on a later pass, leaving the old alias in place.

use std::io;
use std::path::{Component, Path, PathBuf};

use apitorrent_types::automation::AliasMethod;

#[derive(Debug, thiserror::Error)]
pub enum PlaceError {
    /// The alias is open in another program, which blocks replacing it (Windows).
    #[error("the alias is open in another program; it will be replaced once it is closed")]
    InUse,
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Io(#[from] io::Error),
}

/// The alias's absolute path: `relative` (validated) under `download_dir`.
pub fn resolve(download_dir: &Path, relative: &str) -> Result<PathBuf, String> {
    let rel = Path::new(relative.trim());
    let plain = !relative.trim().is_empty()
        && !relative.contains(':')
        && rel.components().all(|c| matches!(c, Component::Normal(_)));
    if !plain {
        return Err(format!(
            "the alias path `{relative}` must be relative to the download directory, with no \
             `..`"
        ));
    }
    // Joined part by part, so the path uses this platform's separators throughout.
    Ok(rel
        .components()
        .fold(download_dir.to_path_buf(), |p, c| p.join(c)))
}

/// Errors that mean "someone has the file open" when replacing it.
fn is_in_use(e: &io::Error) -> bool {
    // ERROR_ACCESS_DENIED (5), ERROR_SHARING_VIOLATION (32), ERROR_LOCK_VIOLATION (33).
    cfg!(windows) && matches!(e.raw_os_error(), Some(5 | 32 | 33))
}

fn temp_name(alias: &Path) -> PathBuf {
    let name = alias
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let nonce: u32 = rand::random();
    alias.with_file_name(format!(".{name}.apitorrent-{nonce:08x}.tmp"))
}

/// Make `alias` name `target` (a completed file), replacing what is there. `owned`: the
/// existing file at `alias` was made by an alias, so it may be replaced; anything else in
/// the way is refused.
pub fn place(target: &Path, alias: &Path, owned: bool) -> Result<AliasMethod, PlaceError> {
    match std::fs::symlink_metadata(alias) {
        Ok(m) if m.is_dir() => {
            return Err(PlaceError::Refused(format!(
                "{} is a folder",
                alias.display()
            )));
        }
        Ok(_) if !owned => {
            return Err(PlaceError::Refused(format!(
                "{} already exists and wasn't made by this alias; move it away first",
                alias.display()
            )));
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    if let Some(parent) = alias.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = temp_name(alias);
    let method = match std::fs::hard_link(target, &tmp) {
        Ok(()) => AliasMethod::HardLink,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(e.into()),
        // Another volume, or a filesystem without hard links (FAT, some network shares).
        Err(e) => {
            tracing::debug!(error = %e, "hard link failed; copying the alias instead");
            if let Err(e) = reflink_copy::reflink_or_copy(target, &tmp) {
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into());
            }
            AliasMethod::Copy
        }
    };
    // Rust's `rename` replaces the destination atomically on every platform (on Windows,
    // with POSIX semantics where the filesystem supports them).
    if let Err(e) = std::fs::rename(&tmp, alias) {
        let _ = std::fs::remove_file(&tmp);
        return Err(if is_in_use(&e) {
            PlaceError::InUse
        } else {
            e.into()
        });
    }
    Ok(method)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_paths_stay_under_the_download_directory() {
        let dl = Path::new("downloads");
        assert_eq!(
            resolve(dl, "library/ubuntu/latest.iso").unwrap(),
            dl.join("library").join("ubuntu").join("latest.iso")
        );
        for bad in ["", "/etc/passwd", "../x", "a/../../b", "C:\\x", "C:x"] {
            assert!(resolve(dl, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn places_and_replaces_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let v1 = dir.path().join("v1.iso");
        let v2 = dir.path().join("v2.iso");
        std::fs::write(&v1, b"one").unwrap();
        std::fs::write(&v2, b"two!").unwrap();
        let alias = dir.path().join("library").join("latest.iso");
        assert_eq!(place(&v1, &alias, false).unwrap(), AliasMethod::HardLink);
        assert_eq!(std::fs::read(&alias).unwrap(), b"one");
        // Something not made by the alias is never replaced.
        assert!(matches!(
            place(&v2, &alias, false),
            Err(PlaceError::Refused(_))
        ));
        assert_eq!(place(&v2, &alias, true).unwrap(), AliasMethod::HardLink);
        assert_eq!(std::fs::read(&alias).unwrap(), b"two!");
        // The alias outlives its target.
        std::fs::remove_file(&v2).unwrap();
        assert_eq!(std::fs::read(&alias).unwrap(), b"two!");
        // No temporary files are left behind.
        let left: Vec<_> = std::fs::read_dir(alias.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, vec![std::ffi::OsString::from("latest.iso")]);
        let folder = dir.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        assert!(matches!(
            place(&v1, &folder, true),
            Err(PlaceError::Refused(_))
        ));
    }

    /// A reader that opened the alias without delete sharing blocks the rename on Windows:
    /// the old alias stays, the error says why, and the next try succeeds.
    #[cfg(windows)]
    #[test]
    fn an_open_alias_is_retried_not_broken() {
        use std::os::windows::fs::OpenOptionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let v1 = dir.path().join("v1.iso");
        let v2 = dir.path().join("v2.iso");
        std::fs::write(&v1, b"one").unwrap();
        std::fs::write(&v2, b"two").unwrap();
        let alias = dir.path().join("latest.iso");
        place(&v1, &alias, false).unwrap();
        // FILE_SHARE_READ only: no FILE_SHARE_DELETE, as many programs open files.
        let reader = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&alias)
            .unwrap();
        assert!(matches!(place(&v2, &alias, true), Err(PlaceError::InUse)));
        assert_eq!(std::fs::read(&alias).unwrap(), b"one");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            3,
            "no temp left"
        );
        drop(reader);
        place(&v2, &alias, true).unwrap();
        assert_eq!(std::fs::read(&alias).unwrap(), b"two");
    }

    /// Elsewhere an open reader keeps reading the old file while the alias moves on.
    #[cfg(unix)]
    #[test]
    fn an_open_alias_keeps_its_old_content_for_the_reader() {
        use std::io::Read as _;
        let dir = tempfile::tempdir().unwrap();
        let v1 = dir.path().join("v1.iso");
        let v2 = dir.path().join("v2.iso");
        std::fs::write(&v1, b"one").unwrap();
        std::fs::write(&v2, b"two").unwrap();
        let alias = dir.path().join("latest.iso");
        place(&v1, &alias, false).unwrap();
        let mut reader = std::fs::File::open(&alias).unwrap();
        place(&v2, &alias, true).unwrap();
        let mut old = String::new();
        reader.read_to_string(&mut old).unwrap();
        assert_eq!(old, "one");
        assert_eq!(std::fs::read(&alias).unwrap(), b"two");
    }
}

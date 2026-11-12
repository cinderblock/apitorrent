//! Which file on disk a path names: its volume, its index on that volume, and how many
//! hard links it has. Two paths with the same identity are the same file (hard links), so
//! sharing them costs no space; files on different volumes can never be hard-linked.

use std::io;
use std::path::Path;

/// A file's identity on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    /// The volume (device) it is on.
    pub volume: u64,
    /// Its index on that volume (inode, or NTFS file id).
    pub index: u128,
    /// How many paths (hard links) name it.
    pub links: u64,
}

impl FileIdentity {
    /// Whether two identities are the same file, whatever their link counts.
    pub fn same_file(&self, other: &Self) -> bool {
        self.volume == other.volume && self.index == other.index
    }
}

/// The identity of the file at `path`. Symbolic links are followed.
#[cfg(unix)]
pub fn identity(path: &Path) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path)?;
    Ok(FileIdentity {
        volume: m.dev(),
        index: u128::from(m.ino()),
        links: m.nlink(),
    })
}

/// The identity of the file at `path`. Symbolic links are followed.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn identity(path: &Path) -> io::Result<FileIdentity> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, GetFileInformationByHandle,
    };

    // No access rights are needed to read the file's information, and sharing everything
    // means this never gets in the way of anyone else's handle.
    let f = std::fs::OpenOptions::new()
        .access_mode(0)
        .share_mode(7)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is a valid open file for the duration of the call, and `info` is a
    // properly sized, writable structure.
    let ok = unsafe { GetFileInformationByHandle(f.as_raw_handle(), &raw mut info) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(FileIdentity {
        volume: u64::from(info.dwVolumeSerialNumber),
        index: (u128::from(info.nFileIndexHigh) << 32) | u128::from(info.nFileIndexLow),
        links: u64::from(info.nNumberOfLinks),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hard_links_share_an_identity() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let c = dir.path().join("c");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&c, b"x").unwrap();
        let before = identity(&a).unwrap();
        assert_eq!(before.links, 1);
        std::fs::hard_link(&a, &b).unwrap();
        let (ia, ib, ic) = (
            identity(&a).unwrap(),
            identity(&b).unwrap(),
            identity(&c).unwrap(),
        );
        assert!(ia.same_file(&ib));
        assert!(ia.same_file(&before));
        assert_eq!(ia.links, 2);
        assert!(!ia.same_file(&ic), "equal bytes, but another file");
        assert_eq!(ia.volume, ic.volume);
        assert!(identity(&dir.path().join("missing")).is_err());
    }
}

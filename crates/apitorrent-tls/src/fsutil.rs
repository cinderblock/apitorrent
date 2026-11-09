//! Small file helpers: atomic replacement, and owner-only files for secrets.

use std::io::Write as _;
use std::path::Path;

use crate::TlsError;

/// Write `data` to `path` atomically: a temporary file in the same directory, flushed to
/// disk, then renamed over the target. Readers see the old file or the new one, never half.
/// With `private`, the file is readable only by its owner on Unix (on Windows it inherits
/// the data directory's per-user ACL).
pub(crate) fn write_atomic(path: &Path, data: &[u8], private: bool) -> Result<(), TlsError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| TlsError::io(dir, e))?;
    let mut tmp_name = path.file_name().unwrap_or_default().to_owned();
    tmp_name.push(".tmp");
    let tmp = dir.join(tmp_name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    let mut f = options.open(&tmp).map_err(|e| TlsError::io(&tmp, e))?;
    f.write_all(data).map_err(|e| TlsError::io(&tmp, e))?;
    f.sync_all().map_err(|e| TlsError::io(&tmp, e))?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(|e| TlsError::io(path, e))?;
    Ok(())
}

/// Read a file, or `None` if it doesn't exist.
pub(crate) fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, TlsError> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(TlsError::io(path, e)),
    }
}

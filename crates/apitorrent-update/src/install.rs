//! Downloading a verified binary and swapping it in for the running one.
//!
//! The download goes to a temporary file next to the executable (the same filesystem, so
//! the final step is a rename), hashed on the way, and is kept only if its size and
//! SHA-256 match the signed manifest.
//!
//! The swap:
//!
//! - **Unix**: the new file takes the old one's permissions and is renamed over it, which
//!   is atomic. The running process keeps its (now unlinked) image.
//! - **Windows**: a running executable can't be overwritten or deleted, but it can be
//!   renamed. So the running exe moves aside to `<exe>.old` first, then the new file takes
//!   its name. If that fails the old one is put back. The `.old` file is removed on the
//!   next start ([`remove_leftover`]).

use std::io::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use crate::UpdateError;
use crate::github::Source;
use crate::manifest::ManifestAsset;

/// Where the running exe goes while its replacement moves in (Windows).
pub fn aside_path(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    exe.with_file_name(name)
}

/// Remove the previous binary a Windows swap left behind. Best effort.
pub fn remove_leftover(exe: &Path) {
    let old = aside_path(exe);
    if old.exists() {
        match std::fs::remove_file(&old) {
            Ok(()) => tracing::info!(path = %old.display(), "removed the previous binary"),
            Err(e) => {
                tracing::debug!(path = %old.display(), error = %e, "couldn't remove the previous binary");
            }
        }
    }
}

/// A downloaded, verified binary waiting to be swapped in. Deleted if dropped first.
#[derive(Debug)]
pub struct Staged {
    path: PathBuf,
    kept: bool,
}

impl Staged {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.kept {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn io(context: &str, path: &Path, e: &std::io::Error) -> UpdateError {
    UpdateError::Io(format!("{context} {}: {e}", path.display()))
}

/// Download `asset` into `dir`, checking its size and SHA-256 against the manifest.
pub async fn download(
    source: &Source,
    client: &reqwest::Client,
    asset: &ManifestAsset,
    dir: &Path,
) -> Result<Staged, UpdateError> {
    let url = url::Url::parse(&asset.url)
        .map_err(|e| UpdateError::Network(format!("{}: {e}", asset.url)))?;
    if !source.allows(&url) {
        return Err(UpdateError::Network(format!("refusing to fetch {url}")));
    }
    let mut resp = client
        .get(url.clone())
        .send()
        .await
        .map_err(|e| UpdateError::Network(format!("{url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(UpdateError::Network(format!(
            "{url}: HTTP {}",
            resp.status()
        )));
    }
    let path = dir.join(format!(
        ".apitorrentd-update-{:016x}.tmp",
        rand::random::<u64>()
    ));
    let staged = Staged {
        path: path.clone(),
        kept: false,
    };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| io("couldn't create", &path, &e))?;
    let mut hash = Sha256::new();
    let mut len: u64 = 0;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| UpdateError::Network(format!("{url}: {e}")))?
    {
        len += chunk.len() as u64;
        if len > asset.size {
            return Err(UpdateError::Mismatch(format!(
                "the download is larger than the {} bytes the manifest says",
                asset.size
            )));
        }
        hash.update(&chunk);
        file.write_all(&chunk)
            .map_err(|e| io("couldn't write", &path, &e))?;
    }
    file.sync_all()
        .map_err(|e| io("couldn't write", &path, &e))?;
    drop(file);
    verify_digest(asset, len, &hash.finalize())?;
    Ok(staged)
}

/// The downloaded length and digest against the manifest's.
pub fn verify_digest(asset: &ManifestAsset, len: u64, digest: &[u8]) -> Result<(), UpdateError> {
    if len != asset.size {
        return Err(UpdateError::Mismatch(format!(
            "downloaded {len} bytes, the manifest says {}",
            asset.size
        )));
    }
    let got = hex::encode(digest);
    if got != asset.sha256 {
        return Err(UpdateError::Mismatch(format!(
            "SHA-256 {got} doesn't match the manifest's {}",
            asset.sha256
        )));
    }
    Ok(())
}

/// Put `staged` in place of `exe`.
pub fn swap(mut staged: Staged, exe: &Path) -> Result<(), UpdateError> {
    swap_file(&staged.path, exe)?;
    staged.kept = true;
    Ok(())
}

/// Put the file `new` in place of `exe`, the way this platform needs (see the module
/// docs). `new` must be on the same filesystem.
pub fn swap_file(new: &Path, exe: &Path) -> Result<(), UpdateError> {
    if cfg!(windows) {
        swap_aside(new, exe)
    } else {
        swap_atomic(new, exe)
    }
}

/// Unix: rename over the old file, which keeps running from its unlinked inode.
pub(crate) fn swap_atomic(new: &Path, exe: &Path) -> Result<(), UpdateError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(exe)
            .map_err(|e| io("couldn't read", exe, &e))?
            .permissions()
            .mode();
        std::fs::set_permissions(new, std::fs::Permissions::from_mode(mode | 0o100))
            .map_err(|e| io("couldn't set permissions on", new, &e))?;
    }
    std::fs::rename(new, exe).map_err(|e| io("couldn't replace", exe, &e))
}

/// Windows: move the running exe aside, then rename the new one into its place.
pub(crate) fn swap_aside(new: &Path, exe: &Path) -> Result<(), UpdateError> {
    let old = aside_path(exe);
    // A leftover from an earlier update. If it is still running (a previous process
    // that hasn't exited), this fails, and so would the rename below.
    if old.exists() {
        std::fs::remove_file(&old).map_err(|e| io("couldn't remove", &old, &e))?;
    }
    std::fs::rename(exe, &old).map_err(|e| io("couldn't move aside", exe, &e))?;
    if let Err(e) = std::fs::rename(new, exe) {
        let err = io("couldn't move the new binary to", exe, &e);
        if let Err(back) = std::fs::rename(&old, exe) {
            tracing::error!(
                error = %back,
                from = %old.display(),
                to = %exe.display(),
                "COULDN'T PUT THE OLD BINARY BACK: rename it by hand"
            );
        }
        return Err(err);
    }
    Ok(())
}

/// Check that `dir` (the executable's directory) takes new files, before downloading.
pub fn check_writable(dir: &Path) -> Result<(), UpdateError> {
    let probe = dir.join(format!(".apitorrentd-probe-{:016x}", rand::random::<u64>()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => Err(UpdateError::Io(format!(
            "can't write next to the executable in {}: {e}. Update with the tool that \
             installed apitorrentd, or run `apitorrentd update` as a user who can",
            dir.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_checks() {
        let data = b"binary";
        let asset = ManifestAsset {
            target: "t".into(),
            url: "https://x/y".into(),
            sha256: hex::encode(Sha256::digest(data)),
            size: data.len() as u64,
        };
        verify_digest(&asset, 6, &Sha256::digest(data)).unwrap();
        assert!(matches!(
            verify_digest(&asset, 6, &Sha256::digest(b"tampered")),
            Err(UpdateError::Mismatch(_))
        ));
        assert!(matches!(
            verify_digest(&asset, 7, &Sha256::digest(data)),
            Err(UpdateError::Mismatch(_))
        ));
    }

    #[test]
    fn swaps_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("app.exe");
        for swap in [swap_atomic, swap_aside] {
            std::fs::write(&exe, b"old").unwrap();
            let new = dir.path().join("new.tmp");
            std::fs::write(&new, b"new").unwrap();
            swap(&new, &exe).unwrap();
            assert_eq!(std::fs::read(&exe).unwrap(), b"new");
            assert!(!new.exists());
        }
        // The aside swap left the old binary for the next start to remove.
        assert_eq!(std::fs::read(aside_path(&exe)).unwrap(), b"old");
        remove_leftover(&exe);
        assert!(!aside_path(&exe).exists());
    }

    #[test]
    fn a_failed_aside_swap_puts_the_old_binary_back() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("app.exe");
        std::fs::write(&exe, b"old").unwrap();
        let missing = dir.path().join("missing.tmp");
        assert!(swap_aside(&missing, &exe).is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
    }

    #[test]
    fn writable_probe() {
        let dir = tempfile::tempdir().unwrap();
        check_writable(dir.path()).unwrap();
        assert!(check_writable(&dir.path().join("nope")).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}

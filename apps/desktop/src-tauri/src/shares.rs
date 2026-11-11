//! Turning a storage root's share locators into local paths on this computer.
//!
//! On Windows an SMB share is opened directly by its UNC path (`\\nas\torrents`), and so is
//! an NFS export with the Client for NFS. On macOS and Linux a share has to be mounted first,
//! so the app looks for a mount of it in the mount table (`/proc/self/mounts`, or `mount` on
//! macOS), and for GNOME's GVFS mounts. Every path found here is only a candidate: a
//! mapping is used once the root's marker reads correctly through it.

use std::path::{Path, PathBuf};

use apitorrent_types::share::{ShareLocator, ShareProtocol};

/// One mounted filesystem: what was mounted and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub source: String,
    pub target: PathBuf,
    pub fstype: String,
}

/// Undo the octal escapes (`\040` for a space) in `/proc/self/mounts` fields.
fn unescape_octal(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && let Some(v) = bytes
                .get(i + 1..i + 4)
                .and_then(|d| std::str::from_utf8(d).ok())
                .and_then(|d| u8::from_str_radix(d, 8).ok())
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Linux's `/proc/self/mounts`: `source target fstype options 0 0` per line.
pub fn parse_proc_mounts(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split_ascii_whitespace();
            let source = unescape_octal(f.next()?);
            let target = PathBuf::from(unescape_octal(f.next()?));
            let fstype = f.next()?.to_owned();
            Some(Mount {
                source,
                target,
                fstype,
            })
        })
        .collect()
}

/// macOS's `mount` output: `source on target (fstype, options…)` per line. Names with
/// " on " or " (" in them can't be told apart; such a line is read as best it can be, and
/// the marker check catches a wrong guess.
pub fn parse_mount_output(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let (source, rest) = line.split_once(" on ")?;
            let (target, opts) = rest.rsplit_once(" (")?;
            let fstype = opts
                .split([',', ')'])
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned();
            Some(Mount {
                source: source.to_owned(),
                target: PathBuf::from(target),
                fstype,
            })
        })
        .collect()
}

/// The host and path of a mount's source, if it is a network share of `protocol`:
/// `//[user@]host/share/…` for SMB, `host:/export/…` for NFS.
fn mount_source(source: &str, protocol: ShareProtocol) -> Option<(String, Vec<String>)> {
    match protocol {
        ShareProtocol::Smb => {
            let rest = source.strip_prefix("//")?;
            let (authority, path) = rest.split_once('/')?;
            let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
            // macOS writes `host:port` only when it isn't 445; a port means another share.
            let host = host.split_once(':').map_or(host, |(h, _)| h);
            Some((host.to_owned(), split(path)))
        }
        ShareProtocol::Nfs => {
            let (host, path) = if let Some(v6) = source.strip_prefix('[') {
                let (h, rest) = v6.split_once(']')?;
                (h, rest.strip_prefix(':')?)
            } else {
                source.split_once(':')?
            };
            path.starts_with('/')
                .then(|| (host.to_owned(), split(path)))
        }
    }
}

fn split(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|c| !c.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The parts of `path` below `prefix`, comparing as the protocol does (SMB share and
/// folder names ignore case; NFS paths don't).
fn below<'a>(
    path: &'a [String],
    prefix: &[String],
    protocol: ShareProtocol,
) -> Option<&'a [String]> {
    if prefix.len() > path.len() {
        return None;
    }
    let same = |a: &String, b: &String| match protocol {
        ShareProtocol::Smb => a.to_lowercase() == b.to_lowercase(),
        ShareProtocol::Nfs => a == b,
    };
    prefix
        .iter()
        .zip(path)
        .all(|(a, b)| same(a, b))
        .then(|| &path[prefix.len()..])
}

fn join(base: &Path, parts: &[String]) -> PathBuf {
    let mut p = base.to_path_buf();
    p.extend(parts);
    p
}

/// Local paths that may reach the share `locator` names, most direct first.
pub fn local_paths(locator: &ShareLocator, windows: bool, mounts: &[Mount]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if windows {
        match locator.protocol {
            ShareProtocol::Smb => out.extend(locator.unc_path().map(PathBuf::from)),
            // The Client for NFS opens exports by UNC path too (`\\host\export\…`).
            ShareProtocol::Nfs if locator.port.is_none() => out.push(PathBuf::from(format!(
                "\\\\{}\\{}",
                locator.host,
                locator.path.join("\\")
            ))),
            ShareProtocol::Nfs => {}
        }
        return out;
    }
    for m in mounts {
        if let Some((host, prefix)) = mount_source(&m.source, locator.protocol)
            && host.eq_ignore_ascii_case(&locator.host)
            && let Some(rest) = below(&locator.path, &prefix, locator.protocol)
        {
            out.push(join(&m.target, rest));
        }
        // GNOME mounts SMB shares under its GVFS directory, one folder per share.
        if locator.protocol == ShareProtocol::Smb
            && m.fstype.starts_with("fuse.gvfsd")
            && let Some((share, rest)) = locator.path.split_first()
        {
            let dir = format!(
                "smb-share:server={},share={}",
                locator.host.to_lowercase(),
                share.to_lowercase()
            );
            out.push(join(&m.target.join(dir), rest));
        }
    }
    out.dedup();
    out
}

/// This computer's mounts, where the OS has a table of them (not Windows, which opens
/// shares by UNC path instead).
pub fn current_mounts() -> Vec<Mount> {
    if cfg!(target_os = "linux") {
        std::fs::read_to_string("/proc/self/mounts")
            .map(|t| parse_proc_mounts(&t))
            .unwrap_or_default()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("/sbin/mount")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| parse_mount_output(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or_default()
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(s: &str) -> ShareLocator {
        ShareLocator::parse(s).unwrap()
    }

    #[test]
    fn windows_opens_shares_by_unc_path() {
        let w = |s: &str| local_paths(&loc(s), true, &[]);
        assert_eq!(
            w("smb://NAS/torrents/tv"),
            [PathBuf::from(r"\\NAS\torrents\tv")]
        );
        assert_eq!(w(r"\\nas\t"), [PathBuf::from(r"\\nas\t")]);
        assert_eq!(w("nfs://nas/srv/t"), [PathBuf::from(r"\\nas\srv\t")]);
        assert!(w("smb://nas:4455/t").is_empty(), "UNC can't name a port");
    }

    #[test]
    fn linux_mount_table() {
        let mounts = parse_proc_mounts(
            "/dev/sda1 / ext4 rw 0 0\n\
             //nas/Torrents /mnt/my\\040torrents cifs rw,vers=3.0 0 0\n\
             nas:/srv/media /media/nfs nfs4 rw 0 0\n\
             [fd00::5]:/export /mnt/v6 nfs rw 0 0\n\
             gvfsd-fuse /run/user/1000/gvfs fuse.gvfsd-fuse rw 0 0\n",
        );
        assert_eq!(mounts[1].target, PathBuf::from("/mnt/my torrents"));
        let l = |s: &str| local_paths(&loc(s), false, &mounts);
        assert_eq!(
            l(r"\\NAS\torrents\tv"),
            [
                PathBuf::from("/mnt/my torrents/tv"),
                PathBuf::from("/run/user/1000/gvfs/smb-share:server=nas,share=torrents/tv"),
            ]
        );
        assert_eq!(
            l("nfs://nas/srv/media/tv"),
            [PathBuf::from("/media/nfs/tv")]
        );
        // NFS paths are case-sensitive; other hosts don't match.
        assert!(l("nfs://nas/srv/Media").is_empty());
        assert!(l("nfs://other/srv/media").is_empty());
        assert_eq!(l("nfs://[fd00::5]/export"), [PathBuf::from("/mnt/v6")]);
        // A mount of a folder deeper than the locator doesn't contain it.
        let deep = parse_proc_mounts("//nas/t/sub /mnt/sub cifs rw 0 0");
        assert!(local_paths(&loc(r"\\nas\t"), false, &deep).is_empty());
    }

    #[test]
    fn macos_mount_output() {
        let mounts = parse_mount_output(
            "/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)\n\
             //cam@nas.local/torrents on /Volumes/torrents (smbfs, nodev, nosuid, mounted by cam)\n\
             nas.local:/srv/t on /Volumes/t (nfs)\n",
        );
        assert_eq!(mounts.len(), 3);
        assert_eq!(mounts[1].fstype, "smbfs");
        assert_eq!(
            local_paths(&loc("smb://nas.local/torrents/a b"), false, &mounts),
            [PathBuf::from("/Volumes/torrents/a b")]
        );
        assert_eq!(
            local_paths(&loc("nfs://NAS.local/srv/t"), false, &mounts),
            [PathBuf::from("/Volumes/t")]
        );
    }
}

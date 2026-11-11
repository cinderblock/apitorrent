//! Path mappings: how a path on the daemon's machine is reached from this one.
//!
//! A Remote profile maps remote prefixes to local paths (`/srv/torrents` → `Z:\torrents`,
//! say, over SMB). Before the app opens anything through a mapping, it checks the
//! storage root's `.apitorrent-root` marker through the local path, so "open folder" never
//! lands in the wrong directory. Remote paths come from the server and are treated as
//! untrusted: `..` and `.` components are refused, so a server can't steer the app outside
//! a mapped directory.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use apitorrent_types::{RootMarker, StorageRoot};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathMapping {
    /// A directory on the daemon's machine, as the daemon writes it.
    pub remote: String,
    /// Where that directory is reached from here.
    pub local: PathBuf,
}

impl PathMapping {
    pub fn validate(&self) -> Result<(), String> {
        if RemotePath::parse(&self.remote).is_none() {
            return Err(format!(
                "{}: the remote side must be an absolute path on the server (like /srv/torrents \
                 or D:\\torrents), without . or .. parts",
                self.remote
            ));
        }
        if !self.local.is_absolute() {
            return Err(format!(
                "{}: the local side must be an absolute path",
                self.local.display()
            ));
        }
        Ok(())
    }
}

/// An absolute path on the server, split into components. Windows paths (`C:\…`,
/// `\\server\share\…`) compare case-insensitively, like their filesystems.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePath {
    windows: bool,
    components: Vec<String>,
}

impl RemotePath {
    pub fn parse(path: &str) -> Option<Self> {
        let bytes = path.as_bytes();
        let drive = bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes[2] == b'\\' || bytes[2] == b'/');
        let unc = path.starts_with("\\\\");
        let (windows, components): (bool, Vec<&str>) = if drive || unc {
            (
                true,
                path.split(['\\', '/']).filter(|c| !c.is_empty()).collect(),
            )
        } else if path.starts_with('/') {
            (false, path.split('/').filter(|c| !c.is_empty()).collect())
        } else {
            return None;
        };
        if components.iter().any(|c| *c == "." || *c == "..") {
            return None;
        }
        if unc && components.len() < 2 {
            return None; // `\\server` alone isn't a share.
        }
        Some(Self {
            windows,
            components: components.into_iter().map(str::to_owned).collect(),
        })
    }

    fn same(&self, a: &str, b: &str) -> bool {
        if self.windows {
            a.to_lowercase() == b.to_lowercase()
        } else {
            a == b
        }
    }

    /// The components of `self` below `prefix`, if it is under (or equal to) it.
    pub fn strip_prefix(&self, prefix: &RemotePath) -> Option<&[String]> {
        if self.windows != prefix.windows || prefix.components.len() > self.components.len() {
            return None;
        }
        prefix
            .components
            .iter()
            .zip(&self.components)
            .all(|(a, b)| self.same(a, b))
            .then(|| &self.components[prefix.components.len()..])
    }

    /// Append path components that came from the server (a torrent's file path, joined
    /// with `/`). `None` if any is `.`, `..` or empty.
    pub fn join_relative(&self, relative: &str) -> Option<Self> {
        let mut next = self.clone();
        for c in relative.split('/') {
            if c.is_empty() || c == "." || c == ".." || c.contains('\\') && !self.windows {
                return None;
            }
            if self.windows {
                for part in c.split('\\') {
                    if part.is_empty() || part == "." || part == ".." {
                        return None;
                    }
                    next.components.push(part.to_owned());
                }
            } else {
                next.components.push(c.to_owned());
            }
        }
        Some(next)
    }
}

/// Join server-provided components onto a local directory, refusing anything that could
/// escape it or mean something else here (separators, drive letters, alternate streams).
fn join_local(base: &Path, components: &[String]) -> Option<PathBuf> {
    let mut out = base.to_path_buf();
    for c in components {
        let bad = c.is_empty()
            || c == "."
            || c == ".."
            || c.contains('/')
            || c.contains('\0')
            || (cfg!(windows) && (c.contains('\\') || c.contains(':')));
        if bad {
            return None;
        }
        out.push(c);
    }
    Some(out)
}

/// The local path for a remote one, through the longest matching mapping.
pub fn to_local(mappings: &[PathMapping], remote: &RemotePath) -> Option<(usize, PathBuf)> {
    mappings
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            let prefix = RemotePath::parse(&m.remote)?;
            let rest = remote.strip_prefix(&prefix)?;
            Some((prefix.components.len(), i, join_local(&m.local, rest)?))
        })
        .max_by_key(|(depth, _, _)| *depth)
        .map(|(_, i, path)| (i, path))
}

/// The outcome of checking one storage root through a mapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "result")]
pub enum MarkerCheck {
    Ok,
    /// No marker at the expected place: the share isn't mounted, or points elsewhere.
    Missing,
    /// A marker of another root (or another server).
    WrongRoot {
        found: String,
    },
    Unreadable {
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RootCheck {
    pub root_id: String,
    pub root_path: String,
    pub marker: PathBuf,
    pub check: MarkerCheck,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MappingStatus {
    pub verified: bool,
    /// One per storage root inside or around the mapped directory.
    pub checks: Vec<RootCheck>,
    /// Why it isn't verified, in words.
    pub problem: Option<String>,
}

/// Check a mapping against the server's storage roots: every root inside the mapped
/// directory, or containing it, must have its marker at the matching local place.
pub fn verify(
    mapping: &PathMapping,
    roots: &[StorageRoot],
    read: &dyn Fn(&Path) -> std::io::Result<Vec<u8>>,
) -> MappingStatus {
    let Some(prefix) = RemotePath::parse(&mapping.remote) else {
        return MappingStatus {
            verified: false,
            checks: Vec::new(),
            problem: Some("the remote path isn't an absolute server path".into()),
        };
    };
    let mut checks = Vec::new();
    for root in roots {
        let Some(root_path) = RemotePath::parse(&root.path) else {
            continue;
        };
        let marker_dir = if let Some(below) = root_path.strip_prefix(&prefix) {
            // The root is inside the mapped directory (or is it).
            join_local(&mapping.local, below)
        } else if let Some(above) = prefix.strip_prefix(&root_path) {
            // The mapped directory is inside the root: the marker is that many levels up.
            mapping
                .local
                .ancestors()
                .nth(above.len())
                .map(Path::to_path_buf)
        } else {
            continue;
        };
        let Some(dir) = marker_dir else {
            continue;
        };
        let marker = dir.join(RootMarker::FILE_NAME);
        let check = match read(&marker) {
            Ok(bytes) => match RootMarker::parse(&bytes) {
                Some(m) if m.id == root.id => MarkerCheck::Ok,
                Some(m) => MarkerCheck::WrongRoot { found: m.id },
                None => MarkerCheck::WrongRoot {
                    found: "(not a marker file)".into(),
                },
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => MarkerCheck::Missing,
            Err(e) => MarkerCheck::Unreadable {
                error: e.to_string(),
            },
        };
        checks.push(RootCheck {
            root_id: root.id.clone(),
            root_path: root.path.clone(),
            marker,
            check,
        });
    }
    let problem =
        if checks.is_empty() {
            Some(
                "no storage root of this server is inside or around the remote path, so there is \
             no marker to check"
                    .into(),
            )
        } else {
            checks.iter().find(|c| c.check != MarkerCheck::Ok).map(|bad| match &bad.check {
            MarkerCheck::Missing => format!(
                "{} isn't there: is the share mounted, and does the local path point at {}?",
                bad.marker.display(),
                bad.root_path
            ),
            MarkerCheck::WrongRoot { .. } => format!(
                "{} belongs to a different storage root or server",
                bad.marker.display()
            ),
            MarkerCheck::Unreadable { error } => {
                format!("{} can't be read: {error}", bad.marker.display())
            }
            MarkerCheck::Ok => unreachable!(),
        })
        };
    MappingStatus {
        verified: problem.is_none(),
        checks,
        problem,
    }
}

/// A mapping worth offering for one of the server's roots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Suggestion {
    /// The root's path on the server.
    pub remote: String,
    /// The best local path found: one the marker verified, else the first candidate.
    pub local: Option<PathBuf>,
    /// The root's marker reads correctly through `local`.
    pub verified: bool,
    /// The share locator `local` came from (`None`: the root's own path works here).
    pub via: Option<String>,
    /// The root's share locators, as the server's admin set them.
    pub locators: Vec<String>,
}

fn marker_matches(dir: &Path, id: &str, read: &dyn Fn(&Path) -> std::io::Result<Vec<u8>>) -> bool {
    read(&dir.join(RootMarker::FILE_NAME))
        .ok()
        .and_then(|b| RootMarker::parse(&b))
        .is_some_and(|m| m.id == id)
}

/// Mappings worth offering for a server's roots. The candidates for each root are its own
/// path (when it is an absolute path here: the daemon runs on this machine, or the path is a
/// UNC share), then the local paths of its share locators (see [`crate::shares`]). The first
/// whose marker verifies wins; otherwise the first candidate is offered unverified, for the
/// user to mount or fix.
pub fn suggestions(
    roots: &[StorageRoot],
    read: &dyn Fn(&Path) -> std::io::Result<Vec<u8>>,
    windows: bool,
    mounts: &[crate::shares::Mount],
) -> Vec<Suggestion> {
    roots
        .iter()
        .map(|root| {
            let own = Path::new(&root.path);
            let mut candidates: Vec<(PathBuf, Option<String>)> = Vec::new();
            // The server's own kind of path, Windows or Unix, can be one here only on the
            // same kind of OS.
            let same_kind = RemotePath::parse(&root.path).is_some_and(|r| r.windows == windows);
            if same_kind && own.is_absolute() {
                candidates.push((own.to_path_buf(), None));
            }
            for l in &root.locators {
                // The server checks locators; one this version can't read is skipped.
                if let Ok(parsed) = apitorrent_types::share::ShareLocator::parse(l) {
                    for p in crate::shares::local_paths(&parsed, windows, mounts) {
                        candidates.push((p, Some(l.clone())));
                    }
                }
            }
            let verified = candidates
                .iter()
                .position(|(p, _)| marker_matches(p, &root.id, read));
            let chosen = verified.or((!candidates.is_empty()).then_some(0));
            let (local, via) = chosen
                .map(|i| candidates.swap_remove(i))
                .map_or((None, None), |(p, v)| (Some(p), v));
            Suggestion {
                remote: root.path.clone(),
                local,
                verified: verified.is_some(),
                via,
                locators: root.locators.clone(),
            }
        })
        .collect()
}

/// File types that run code when opened. "Open file" refuses them (the file came from a
/// torrent and the server chose its name); "show in folder" still works.
pub fn runs_code_when_opened(path: &Path) -> bool {
    const RISKY: &[&str] = &[
        "exe", "com", "bat", "cmd", "scr", "pif", "msi", "msix", "msp", "appx", "ps1", "psm1",
        "vbs", "vbe", "js", "jse", "wsf", "wsh", "hta", "cpl", "lnk", "url", "reg", "jar", "app",
        "command", "sh", "desktop", "appimage", "run", "pkg", "dmg", "scpt",
    ];
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| RISKY.iter().any(|r| r.eq_ignore_ascii_case(e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn rp(s: &str) -> RemotePath {
        RemotePath::parse(s).unwrap_or_else(|| panic!("{s}"))
    }

    fn local(s: &str) -> PathBuf {
        // An absolute local path on whatever OS runs the test.
        if cfg!(windows) {
            PathBuf::from(format!("Z:\\{s}"))
        } else {
            PathBuf::from(format!("/mnt/{s}"))
        }
    }

    #[test]
    fn remote_paths() {
        assert!(RemotePath::parse("/srv/torrents").is_some());
        assert!(RemotePath::parse("D:\\torrents").is_some());
        assert!(RemotePath::parse("d:/torrents").is_some());
        assert!(RemotePath::parse("\\\\nas\\share\\t").is_some());
        for bad in [
            "relative/x",
            "",
            "/srv/../etc",
            "C:\\a\\..\\b",
            "/srv/./x",
            "\\\\nas",
        ] {
            assert!(RemotePath::parse(bad).is_none(), "{bad}");
        }
        let unix = rp("/srv/Torrents/a");
        assert!(
            unix.strip_prefix(&rp("/srv/torrents")).is_none(),
            "case matters on Unix"
        );
        assert_eq!(unix.strip_prefix(&rp("/srv/Torrents/")).unwrap(), ["a"]);
        let win = rp("D:\\Torrents\\a");
        assert_eq!(win.strip_prefix(&rp("d:/torrents")).unwrap(), ["a"]);
        assert!(win.strip_prefix(&rp("/d/torrents")).is_none());
        assert!(
            rp("/srv/torrentsX")
                .strip_prefix(&rp("/srv/torrents"))
                .is_none()
        );
    }

    #[test]
    fn longest_mapping_wins_and_traversal_is_refused() {
        let mappings = vec![
            PathMapping {
                remote: "/srv".into(),
                local: local("srv"),
            },
            PathMapping {
                remote: "/srv/torrents".into(),
                local: local("t"),
            },
        ];
        let (i, path) = to_local(&mappings, &rp("/srv/torrents/show/ep1.mkv")).unwrap();
        assert_eq!(i, 1);
        assert_eq!(path, local("t").join("show").join("ep1.mkv"));
        let (i, path) = to_local(&mappings, &rp("/srv/other")).unwrap();
        assert_eq!(i, 0);
        assert_eq!(path, local("srv").join("other"));
        assert!(to_local(&mappings, &rp("/home/x")).is_none());

        let base = rp("/srv/torrents");
        assert!(base.join_relative("show/../../../etc/passwd").is_none());
        assert!(base.join_relative("").is_none());
        assert!(base.join_relative("a//b").is_none());
        assert_eq!(
            base.join_relative("show/ep1.mkv").unwrap(),
            rp("/srv/torrents/show/ep1.mkv")
        );
        // A Unix file name with a backslash can't be mapped onto Windows' separator.
        assert!(base.join_relative("a\\b").is_none());
        let win = rp("D:\\t");
        assert!(win.join_relative("a\\..\\..\\x").is_none());
    }

    #[cfg(windows)]
    #[test]
    fn names_meaning_something_else_on_windows_are_refused() {
        let mappings = vec![PathMapping {
            remote: "/srv".into(),
            local: local("srv"),
        }];
        // `a:b` would be an alternate data stream (or a drive) here.
        assert!(to_local(&mappings, &rp("/srv/a:b")).is_none());
    }

    fn files(entries: &[(PathBuf, &str)]) -> impl Fn(&Path) -> std::io::Result<Vec<u8>> + use<> {
        let map: HashMap<PathBuf, Vec<u8>> = entries
            .iter()
            .map(|(p, id)| {
                (
                    p.join(RootMarker::FILE_NAME),
                    serde_json::to_vec(&RootMarker::new(*id)).unwrap(),
                )
            })
            .collect();
        move |p: &Path| {
            map.get(p)
                .cloned()
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
        }
    }

    fn root(id: &str, path: &str) -> StorageRoot {
        StorageRoot {
            id: id.into(),
            path: path.into(),
            locators: Vec::new(),
        }
    }

    #[test]
    fn verification_reads_the_marker_through_the_local_path() {
        let roots = [root("r1", "/srv/torrents")];
        let exact = PathMapping {
            remote: "/srv/torrents".into(),
            local: local("t"),
        };
        let fs = files(&[(local("t"), "r1")]);
        let s = verify(&exact, &roots, &fs);
        assert!(s.verified, "{s:?}");
        assert_eq!(s.checks.len(), 1);

        // A parent of the root: the marker is below the local path.
        let parent = PathMapping {
            remote: "/srv".into(),
            local: local("srv"),
        };
        let fs = files(&[(local("srv").join("torrents"), "r1")]);
        assert!(verify(&parent, &roots, &fs).verified);

        // A directory inside the root: the marker is above the local path.
        let inner = PathMapping {
            remote: "/srv/torrents/tv".into(),
            local: local("t").join("tv"),
        };
        let fs = files(&[(local("t"), "r1")]);
        assert!(verify(&inner, &roots, &fs).verified);
    }

    #[test]
    fn verification_fails_on_missing_or_foreign_markers() {
        let roots = [root("r1", "/srv/torrents")];
        let m = PathMapping {
            remote: "/srv/torrents".into(),
            local: local("t"),
        };
        let s = verify(&m, &roots, &files(&[]));
        assert!(!s.verified);
        assert_eq!(s.checks[0].check, MarkerCheck::Missing);
        assert!(s.problem.unwrap().contains("mounted"));

        let s = verify(&m, &roots, &files(&[(local("t"), "someone-else")]));
        assert_eq!(
            s.checks[0].check,
            MarkerCheck::WrongRoot {
                found: "someone-else".into()
            }
        );

        // Nothing to check against: not verified.
        let elsewhere = PathMapping {
            remote: "/home/me".into(),
            local: local("me"),
        };
        let s = verify(&elsewhere, &roots, &files(&[(local("me"), "r1")]));
        assert!(!s.verified);
        assert!(s.checks.is_empty());

        // Every root in the mapped directory must check out.
        let roots = [root("r1", "/srv/a"), root("r2", "/srv/b")];
        let parent = PathMapping {
            remote: "/srv".into(),
            local: local("srv"),
        };
        let fs = files(&[(local("srv").join("a"), "r1")]);
        let s = verify(&parent, &roots, &fs);
        assert!(!s.verified);
        assert_eq!(s.checks.len(), 2);
        let fs = files(&[
            (local("srv").join("a"), "r1"),
            (local("srv").join("b"), "r2"),
        ]);
        assert!(verify(&parent, &roots, &fs).verified);
    }

    #[test]
    fn suggestions_fill_in_paths_that_work_here() {
        let tmp = tempfile::tempdir().unwrap();
        let here = std::path::absolute(tmp.path()).unwrap();
        std::fs::write(
            here.join(RootMarker::FILE_NAME),
            serde_json::to_vec(&RootMarker::new("r1")).unwrap(),
        )
        .unwrap();
        let roots = [
            root("r1", &here.display().to_string()),
            root("r2", "/nowhere/at/all"),
        ];
        let s = suggestions(&roots, &|p| std::fs::read(p), cfg!(windows), &[]);
        assert_eq!(s[0].local.as_deref(), Some(here.as_path()));
        assert!(s[0].verified);
        assert_eq!(s[0].via, None);
        assert_eq!(s[1].remote, "/nowhere/at/all");
        assert!(!s[1].verified);
        if cfg!(windows) {
            assert_eq!(s[1].local, None, "a Unix path isn't a path on Windows");
        }
    }

    #[test]
    fn suggestions_come_from_share_locators() {
        let mut r = root("r1", "/srv/torrents");
        r.locators = vec![r"\\nas\torrents".into(), "nfs://nas/srv/torrents".into()];
        let mounts = crate::shares::parse_proc_mounts("nas:/srv /mnt/srv nfs4 rw 0 0");

        // Windows: UNC first. Neither verifies, so the first is offered unverified.
        let s = suggestions(std::slice::from_ref(&r), &files(&[]), true, &[]);
        assert_eq!(s[0].local, Some(PathBuf::from(r"\\nas\torrents")));
        assert!(!s[0].verified);
        assert_eq!(s[0].via.as_deref(), Some(r"\\nas\torrents"));
        assert_eq!(s[0].locators, r.locators);
        // The one whose marker reads wins, whatever its place.
        let s = suggestions(
            std::slice::from_ref(&r),
            &files(&[(PathBuf::from(r"\\nas\srv\torrents"), "r1")]),
            true,
            &[],
        );
        assert_eq!(s[0].local, Some(PathBuf::from(r"\\nas\srv\torrents")));
        assert!(s[0].verified);
        assert_eq!(s[0].via.as_deref(), Some("nfs://nas/srv/torrents"));

        // macOS and Linux: through the mount table.
        let s = suggestions(
            std::slice::from_ref(&r),
            &files(&[(PathBuf::from("/mnt/srv/torrents"), "r1")]),
            false,
            &mounts,
        );
        assert_eq!(s[0].local, Some(PathBuf::from("/mnt/srv/torrents")));
        assert!(s[0].verified);
        // Not mounted: nothing to offer but the locators themselves.
        let s = suggestions(std::slice::from_ref(&r), &files(&[]), false, &[]);
        if !cfg!(windows) {
            assert_eq!(s[0].local, Some(PathBuf::from("/srv/torrents")));
        }
        assert!(!s[0].verified);
    }

    #[test]
    fn executables_are_not_opened() {
        for p in ["setup.exe", "x.BAT", "a/b/run.ps1", "y.lnk", "z.desktop"] {
            assert!(runs_code_when_opened(Path::new(p)), "{p}");
        }
        for p in ["movie.mkv", "notes.txt", "x.iso", "noext"] {
            assert!(!runs_code_when_opened(Path::new(p)), "{p}");
        }
    }

    #[test]
    fn mappings_must_be_absolute_on_both_sides() {
        assert!(
            PathMapping {
                remote: "srv".into(),
                local: local("x")
            }
            .validate()
            .is_err()
        );
        assert!(
            PathMapping {
                remote: "/srv".into(),
                local: PathBuf::from("relative")
            }
            .validate()
            .is_err()
        );
    }
}

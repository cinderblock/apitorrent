//! Magnet links and `.torrent` files handed to the app by the OS: on the command line (at
//! launch, or forwarded from a second instance) or as URLs (macOS "open" events).

use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::Serialize;

/// Larger than any real `.torrent` file; a guard against reading something huge by mistake.
const MAX_TORRENT_FILE: u64 = 16 * 1024 * 1024;
/// Magnet links are a few hundred bytes; this leaves room for long tracker lists.
const MAX_MAGNET: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum IncomingLink {
    Magnet {
        uri: String,
    },
    /// The file's bytes, base64, as the API's `torrentFile` source takes them.
    #[serde(rename_all = "camelCase")]
    TorrentFile {
        name: String,
        data: String,
    },
    /// Something was handed over but couldn't be used; shown to the user.
    Problem {
        message: String,
    },
}

fn is_magnet(s: &str) -> bool {
    s.get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("magnet:"))
}

fn is_torrent_path(s: &str) -> bool {
    Path::new(s)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("torrent"))
}

/// Links in command-line arguments (without the program name). Relative paths are
/// relative to `cwd`, the directory the launching instance ran in. Other arguments (flags
/// the OS or a launcher adds) are ignored.
pub fn from_args<I, S>(args: I, cwd: &Path) -> Vec<IncomingLink>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter()
        .filter_map(|a| from_arg(a.as_ref(), cwd))
        .collect()
}

/// Links in URLs from the OS (deep links and opened files).
pub fn from_urls(urls: &[url::Url]) -> Vec<IncomingLink> {
    urls.iter()
        .filter_map(|u| match u.scheme() {
            "magnet" => Some(magnet(u.as_str())),
            "file" => u
                .to_file_path()
                .ok()
                .and_then(|p| is_torrent_path(&p.to_string_lossy()).then(|| torrent_file(&p))),
            _ => None,
        })
        .collect()
}

/// Drop magnet links: for when the deep-link plugin delivers them on its own (a second
/// launch's arguments, macOS open events), so they aren't added twice.
pub fn without_magnets(links: Vec<IncomingLink>) -> Vec<IncomingLink> {
    links
        .into_iter()
        .filter(|l| !matches!(l, IncomingLink::Magnet { .. }))
        .collect()
}

fn from_arg(arg: &str, cwd: &Path) -> Option<IncomingLink> {
    let arg = arg.trim();
    if is_magnet(arg) {
        return Some(magnet(arg));
    }
    if let Ok(u) = url::Url::parse(arg)
        && u.scheme() == "file"
    {
        return from_urls(&[u]).into_iter().next();
    }
    if is_torrent_path(arg) {
        let p = PathBuf::from(arg);
        let p = if p.is_absolute() { p } else { cwd.join(p) };
        return Some(torrent_file(&p));
    }
    None
}

fn magnet(uri: &str) -> IncomingLink {
    if uri.len() > MAX_MAGNET {
        return IncomingLink::Problem {
            message: "a magnet link was too long to use".into(),
        };
    }
    IncomingLink::Magnet {
        uri: uri.to_owned(),
    }
}

fn torrent_file(path: &Path) -> IncomingLink {
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let problem = |why: String| IncomingLink::Problem {
        message: format!("couldn't open {name}: {why}"),
    };
    match std::fs::metadata(path) {
        Ok(m) if !m.is_file() => return problem("not a file".into()),
        Ok(m) if m.len() > MAX_TORRENT_FILE => return problem("too large for a .torrent".into()),
        Ok(_) => {}
        Err(e) => return problem(e.to_string()),
    }
    match std::fs::read(path) {
        Ok(bytes) => IncomingLink::TorrentFile {
            name,
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        },
        Err(e) => problem(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magnets_and_files_on_the_command_line() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("ubuntu.torrent"), b"d4:infod...ee").unwrap();
        let links = from_args(
            [
                "--flag",
                "MAGNET:?xt=urn:btih:abc",
                "ubuntu.torrent",
                "missing.torrent",
                "notes.txt",
            ],
            tmp.path(),
        );
        assert_eq!(links.len(), 3);
        assert_eq!(
            links[0],
            IncomingLink::Magnet {
                uri: "MAGNET:?xt=urn:btih:abc".into()
            }
        );
        match &links[1] {
            IncomingLink::TorrentFile { name, data } => {
                assert_eq!(name, "ubuntu.torrent");
                assert_eq!(
                    base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .unwrap(),
                    b"d4:infod...ee"
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(
            matches!(&links[2], IncomingLink::Problem { message } if message.contains("missing.torrent"))
        );
    }

    #[test]
    fn urls_from_the_os() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a b.torrent");
        std::fs::write(&file, b"de").unwrap();
        let urls = [
            url::Url::parse("magnet:?xt=urn:btih:abc").unwrap(),
            url::Url::from_file_path(&file).unwrap(),
            url::Url::parse("https://example.com/x.torrent").unwrap(),
        ];
        let links = from_urls(&urls);
        assert_eq!(links.len(), 2);
        assert!(
            matches!(&links[1], IncomingLink::TorrentFile { name, .. } if name == "a b.torrent")
        );
        // A file URL on the command line works too.
        let from_arg = from_args(
            [url::Url::from_file_path(&file).unwrap().as_str()],
            tmp.path(),
        );
        assert!(matches!(&from_arg[0], IncomingLink::TorrentFile { .. }));
    }

    #[test]
    fn magnets_can_be_left_to_the_deep_link_handler() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.torrent"), b"de").unwrap();
        let links = from_args(["magnet:?xt=urn:btih:abc", "a.torrent"], tmp.path());
        let rest = without_magnets(links);
        assert_eq!(rest.len(), 1);
        assert!(matches!(&rest[0], IncomingLink::TorrentFile { .. }));
    }

    #[test]
    fn oversized_input_is_refused() {
        let long = format!("magnet:?xt=urn:btih:{}", "a".repeat(MAX_MAGNET));
        assert!(matches!(
            from_args([long], Path::new(".")).as_slice(),
            [IncomingLink::Problem { .. }]
        ));
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("dir.torrent")).unwrap();
        assert!(matches!(
            from_args(["dir.torrent"], tmp.path()).as_slice(),
            [IncomingLink::Problem { .. }]
        ));
    }
}

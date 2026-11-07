//! Naming policies: the same torrent laid out under portable, native and verbatim rules.

use apitorrent_metainfo::sanitize::Naming;
use apitorrent_metainfo::{FileSource, Torrent, TorrentBuilder, Version};

fn torrent() -> Torrent {
    let bytes = TorrentBuilder::new("show: season 1", Version::V1)
        .piece_length(16 * 1024)
        .file(FileSource::from_bytes(&["ep 1: pilot.mkv"], vec![1; 10]))
        .file(FileSource::from_bytes(&["notes."], vec![2; 10]))
        .file(FileSource::from_bytes(&["CON.txt"], vec![3; 10]))
        .file(FileSource::from_bytes(&["A.txt"], vec![4; 10]))
        .file(FileSource::from_bytes(&["a.txt"], vec![5; 10]))
        .build()
        .unwrap();
    Torrent::from_bytes(&bytes).unwrap()
}

fn joined(paths: &[Vec<String>]) -> Vec<String> {
    paths.iter().map(|p| p.join("/")).collect()
}

#[test]
fn portable_is_the_parsed_layout() {
    let t = torrent();
    let portable = t.info.paths(Naming::Portable).unwrap();
    let parsed: Vec<Vec<String>> = t.info.files.iter().map(|f| f.path.clone()).collect();
    assert_eq!(portable, parsed);
    assert_eq!(
        joined(&portable),
        [
            "show_ season 1/ep 1_ pilot.mkv",
            "show_ season 1/notes",
            "show_ season 1/_CON.txt",
            "show_ season 1/A.txt",
            "show_ season 1/a.1.txt",
        ]
    );
}

#[test]
fn verbatim_keeps_the_original_names() {
    let t = torrent();
    assert_eq!(
        joined(&t.info.paths(Naming::Verbatim).unwrap()),
        [
            "show: season 1/ep 1: pilot.mkv",
            "show: season 1/notes.",
            "show: season 1/CON.txt",
            "show: season 1/A.txt",
            "show: season 1/a.txt",
        ]
    );
}

#[cfg(not(windows))]
#[test]
fn native_on_posix_keeps_what_posix_allows() {
    let t = torrent();
    let case_sensitive = t
        .info
        .paths(Naming::Native {
            case_insensitive: false,
        })
        .unwrap();
    assert_eq!(
        joined(&case_sensitive),
        [
            "show: season 1/ep 1: pilot.mkv",
            "show: season 1/notes.",
            "show: season 1/CON.txt",
            "show: season 1/A.txt",
            "show: season 1/a.txt",
        ]
    );
    let folding = t
        .info
        .paths(Naming::Native {
            case_insensitive: true,
        })
        .unwrap();
    assert_eq!(folding[4].join("/"), "show: season 1/a.1.txt");
}

#[cfg(windows)]
#[test]
fn native_on_windows_is_portable() {
    let t = torrent();
    assert_eq!(
        t.info
            .paths(Naming::Native {
                case_insensitive: true
            })
            .unwrap(),
        t.info.paths(Naming::Portable).unwrap()
    );
}

#[test]
fn posix_rules_still_block_escapes() {
    use apitorrent_metainfo::sanitize::posix_component;
    assert_eq!(posix_component(b".."), "_");
    assert_eq!(posix_component(b"."), "_");
    assert_eq!(posix_component(b"a/b"), "a_b");
    assert_eq!(posix_component(b"/etc"), "etc");
    assert_eq!(posix_component(b"x\x00y\x1b"), "x_y_");
    assert_eq!(posix_component(b""), "_");
    assert_eq!(posix_component(&[b'a'; 300]).len(), 255);
}

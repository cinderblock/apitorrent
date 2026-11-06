//! Parse libtorrent's test torrents (BSD-3-Clause, see `fixtures/libtorrent/LICENSE`).
//!
//! The expected outcomes follow libtorrent's `test_torrent_info.cpp`, with deliberate
//! differences where apitorrent's rules differ (portable sanitising, limits). Each
//! difference is called out next to its case.

use std::path::PathBuf;

use apitorrent_metainfo::{MetainfoError, Torrent, Version};

fn load(name: &str) -> Result<Torrent, MetainfoError> {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "tests",
        "fixtures",
        "libtorrent",
        name,
    ]
    .iter()
    .collect();
    let data = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    Torrent::from_bytes(&data)
}

fn ok(name: &str) -> Torrent {
    load(name).unwrap_or_else(|e| panic!("{name} should parse: {e}"))
}

fn paths(t: &Torrent) -> Vec<String> {
    t.info
        .files
        .iter()
        .filter(|f| !f.is_pad())
        .map(apitorrent_metainfo::FileEntry::display_path)
        .collect()
}

const VALID: &[&str] = &[
    "base.torrent",
    "empty_path.torrent",
    "parent_path.torrent",
    "hidden_parent_path.torrent",
    "single_multi_file.torrent",
    "slash_path.torrent",
    "slash_path2.torrent",
    "slash_path3.torrent",
    "backslash_path.torrent",
    "url_list.torrent",
    "url_list2.torrent",
    "url_list3.torrent",
    "httpseed.torrent",
    "empty_httpseed.torrent",
    "long_name.torrent",
    "whitespace_url.torrent",
    "duplicate_files.torrent",
    "pad_file.torrent",
    "creation_date.torrent",
    "no_creation_date.torrent",
    "url_seed.torrent",
    "url_seed_multi.torrent",
    "url_seed_multi_single_file.torrent",
    "url_seed_multi_space.torrent",
    "url_seed_multi_space_nolist.torrent",
    "empty_path_multi.torrent",
    "duplicate_web_seeds.torrent",
    "invalid_name2.torrent",
    "invalid_name3.torrent",
    "symlink1.torrent",
    "symlink2.torrent",
    "unordered.torrent",
    "symlink_zero_size.torrent",
    "pad_file_no_path.torrent",
    "large.torrent",
    "absolute_filename.torrent",
    "invalid_filename.torrent",
    "invalid_filename2.torrent",
    "invalid_directory_name.torrent",
    "overlapping_symlinks.torrent",
    "v2.torrent",
    "v2_multipiece_file.torrent",
    "v2_only.torrent",
    "v2_invalid_filename.torrent",
    "v2_invalid_filename2.torrent",
    "v2_multiple_files.torrent",
    "v2_symlinks.torrent",
    "v2_hybrid.torrent",
    "v2_hybrid-missing-tailpad.torrent",
    "v2_identical_files.torrent",
    "v2_empty_file.torrent",
    "empty-files-1.torrent",
    "empty-files-2.torrent",
    "empty-files-3.torrent",
    "empty-files-4.torrent",
    "empty-files-5.torrent",
    "v2_no_piece_layers.torrent",
    "v2_incomplete_piece_layer.torrent",
    "similar.torrent",
    "similar2.torrent",
    "collection.torrent",
    "collection2.torrent",
    "dht_nodes.torrent",
    "large_piece_size.torrent",
    "sample.torrent",
];

const INVALID: &[&str] = &[
    "missing_piece_len.torrent",
    "invalid_piece_len.torrent",
    "negative_piece_len.torrent",
    "no_name.torrent",
    "bad_name.torrent",
    "invalid_name.torrent",
    "invalid_info.torrent",
    "string.torrent",
    "negative_size.torrent",
    "negative_file_size.torrent",
    "invalid_path_list.torrent",
    "missing_path_list.torrent",
    "invalid_pieces.torrent",
    "unaligned_pieces.torrent",
    "invalid_file_size.torrent",
    "invalid_symlink.torrent",
    "many_pieces.torrent",
    "no_files.torrent",
    "zero.torrent",
    "zero2.torrent",
    "v2_mismatching_metadata.torrent",
    "v2_no_power2_piece.torrent",
    "v2_invalid_file.torrent",
    "v2_deep_recursion.torrent",
    "v2_non_multiple_piece_layer.torrent",
    "v2_piece_layer_invalid_file_hash.torrent",
    "v2_invalid_piece_layer.torrent",
    "v2_invalid_piece_layer_root.torrent",
    "v2_unknown_piece_layer_entry.torrent",
    "v2_invalid_piece_layer_size.torrent",
    "v2_same_root_single_piece.torrent",
    "v2_same_root_multi_piece.torrent",
    "v2_bad_file_alignment.torrent",
    "v2_unordered_files.torrent",
    "v2_overlong_integer.torrent",
    "v2_missing_file_root_invalid_symlink.torrent",
    "v2_large_offset.torrent",
    "v2_piece_size.torrent",
    "v2_invalid_pad_file.torrent",
    "v2_zero_root.torrent",
    "v2_zero_root_small.torrent",
    "v2_empty_filename.torrent",
    "v2_invalid_root_hash.torrent",
    // libtorrent rejects this for its 16 TiB file, which is only a libtorrent limit. It is
    // rejected here because its piece layer doesn't match the claimed size.
    "v2_large_file.torrent",
];

#[test]
fn every_fixture_is_classified() {
    let dir: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "tests",
        "fixtures",
        "libtorrent",
    ]
    .iter()
    .collect();
    let mut unclassified = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        if name.ends_with(".torrent")
            && !VALID.contains(&name.as_str())
            && !INVALID.contains(&name.as_str())
        {
            unclassified.push(name);
        }
    }
    assert!(
        unclassified.is_empty(),
        "unclassified fixtures: {unclassified:?}"
    );
}

#[test]
fn valid_torrents_parse() {
    let failures: Vec<String> = VALID
        .iter()
        .filter_map(|name| load(name).err().map(|e| format!("{name}: {e}")))
        .collect();
    assert!(
        failures.is_empty(),
        "should parse:\n{}",
        failures.join("\n")
    );
}

#[test]
fn invalid_torrents_are_rejected() {
    let accepted: Vec<&str> = INVALID
        .iter()
        .copied()
        .filter(|name| load(name).is_ok())
        .collect();
    assert!(accepted.is_empty(), "should be rejected: {accepted:?}");
}

#[test]
fn specific_errors() {
    use MetainfoError as E;
    assert!(matches!(
        load("invalid_piece_len.torrent"),
        Err(E::Field("piece length"))
    ));
    assert!(matches!(
        load("negative_piece_len.torrent"),
        Err(E::InvalidPieceLength(_))
    ));
    assert!(matches!(
        load("v2_no_power2_piece.torrent"),
        Err(E::InvalidPieceLength(_))
    ));
    assert!(matches!(
        load("invalid_name.torrent"),
        Err(E::Field("name"))
    ));
    assert!(matches!(load("invalid_info.torrent"), Err(E::MissingInfo)));
    assert!(matches!(load("string.torrent"), Err(E::NotADictionary)));
    assert!(matches!(
        load("negative_file_size.torrent"),
        Err(E::InvalidLength { .. })
    ));
    assert!(matches!(
        load("unaligned_pieces.torrent"),
        Err(E::UnalignedPieces)
    ));
    // The fixture's symlink target is malformed inside the `path` list itself.
    assert!(matches!(
        load("invalid_symlink.torrent"),
        Err(E::Field("path"))
    ));
    assert!(matches!(
        load("many_pieces.torrent"),
        Err(E::TooManyPieces(_))
    ));
    assert!(matches!(
        load("v2_deep_recursion.torrent"),
        Err(E::Bencode(_))
    ));
    assert!(matches!(
        load("v2_unordered_files.torrent"),
        Err(E::FileTree(_))
    ));
    assert!(matches!(
        load("v2_overlong_integer.torrent"),
        Err(E::Bencode(_))
    ));
    assert!(matches!(
        load("v2_same_root_single_piece.torrent"),
        Err(E::InconsistentFileRoots)
    ));
    assert!(matches!(
        load("v2_same_root_multi_piece.torrent"),
        Err(E::InconsistentFileRoots)
    ));
    assert!(matches!(
        load("v2_mismatching_metadata.torrent"),
        Err(E::InconsistentHybrid(_))
    ));
    assert!(matches!(
        load("v2_invalid_pad_file.torrent"),
        Err(E::InvalidPadFile(_))
    ));
    assert!(matches!(
        load("v2_zero_root.torrent"),
        Err(E::MissingPiecesRoot(_))
    ));
    assert!(matches!(
        load("v2_zero_root_small.torrent"),
        Err(E::MissingPiecesRoot(_))
    ));
    for name in [
        "v2_non_multiple_piece_layer.torrent",
        "v2_piece_layer_invalid_file_hash.torrent",
        "v2_invalid_piece_layer.torrent",
        "v2_invalid_piece_layer_root.torrent",
        "v2_unknown_piece_layer_entry.torrent",
        "v2_invalid_piece_layer_size.torrent",
    ] {
        assert!(
            matches!(load(name), Err(E::InvalidPieceLayer(_))),
            "{name}: {:?}",
            load(name)
        );
    }
}

#[test]
fn v2_hashes_and_roots() {
    let t = ok("v2.torrent");
    assert_eq!(t.info.version(), Version::Hybrid);
    assert_eq!(paths(&t), ["test64K"]);
    assert_eq!(t.info.files[0].length, 65536);
    assert_eq!(
        t.info.files[0].pieces_root.unwrap().to_string(),
        "60aae9c7b428f87e0713e88229e18f0adf12cd7b22a0dd8a92bb2485eb7af242"
    );
    assert_eq!(
        t.info_hashes().get_v2().unwrap().to_string(),
        "597b180c1a170a585dfc5e85d834d69013ceda174b8f357d5bb1a0ca509faf0a"
    );

    let t = ok("v2_multipiece_file.torrent");
    assert_eq!(
        t.info_hashes().get_v2().unwrap().to_string(),
        "108ac2c3718ce722e6896edc56c4afa98f1d711ecaace7aad74fca418ebd03de"
    );
    assert!(t.has_all_piece_layers());

    let t = ok("v2_only.torrent");
    assert_eq!(t.info.version(), Version::V2);
    assert!(t.info_hashes().get_v1().is_none());
    assert_eq!(
        t.info_hashes().get_v2().unwrap().to_string(),
        "95e04d0c4bad94ab206efa884666fd89777dbe4f7bd9945af1829037a85c6192"
    );
    assert_eq!(
        t.info.files[0].pieces_root.unwrap().to_string(),
        "515ea9181744b817744ded9d2e8e9dc6a8450c0b0c52e24b5077f302ffbd9008"
    );

    assert!(!ok("v2_no_piece_layers.torrent").has_all_piece_layers());
    assert!(!ok("v2_incomplete_piece_layer.torrent").has_all_piece_layers());
}

#[test]
fn v2_only_layout_matches_hybrid_alignment() {
    let t = ok("v2_multiple_files.torrent");
    for f in &t.info.files {
        if !f.is_pad() && f.length > 0 {
            assert_eq!(
                f.offset % t.info.piece_length,
                0,
                "{} is unaligned",
                f.display_path()
            );
        }
    }
}

#[test]
fn sanitising_and_disambiguation() {
    // Path separators inside a component are replaced, never interpreted.
    assert_eq!(paths(&ok("slash_path.torrent")), ["temp/_/_/bar"]);
    assert_eq!(
        paths(&ok("slash_path2.torrent")),
        ["temp/abc_.._.._def/_/bar"]
    );
    assert_eq!(paths(&ok("slash_path3.torrent")), ["temp_.._.._abc"]);
    assert_eq!(
        paths(&ok("duplicate_files.torrent")),
        ["temp/foo/bar.txt", "temp/foo/bar.1.txt"]
    );
    assert_eq!(
        paths(&ok("invalid_directory_name.torrent")),
        ["test2/_/foo", "test2/_/foo.1"]
    );
    assert_eq!(
        paths(&ok("empty_path_multi.torrent")),
        ["temp/abcde", "temp/_", "temp/_.1"]
    );
    assert_eq!(
        paths(&ok("absolute_filename.torrent")),
        ["temp/abcde", "temp/foobar"]
    );
    // Every invalid character becomes `_`; libtorrent drops some of them instead.
    assert_eq!(paths(&ok("v2_invalid_filename.torrent")), ["_est_MB"]);
    assert_eq!(
        paths(&ok("v2_invalid_filename2.torrent")),
        ["test/_", "test/_.1", "test/stress_test2"]
    );
    // An unusable name becomes the info-hash.
    assert_eq!(
        ok("invalid_name2.torrent").info.name,
        "b61560c2918f463768cd122b6d2fdd47b77bdb35"
    );
    // Trailing spaces are removed on every platform, not just Windows.
    assert_eq!(ok("invalid_name3.torrent").info.name, "foobar");
}

#[test]
fn pad_files() {
    let t = ok("pad_file.torrent");
    assert_eq!(t.info.files.len(), 2);
    assert!(!t.info.files[0].is_pad());
    assert!(t.info.files[1].is_pad());
    let t = ok("pad_file_no_path.torrent");
    assert_eq!(t.info.files[1].display_path(), ".pad/2124");
}

#[test]
fn symlinks() {
    let t = ok("symlink1.torrent");
    assert_eq!(
        t.info.files[1].symlink_target.as_ref().unwrap().join("/"),
        "temp/a/b/bar"
    );
    let t = ok("v2_symlinks.torrent");
    assert_eq!(
        t.info.files[0].symlink_target.as_ref().unwrap().join("/"),
        "SDL2.framework/Versions/Current/Headers"
    );
}

#[test]
fn top_level_fields() {
    assert_eq!(ok("creation_date.torrent").creation_date, Some(1_234_567));
    assert_eq!(ok("no_creation_date.torrent").creation_date, None);
    assert_eq!(
        ok("whitespace_url.torrent").trackers[0][0],
        "udp://test.com/announce"
    );
    assert_eq!(ok("url_seed.torrent").web_seeds, ["http://test.com/file"]);
    assert_eq!(
        ok("url_seed_multi.torrent").web_seeds,
        ["http://test.com/file/"]
    );
    assert_eq!(
        ok("url_seed_multi_space.torrent").web_seeds,
        ["http://test.com/test%20file/foo%20bar/"]
    );
    assert_eq!(
        ok("url_seed_multi_space_nolist.torrent").web_seeds,
        ["http://test.com/test%20file/foo%20bar/"]
    );
    assert_eq!(ok("duplicate_web_seeds.torrent").web_seeds.len(), 3);
    assert_eq!(
        ok("dht_nodes.torrent").nodes,
        [
            ("127.0.0.1".to_owned(), 6881),
            ("192.168.1.1".to_owned(), 6881)
        ]
    );
    assert_eq!(
        ok("large_piece_size.torrent").info.piece_length,
        32767 * 0x4000
    );
    for name in ["similar.torrent", "similar2.torrent"] {
        let t = ok(name);
        assert_eq!(t.info.similar.len(), 1);
        assert_eq!(t.info.similar[0].0, *b"aaaaaaaaaaaaaaaaaaaa");
    }
    for name in ["collection.torrent", "collection2.torrent"] {
        assert_eq!(ok(name).info.collections, ["bar", "foo"]);
    }
}

#[test]
#[ignore = "debugging aid"]
fn show() {
    let name = std::env::var("FIXTURE").unwrap();
    println!("{:#?}", load(&name));
}

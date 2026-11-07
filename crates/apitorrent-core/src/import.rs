//! Turning what [`apitorrent_deluge`] read into torrents to add.

use std::path::{Path, PathBuf};

use apitorrent_bencode::OwnedValue;
use apitorrent_engine::bitfield::Bitfield;
use apitorrent_metainfo::Torrent;
use apitorrent_metainfo::sanitize::Naming;

/// Replace a `.torrent` file's trackers, keeping the info dictionary byte for byte (so the
/// info-hash doesn't change).
pub fn with_trackers(bytes: &[u8], tiers: &[Vec<String>]) -> Option<Vec<u8>> {
    let v = apitorrent_bencode::decode_with(bytes, &apitorrent_bencode::DecodeOptions::lenient())
        .ok()?;
    let root = v.as_dict()?;
    let mut out = OwnedValue::dict();
    for (k, val) in root.iter() {
        if k == b"announce" || k == b"announce-list" {
            continue;
        }
        let owned = if k == b"info" {
            OwnedValue::Raw(val.as_dict()?.raw().to_vec())
        } else {
            val.to_owned_value()
        };
        out = out.insert(k.to_vec(), owned);
    }
    if let Some(first) = tiers.iter().flatten().next() {
        out = out.insert("announce", first.as_str());
        let list: Vec<OwnedValue> = tiers
            .iter()
            .filter(|t| !t.is_empty())
            .map(|t| OwnedValue::List(t.iter().map(|u| OwnedValue::from(u.as_str())).collect()))
            .collect();
        out = out.insert("announce-list", OwnedValue::List(list));
    }
    Some(out.to_bytes())
}

/// Rewrite a path saved by Deluge with the first matching prefix mapping: Deluge may have
/// run on another machine, or with the downloads mounted somewhere else.
pub fn remap(path: &str, map: &[(String, String)]) -> String {
    let norm = |s: &str| s.replace('\\', "/");
    let p = norm(path);
    for (from, to) in map {
        let from = norm(from);
        let from = from.trim_end_matches('/');
        if p == from || p.starts_with(&format!("{from}/")) {
            return format!("{}{}", to.trim_end_matches(['/', '\\']), &p[from.len()..]);
        }
    }
    path.to_owned()
}

/// Where each of a Deluge torrent's files is, relative to its save path. A renamed file
/// keeps its Deluge name. Otherwise libtorrent's layout is matched: the portable name if
/// that file exists, else the native one (libtorrent only changes what the OS forbids).
/// Returns `None` when that's just the portable layout.
pub fn file_layout<S: std::hash::BuildHasher>(
    torrent: &Torrent,
    save_path: &Path,
    renamed: &std::collections::HashMap<usize, String, S>,
) -> Option<Vec<Vec<String>>> {
    let portable: Vec<Vec<String>> = torrent.info.files.iter().map(|f| f.path.clone()).collect();
    let native = torrent
        .info
        .paths(Naming::Native {
            case_insensitive: cfg!(any(windows, target_os = "macos")),
        })
        .unwrap_or_else(|_| portable.clone());
    let exists = |rel: &[String]| {
        rel.iter()
            .fold(save_path.to_path_buf(), |p, c| p.join(c))
            .exists()
    };
    let mut out = Vec::with_capacity(portable.len());
    for (i, f) in torrent.info.files.iter().enumerate() {
        let chosen = if let Some(name) = renamed.get(&i).filter(|_| !f.is_pad()) {
            let parts: Vec<String> = name
                .split('/')
                .filter(|c| !c.is_empty() && *c != "." && *c != "..")
                .map(str::to_owned)
                .collect();
            if parts.is_empty() {
                portable[i].clone()
            } else {
                parts
            }
        } else if exists(&portable[i]) || !exists(&native[i]) {
            portable[i].clone()
        } else {
            native[i].clone()
        };
        out.push(chosen);
    }
    (out != portable).then_some(out)
}

/// libtorrent's resume `pieces` (one byte per piece, non-zero for had) as a bitfield.
pub fn pieces_bitfield(pieces: &[u8], count: u32) -> Option<Bitfield> {
    if pieces.len() != count as usize {
        return None;
    }
    let mut b = Bitfield::new(count);
    for (i, &p) in pieces.iter().enumerate() {
        if p != 0 {
            b.set(u32::try_from(i).ok()?);
        }
    }
    Some(b)
}

/// The absolute save path for an imported torrent.
pub fn save_path(
    deluge: Option<&str>,
    default: Option<&str>,
    ours: &Path,
    map: &[(String, String)],
) -> PathBuf {
    deluge
        .or(default)
        .map(|p| PathBuf::from(remap(p, map)))
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| ours.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remapping() {
        let map = vec![("/srv/torrents".to_owned(), "D:\\torrents".to_owned())];
        assert_eq!(remap("/srv/torrents/iso", &map), "D:\\torrents/iso");
        assert_eq!(remap("/srv/torrents", &map), "D:\\torrents");
        assert_eq!(remap("/srv/torrentsx", &map), "/srv/torrentsx");
        assert_eq!(remap("/elsewhere", &map), "/elsewhere");
    }

    #[test]
    fn trackers_change_but_the_hash_doesnt() {
        use apitorrent_metainfo::{FileSource, TorrentBuilder, Version};
        let bytes = TorrentBuilder::new("x", Version::V1)
            .piece_length(16 * 1024)
            .tracker_tier(vec!["http://old/announce".into()])
            .file(FileSource::from_bytes(&[], vec![1; 100]))
            .build()
            .unwrap();
        let before = Torrent::from_bytes(&bytes).unwrap();
        let tiers = vec![
            vec!["https://a/announce".to_owned()],
            vec!["udp://b:1337".to_owned()],
        ];
        let after = Torrent::from_bytes(&with_trackers(&bytes, &tiers).unwrap()).unwrap();
        assert_eq!(after.info.info_hashes(), before.info.info_hashes());
        assert_eq!(after.trackers, tiers);
    }

    #[test]
    fn piece_bytes() {
        let b = pieces_bitfield(&[1, 0, 1], 3).unwrap();
        assert!(b.get(0) && !b.get(1) && b.get(2));
        assert!(pieces_bitfield(&[1, 0], 3).is_none());
    }
}

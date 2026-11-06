//! Creating torrents: v1, v2 or hybrid.

use std::io::{self, Read};

use apitorrent_bencode::OwnedValue;
use sha1::Digest as _;

use crate::hash::Sha256Hash;
use crate::info::Version;
use crate::merkle::{self, FileHasher};

/// One input file.
pub struct FileSource {
    /// Path components inside the torrent. For a single-file torrent, leave this empty: the
    /// file is named after the torrent.
    pub path: Vec<String>,
    pub length: u64,
    pub reader: Box<dyn Read + Send>,
    pub executable: bool,
}

impl std::fmt::Debug for FileSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileSource")
            .field("path", &self.path)
            .field("length", &self.length)
            .finish_non_exhaustive()
    }
}

impl FileSource {
    /// A file held in memory. Mostly for tests.
    pub fn from_bytes(path: &[&str], data: Vec<u8>) -> Self {
        Self {
            path: path.iter().map(|s| (*s).to_owned()).collect(),
            length: data.len() as u64,
            reader: Box::new(io::Cursor::new(data)),
            executable: false,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    #[error("no files")]
    NoFiles,
    #[error("piece length must be a power of two of at least 16 KiB")]
    BadPieceLength,
    #[error("every file in a multi-file torrent needs a path")]
    MissingPath,
    #[error("`{0}` was shorter than its declared length")]
    ShortRead(String),
    #[error("reading input: {0}")]
    Io(#[from] io::Error),
}

/// Builds a `.torrent` file.
#[derive(Debug)]
pub struct TorrentBuilder {
    name: String,
    version: Version,
    piece_length: u64,
    files: Vec<FileSource>,
    trackers: Vec<Vec<String>>,
    web_seeds: Vec<String>,
    private: bool,
    source: Option<String>,
    comment: Option<String>,
    created_by: Option<String>,
    creation_date: Option<i64>,
}

impl TorrentBuilder {
    pub fn new(name: impl Into<String>, version: Version) -> Self {
        Self {
            name: name.into(),
            version,
            piece_length: 256 * 1024,
            files: Vec::new(),
            trackers: Vec::new(),
            web_seeds: Vec::new(),
            private: false,
            source: None,
            comment: None,
            created_by: Some(concat!("apitorrent ", env!("CARGO_PKG_VERSION")).to_owned()),
            creation_date: None,
        }
    }

    /// A piece length suited to the total size: about 1,000–2,000 pieces, between 16 KiB
    /// and 16 MiB.
    pub fn auto_piece_length(total: u64) -> u64 {
        let target = total / 1500;
        target
            .next_power_of_two()
            .clamp(merkle::BLOCK_SIZE, 16 * 1024 * 1024)
    }

    #[must_use]
    pub fn piece_length(mut self, len: u64) -> Self {
        self.piece_length = len;
        self
    }

    #[must_use]
    pub fn file(mut self, file: FileSource) -> Self {
        self.files.push(file);
        self
    }

    #[must_use]
    pub fn tracker_tier(mut self, urls: Vec<String>) -> Self {
        self.trackers.push(urls);
        self
    }

    #[must_use]
    pub fn web_seed(mut self, url: impl Into<String>) -> Self {
        self.web_seeds.push(url.into());
        self
    }

    #[must_use]
    pub fn private(mut self, private: bool) -> Self {
        self.private = private;
        self
    }

    #[must_use]
    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    #[must_use]
    pub fn comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = Some(comment.into());
        self
    }

    #[must_use]
    pub fn creation_date(mut self, seconds: i64) -> Self {
        self.creation_date = Some(seconds);
        self
    }

    /// Hash every file and produce the encoded `.torrent`.
    pub fn build(self) -> Result<Vec<u8>, CreateError> {
        if self.files.is_empty() {
            return Err(CreateError::NoFiles);
        }
        if !self.piece_length.is_power_of_two() || self.piece_length < merkle::BLOCK_SIZE {
            return Err(CreateError::BadPieceLength);
        }
        // One file with an empty path is a single-file torrent. One file with a path is a
        // multi-file layout that happens to hold one file.
        let single = self.files.len() == 1 && self.files[0].path.is_empty();
        if !single && self.files.iter().any(|f| f.path.is_empty()) {
            return Err(CreateError::MissingPath);
        }
        let single_length = self.files[0].length;

        let v1 = matches!(self.version, Version::V1 | Version::Hybrid);
        let v2 = matches!(self.version, Version::V2 | Version::Hybrid);
        let mut files = self.files;
        if v2 {
            // A v2 file tree is a nest of sorted dictionaries, so its files come out in path
            // order. A hybrid torrent's v1 list must match that order (BEP 52), and the piece
            // layout follows it.
            files.sort_by(|a, b| a.path.cmp(&b.path));
        }
        let pad = self.version == Version::Hybrid;
        let piece_length = self.piece_length;

        let mut v1_hasher = V1Hasher::new(piece_length);
        let mut v1_files = Vec::new();
        let mut v2_files: Vec<(Vec<String>, u64, Sha256Hash, bool)> = Vec::new();
        let mut layers: Vec<(Sha256Hash, Vec<Sha256Hash>)> = Vec::new();
        let count = files.len();

        let mut buf = vec![0u8; 1 << 20];
        for (i, mut file) in files.into_iter().enumerate() {
            let mut file_hasher = FileHasher::new();
            let mut remaining = file.length;
            while remaining > 0 {
                let want = usize::try_from(remaining.min(buf.len() as u64)).unwrap_or(buf.len());
                let n = file.reader.read(&mut buf[..want])?;
                if n == 0 {
                    return Err(CreateError::ShortRead(file.path.join("/")));
                }
                if v1 {
                    v1_hasher.update(&buf[..n]);
                }
                if v2 {
                    file_hasher.update(&buf[..n]);
                }
                remaining -= n as u64;
            }
            let attr = if file.executable { Some("x") } else { None };
            if v1 {
                let mut entry = OwnedValue::dict().insert("length", to_i64(file.length));
                if !single {
                    entry = entry.insert(
                        "path",
                        file.path
                            .iter()
                            .map(|c| OwnedValue::from(c.as_str()))
                            .collect::<Vec<_>>(),
                    );
                }
                if let Some(a) = attr {
                    entry = entry.insert("attr", a);
                }
                v1_files.push(entry);
            }
            if v2 {
                let (root, layer) = file_hasher.finish(piece_length);
                if !layer.is_empty() {
                    layers.push((root, layer));
                }
                v2_files.push((file.path.clone(), file.length, root, file.executable));
            }
            if pad && i + 1 < count {
                let total_rem = v1_hasher.position() % piece_length;
                if total_rem != 0 && file.length > 0 {
                    let pad_len = piece_length - total_rem;
                    v1_hasher.update_zeros(pad_len);
                    v1_files.push(
                        OwnedValue::dict()
                            .insert("length", to_i64(pad_len))
                            .insert("attr", "p")
                            .insert(
                                "path",
                                vec![
                                    OwnedValue::from(".pad"),
                                    OwnedValue::from(pad_len.to_string()),
                                ],
                            ),
                    );
                }
            }
        }

        let mut info = OwnedValue::dict()
            .insert("name", self.name.as_str())
            .insert("piece length", to_i64(piece_length));
        if v1 {
            info = info.insert("pieces", v1_hasher.finish());
            if single {
                info = info.insert("length", to_i64(single_length));
            } else {
                info = info.insert("files", v1_files);
            }
        }
        if v2 {
            info = info
                .insert("meta version", 2i64)
                .insert("file tree", build_file_tree(&v2_files, single, &self.name));
        }
        if self.private {
            info = info.insert("private", 1i64);
        }
        if let Some(s) = &self.source {
            info = info.insert("source", s.as_str());
        }

        let info_bytes = info.to_bytes();
        let mut root = OwnedValue::dict().insert("info", OwnedValue::Raw(info_bytes));
        if let Some(first) = self.trackers.first().and_then(|t| t.first()) {
            root = root.insert("announce", first.as_str());
        }
        if self.trackers.len() > 1 || self.trackers.first().is_some_and(|t| t.len() > 1) {
            root = root.insert(
                "announce-list",
                self.trackers
                    .iter()
                    .map(|tier| {
                        OwnedValue::List(
                            tier.iter().map(|u| OwnedValue::from(u.as_str())).collect(),
                        )
                    })
                    .collect::<Vec<_>>(),
            );
        }
        if !self.web_seeds.is_empty() {
            root = root.insert(
                "url-list",
                self.web_seeds
                    .iter()
                    .map(|u| OwnedValue::from(u.as_str()))
                    .collect::<Vec<_>>(),
            );
        }
        if let Some(c) = &self.comment {
            root = root.insert("comment", c.as_str());
        }
        if let Some(c) = &self.created_by {
            root = root.insert("created by", c.as_str());
        }
        if let Some(d) = self.creation_date {
            root = root.insert("creation date", d);
        }
        if v2 && !layers.is_empty() {
            let mut map = OwnedValue::dict();
            for (root_hash, layer) in layers {
                let bytes: Vec<u8> = layer.iter().flat_map(|h| h.0).collect();
                map = map.insert(root_hash.0.to_vec(), bytes);
            }
            root = root.insert("piece layers", map);
        }
        Ok(root.to_bytes())
    }
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).expect("lengths fit in i64")
}

fn build_file_tree(
    files: &[(Vec<String>, u64, Sha256Hash, bool)],
    single: bool,
    name: &str,
) -> OwnedValue {
    let mut tree = OwnedValue::dict();
    for (path, length, root, executable) in files {
        let path: Vec<String> = if single {
            vec![name.to_owned()]
        } else {
            path.clone()
        };
        let mut leaf = OwnedValue::dict().insert("length", to_i64(*length));
        if *length > 0 {
            leaf = leaf.insert("pieces root", root.0.to_vec());
        }
        if *executable {
            leaf = leaf.insert("attr", "x");
        }
        insert_path(&mut tree, &path, OwnedValue::dict().insert("", leaf));
    }
    tree
}

fn insert_path(node: &mut OwnedValue, path: &[String], leaf: OwnedValue) {
    let OwnedValue::Dict(map) = node else {
        unreachable!("file tree nodes are dictionaries")
    };
    let key = path[0].as_bytes().to_vec();
    if path.len() == 1 {
        map.insert(key, leaf);
    } else {
        let child = map.entry(key).or_insert_with(OwnedValue::dict);
        insert_path(child, &path[1..], leaf);
    }
}

/// SHA-1 over the concatenated piece space.
struct V1Hasher {
    piece_length: u64,
    current: sha1::Sha1,
    in_piece: u64,
    position: u64,
    out: Vec<u8>,
}

impl V1Hasher {
    fn new(piece_length: u64) -> Self {
        Self {
            piece_length,
            current: sha1::Sha1::new(),
            in_piece: 0,
            position: 0,
            out: Vec::new(),
        }
    }

    fn position(&self) -> u64 {
        self.position
    }

    fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            let room = self.piece_length - self.in_piece;
            let take = usize::try_from(room.min(data.len() as u64)).unwrap_or(data.len());
            self.current.update(&data[..take]);
            self.in_piece += take as u64;
            self.position += take as u64;
            data = &data[take..];
            if self.in_piece == self.piece_length {
                let digest = std::mem::take(&mut self.current).finalize();
                self.out.extend_from_slice(&digest);
                self.in_piece = 0;
            }
        }
    }

    fn update_zeros(&mut self, mut len: u64) {
        let zeros = [0u8; 16 * 1024];
        while len > 0 {
            let n = usize::try_from(len.min(zeros.len() as u64)).unwrap_or(zeros.len());
            self.update(&zeros[..n]);
            len -= n as u64;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.in_piece > 0 {
            let digest = self.current.finalize();
            self.out.extend_from_slice(&digest);
        }
        std::mem::take(&mut self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Torrent, Version};

    fn data(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| {
                u8::try_from(i % 256)
                    .unwrap()
                    .wrapping_mul(31)
                    .wrapping_add(seed)
            })
            .collect()
    }

    fn multi(version: Version) -> Torrent {
        let bytes = TorrentBuilder::new("pack", version)
            .piece_length(32 * 1024)
            .file(FileSource::from_bytes(&["a.bin"], data(100_000, 1)))
            .file(FileSource::from_bytes(&["sub", "b.bin"], data(5_000, 2)))
            .file(FileSource::from_bytes(&["sub", "empty"], Vec::new()))
            .file(FileSource::from_bytes(&["z.bin"], data(70_000, 3)))
            .tracker_tier(vec!["udp://tracker.example:6969".into()])
            .build()
            .unwrap();
        Torrent::from_bytes(&bytes).unwrap()
    }

    #[test]
    fn every_version_parses_back() {
        for version in [Version::V1, Version::V2, Version::Hybrid] {
            let t = multi(version);
            assert_eq!(t.info.version(), version);
            assert_eq!(t.info.content_length(), 175_000);
            let paths: Vec<String> = t
                .info
                .files
                .iter()
                .filter(|f| !f.is_pad())
                .map(crate::FileEntry::display_path)
                .collect();
            assert_eq!(
                paths,
                [
                    "pack/a.bin",
                    "pack/sub/b.bin",
                    "pack/sub/empty",
                    "pack/z.bin"
                ],
                "{version:?}"
            );
            if version != Version::V1 {
                assert!(t.has_all_piece_layers());
            }
            assert_eq!(t.trackers, [["udp://tracker.example:6969"]]);
        }
    }

    #[test]
    fn hybrid_and_v2_layouts_agree() {
        let hybrid = multi(Version::Hybrid);
        let v2 = multi(Version::V2);
        let layout = |t: &Torrent| {
            t.info
                .files
                .iter()
                .map(|f| (f.offset, f.length, f.is_pad()))
                .collect::<Vec<_>>()
        };
        assert_eq!(layout(&hybrid), layout(&v2));
        assert_eq!(hybrid.info.piece_count(), v2.info.piece_count());
    }

    #[test]
    fn hybrid_files_follow_tree_order() {
        // Given out of order, with an empty file sorting before a non-empty one.
        let bytes = TorrentBuilder::new("pack", Version::Hybrid)
            .piece_length(16 * 1024)
            .file(FileSource::from_bytes(&["z.bin"], data(20_000, 1)))
            .file(FileSource::from_bytes(
                &["dir", "small.txt"],
                data(7_000, 2),
            ))
            .file(FileSource::from_bytes(&["dir", "empty"], Vec::new()))
            .build()
            .unwrap();
        let t = Torrent::from_bytes(&bytes).unwrap();
        let paths: Vec<String> = t
            .info
            .files
            .iter()
            .filter(|f| !f.is_pad())
            .map(crate::FileEntry::display_path)
            .collect();
        assert_eq!(
            paths,
            ["pack/dir/empty", "pack/dir/small.txt", "pack/z.bin"]
        );
    }

    #[test]
    fn single_file() {
        for version in [Version::V1, Version::V2, Version::Hybrid] {
            let bytes = TorrentBuilder::new("image.iso", version)
                .piece_length(16 * 1024)
                .file(FileSource::from_bytes(&[], data(40_000, 9)))
                .build()
                .unwrap();
            let t = Torrent::from_bytes(&bytes).unwrap();
            assert!(t.info.is_single_file(), "{version:?}");
            assert_eq!(t.info.files[0].path, ["image.iso"]);
            assert_eq!(t.info.piece_count(), 3);
        }
    }

    #[test]
    fn auto_piece_length() {
        assert_eq!(TorrentBuilder::auto_piece_length(1000), 16 * 1024);
        assert_eq!(TorrentBuilder::auto_piece_length(4 << 30), 4 << 20);
        assert_eq!(TorrentBuilder::auto_piece_length(1 << 50), 16 << 20);
    }
}

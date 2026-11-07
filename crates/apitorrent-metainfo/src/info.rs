//! The info dictionary: the part of a torrent covered by its info-hash.

use std::collections::HashSet;
use std::sync::Arc;

use apitorrent_bencode::{Dict, Value};

use crate::error::MetainfoError;
use crate::hash::{InfoHashes, Sha1Hash, Sha256Hash};
use crate::merkle;
use crate::sanitize::{self, Naming};

/// Limits that bound the memory and work a torrent can demand. The defaults are far beyond
/// any real torrent, and still small enough that a hostile one can't exhaust a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_files: usize,
    pub max_pieces: u64,
    pub max_total_length: u64,
    pub max_piece_length: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_files: 1 << 20,
            max_pieces: 1 << 24,
            max_total_length: 1 << 52,
            max_piece_length: 1 << 29,
        }
    }
}

/// Which protocol versions a torrent supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Version {
    V1,
    V2,
    Hybrid,
}

/// BEP 47 file attributes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[allow(clippy::struct_excessive_bools)]
pub struct FileAttributes {
    pub pad: bool,
    pub executable: bool,
    pub hidden: bool,
    pub symlink: bool,
}

impl FileAttributes {
    fn parse(attr: &[u8]) -> Self {
        Self {
            pad: attr.contains(&b'p'),
            executable: attr.contains(&b'x'),
            hidden: attr.contains(&b'h'),
            symlink: attr.contains(&b'l'),
        }
    }
}

/// One file of a torrent, in torrent order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// Sanitised path components, relative to the download directory. For a multi-file
    /// torrent the first component is the torrent's name.
    pub path: Vec<String>,
    pub length: u64,
    /// The file's byte offset in the torrent's piece space.
    pub offset: u64,
    pub attributes: FileAttributes,
    /// For symlinks: the target, as sanitised components relative to the torrent root.
    pub symlink_target: Option<Vec<String>>,
    /// The BEP 52 merkle root. Present for every non-empty file of a v2 or hybrid torrent.
    pub pieces_root: Option<Sha256Hash>,
    /// BEP 47 `sha1` of the whole file, when the creator included it.
    pub sha1: Option<Sha1Hash>,
}

impl FileEntry {
    pub fn is_pad(&self) -> bool {
        self.attributes.pad
    }

    /// The path joined with `/`, for display and logs.
    pub fn display_path(&self) -> String {
        self.path.join("/")
    }

    /// The end of the file in piece space (exclusive).
    pub fn end(&self) -> u64 {
        self.offset + self.length
    }
}

/// A parsed, validated info dictionary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    raw: Arc<[u8]>,
    info_hashes: InfoHashes,
    version: Version,
    /// The display name, sanitised. The torrent's info-hash if the name was empty or
    /// unusable.
    pub name: String,
    pub piece_length: u64,
    pub files: Vec<FileEntry>,
    /// v1 piece hashes, one per piece. Empty for v2-only torrents.
    pub v1_pieces: Arc<[Sha1Hash]>,
    pub private: bool,
    /// The `source` tag that private trackers use to make cross-seeded torrents unique.
    pub source: Option<String>,
    /// BEP 38.
    pub similar: Vec<Sha1Hash>,
    /// BEP 38.
    pub collections: Vec<String>,
    total_length: u64,
    piece_count: u64,
}

impl Info {
    /// Parse an info dictionary on its own: from BEP 9 metadata exchange, or from storage.
    pub fn from_bytes(raw: &[u8], limits: &Limits) -> Result<Self, MetainfoError> {
        let value =
            apitorrent_bencode::decode_with(raw, &apitorrent_bencode::DecodeOptions::lenient())?;
        let dict = value.as_dict().ok_or(MetainfoError::MissingInfo)?;
        Self::from_dict(dict, limits)
    }

    pub(crate) fn from_dict(info: &Dict<'_>, limits: &Limits) -> Result<Self, MetainfoError> {
        Self::from_dict_with(info, limits, Naming::Portable)
    }

    /// Every file's path under other naming rules, in the same order as
    /// [`Info::files`]. Used for native layouts and for web seed URLs.
    pub fn paths(&self, naming: Naming) -> Result<Vec<Vec<String>>, MetainfoError> {
        let value = apitorrent_bencode::decode_with(
            &self.raw,
            &apitorrent_bencode::DecodeOptions::lenient(),
        )?;
        let dict = value.as_dict().ok_or(MetainfoError::MissingInfo)?;
        let limits = Limits {
            max_files: usize::MAX,
            max_total_length: u64::MAX,
            max_pieces: u64::MAX,
            ..Limits::default()
        };
        let other = Self::from_dict_with(dict, &limits, naming)?;
        Ok(other.files.into_iter().map(|f| f.path).collect())
    }

    fn from_dict_with(
        info: &Dict<'_>,
        limits: &Limits,
        naming: Naming,
    ) -> Result<Self, MetainfoError> {
        let raw = info.raw();

        let meta_version = info.get(b"meta version");
        let has_v2 = match meta_version {
            None => false,
            Some(Value::Int(2)) => true,
            Some(Value::Int(v)) => return Err(MetainfoError::UnsupportedVersion(*v)),
            Some(_) => return Err(MetainfoError::Field("meta version")),
        };
        let has_v1 = info.contains_key(b"pieces");
        if !has_v1 && !has_v2 {
            return Err(MetainfoError::Field("pieces"));
        }
        let version = match (has_v1, has_v2) {
            (true, true) => Version::Hybrid,
            (true, false) => Version::V1,
            _ => Version::V2,
        };

        let info_hashes = InfoHashes::new(
            has_v1.then(|| Sha1Hash::digest(raw)),
            has_v2.then(|| Sha256Hash::digest(raw)),
        )
        .expect("at least one version is present");

        let piece_length = parse_piece_length(info, has_v2, limits)?;

        let name_raw = info
            .get_bytes(b"name.utf-8")
            .or_else(|| info.get_bytes(b"name"))
            .ok_or(MetainfoError::Field("name"))?;
        let name = if name_raw.iter().all(|&b| b == b' ' || b == b'.') {
            info_hashes.id_string()
        } else {
            naming.component(name_raw)
        };

        let v2_files = if has_v2 {
            let tree = info
                .get_dict(b"file tree")
                .ok_or(MetainfoError::Field("file tree"))?;
            Some(parse_file_tree(tree, &name, naming, limits)?)
        } else {
            None
        };
        let v1_files = if has_v1 {
            Some(parse_v1_files(info, &name, naming, limits)?)
        } else {
            None
        };

        let mut files = match (v1_files, v2_files) {
            (Some(v1), Some(v2)) => merge_hybrid(v1, &v2, piece_length)?,
            (Some(v1), None) => v1,
            (None, Some(v2)) => align_v2(v2, piece_length),
            (None, None) => unreachable!(),
        };
        if files.iter().all(FileEntry::is_pad) {
            return Err(MetainfoError::NoFiles);
        }
        if files.len() > limits.max_files {
            return Err(MetainfoError::TooManyFiles(limits.max_files));
        }

        let mut offset = 0u64;
        for file in &mut files {
            file.offset = offset;
            offset = offset
                .checked_add(file.length)
                .filter(|&o| o <= limits.max_total_length)
                .ok_or(MetainfoError::TooLarge(limits.max_total_length))?;
        }
        let total_length = offset;
        if total_length == 0 {
            return Err(MetainfoError::InvalidLength {
                path: name.clone(),
                length: 0,
            });
        }
        let piece_count = total_length.div_ceil(piece_length);
        if piece_count > limits.max_pieces {
            return Err(MetainfoError::TooManyPieces(limits.max_pieces));
        }

        let v1_pieces: Arc<[Sha1Hash]> = if has_v1 {
            let pieces = info
                .get_bytes(b"pieces")
                .ok_or(MetainfoError::Field("pieces"))?;
            if pieces.len() % 20 != 0 {
                return Err(MetainfoError::UnalignedPieces);
            }
            let found = (pieces.len() / 20) as u64;
            if found != piece_count {
                return Err(MetainfoError::PieceCountMismatch {
                    expected: piece_count,
                    found,
                });
            }
            pieces
                .as_chunks::<20>()
                .0
                .iter()
                .map(|c| Sha1Hash::from_slice(c).expect("chunk is 20 bytes"))
                .collect()
        } else {
            Arc::from([])
        };

        disambiguate(&mut files, naming);
        check_roots(&files)?;

        let private = info.get_int(b"private") == Some(1);
        let source = info.get_str(b"source").map(str::to_owned);
        let (similar, collections) = parse_bep38(info);

        Ok(Self {
            raw: Arc::from(raw),
            info_hashes,
            version,
            name,
            piece_length,
            files,
            v1_pieces,
            private,
            source,
            similar,
            collections,
            total_length,
            piece_count,
        })
    }

    /// The exact bytes of the info dictionary. Served to peers over BEP 9.
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    pub fn info_hashes(&self) -> InfoHashes {
        self.info_hashes
    }

    pub fn version(&self) -> Version {
        self.version
    }

    pub fn has_v1(&self) -> bool {
        matches!(self.version, Version::V1 | Version::Hybrid)
    }

    pub fn has_v2(&self) -> bool {
        matches!(self.version, Version::V2 | Version::Hybrid)
    }

    /// Total size of the piece space, including pad files.
    pub fn total_length(&self) -> u64 {
        self.total_length
    }

    /// Total size of the real (non-pad) files.
    pub fn content_length(&self) -> u64 {
        self.files
            .iter()
            .filter(|f| !f.is_pad())
            .map(|f| f.length)
            .sum()
    }

    pub fn piece_count(&self) -> u64 {
        self.piece_count
    }

    /// The length of piece `index`. Only the last piece can be short.
    pub fn piece_size(&self, index: u64) -> u64 {
        if index + 1 == self.piece_count {
            self.total_length - index * self.piece_length
        } else {
            self.piece_length
        }
    }

    /// Whether the torrent has a single file that is not inside a directory.
    pub fn is_single_file(&self) -> bool {
        let mut real = self.files.iter().filter(|f| !f.is_pad());
        matches!((real.next(), real.next()), (Some(f), None) if f.path.len() == 1)
    }

    /// Check a v2 piece layer for file `index` (received from a peer, or from a `.torrent`).
    pub fn validate_piece_layer(
        &self,
        index: usize,
        layer: &[Sha256Hash],
    ) -> Result<(), MetainfoError> {
        let file = self
            .files
            .get(index)
            .ok_or_else(|| MetainfoError::InvalidPieceLayer("no such file".into()))?;
        let root = file
            .pieces_root
            .ok_or_else(|| MetainfoError::InvalidPieceLayer("file has no pieces root".into()))?;
        let expected = file.length.div_ceil(self.piece_length);
        if layer.len() as u64 != expected {
            return Err(MetainfoError::InvalidPieceLayer(format!(
                "`{}` needs {expected} hashes, got {}",
                file.display_path(),
                layer.len()
            )));
        }
        if merkle::root_from_piece_layer(layer, file.length, self.piece_length) != root {
            return Err(MetainfoError::InvalidPieceLayer(format!(
                "`{}` layer does not match its pieces root",
                file.display_path()
            )));
        }
        Ok(())
    }
}

fn parse_piece_length(info: &Dict<'_>, v2: bool, limits: &Limits) -> Result<u64, MetainfoError> {
    let raw = info
        .get_int(b"piece length")
        .ok_or(MetainfoError::Field("piece length"))?;
    let len = u64::try_from(raw).map_err(|_| MetainfoError::InvalidPieceLength(raw))?;
    let valid = len > 0
        && len <= limits.max_piece_length
        && (!v2 || (len.is_power_of_two() && len >= merkle::BLOCK_SIZE));
    if !valid {
        return Err(MetainfoError::InvalidPieceLength(raw));
    }
    Ok(len)
}

fn file_length(dict: &Dict<'_>, path: &[String]) -> Result<u64, MetainfoError> {
    let raw = dict
        .get_int(b"length")
        .ok_or(MetainfoError::Field("length"))?;
    u64::try_from(raw).map_err(|_| MetainfoError::InvalidLength {
        path: path.join("/"),
        length: raw,
    })
}

fn parse_components(list: &apitorrent_bencode::List<'_>, naming: Naming) -> Option<Vec<String>> {
    let mut out = Vec::with_capacity(list.len());
    for item in list {
        let bytes = item.as_bytes()?;
        // Empty components are meaningless (`a//b`), so they are dropped.
        if !bytes.is_empty() {
            out.push(naming.component(bytes));
        }
    }
    Some(out)
}

fn parse_v1_files(
    info: &Dict<'_>,
    name: &str,
    naming: Naming,
    limits: &Limits,
) -> Result<Vec<FileEntry>, MetainfoError> {
    let Some(list) = info.get(b"files") else {
        let path = vec![name.to_owned()];
        let length = file_length(info, &path)?;
        if length == 0 {
            return Err(MetainfoError::InvalidLength {
                path: name.to_owned(),
                length: 0,
            });
        }
        return Ok(vec![FileEntry {
            path,
            length,
            offset: 0,
            attributes: FileAttributes::parse(info.get_bytes(b"attr").unwrap_or_default()),
            symlink_target: None,
            pieces_root: None,
            sha1: info.get_bytes(b"sha1").and_then(Sha1Hash::from_slice),
        }]);
    };
    let list = list.as_list().ok_or(MetainfoError::Field("files"))?;
    if list.len() > limits.max_files {
        return Err(MetainfoError::TooManyFiles(limits.max_files));
    }
    let mut files = Vec::with_capacity(list.len());
    for entry in list {
        let dict = entry.as_dict().ok_or(MetainfoError::Field("files"))?;
        let mut attributes = FileAttributes::parse(dict.get_bytes(b"attr").unwrap_or_default());
        let path_value = dict.get(b"path.utf-8").or_else(|| dict.get(b"path"));
        let mut path = vec![name.to_owned()];
        match path_value {
            Some(Value::List(l)) => {
                path.extend(parse_components(l, naming).ok_or(MetainfoError::Field("path"))?);
            }
            Some(_) => return Err(MetainfoError::Field("path")),
            // BEP 47 lets pad files omit their path.
            None if attributes.pad => {}
            None => return Err(MetainfoError::Field("path")),
        }
        // BitComet marked pad files by name before BEP 47 defined the attribute.
        if path
            .last()
            .is_some_and(|c| c.starts_with("_____padding_file_"))
        {
            attributes.pad = true;
        }
        let length = if attributes.symlink {
            // A symlink has no content in piece space, whatever length it claims, and BEP 47
            // lets it omit the length entirely.
            0
        } else {
            file_length(dict, &path)?
        };
        if attributes.pad {
            // Pad files are never written to disk, so their names don't matter. A uniform
            // name keeps them out of the way of real files.
            path = vec![".pad".to_owned(), length.to_string()];
        } else if path.len() == 1 {
            // Every component was empty.
            path.push("_".to_owned());
        }
        let symlink_target = if attributes.symlink {
            Some(parse_symlink(dict, &path, &[name.to_owned()], naming)?)
        } else {
            None
        };
        files.push(FileEntry {
            path,
            length,
            offset: 0,
            attributes,
            symlink_target,
            pieces_root: None,
            sha1: dict.get_bytes(b"sha1").and_then(Sha1Hash::from_slice),
        });
    }
    Ok(files)
}

/// A symlink target, relative to the download directory like file paths are. BEP 47
/// defines it relative to the torrent root, so `root` (the torrent's directory, if it has
/// one) is prepended.
fn parse_symlink(
    dict: &Dict<'_>,
    path: &[String],
    root: &[String],
    naming: Naming,
) -> Result<Vec<String>, MetainfoError> {
    let invalid = || MetainfoError::InvalidSymlink(path.join("/"));
    let list = dict.get_list(b"symlink path").ok_or_else(invalid)?;
    let target = parse_components(list, naming).ok_or_else(invalid)?;
    if target.is_empty() {
        return Err(invalid());
    }
    Ok(root.iter().cloned().chain(target).collect())
}

/// A file from a v2 file tree, before alignment.
struct TreeFile {
    path: Vec<String>,
    length: u64,
    attributes: FileAttributes,
    symlink_target: Option<Vec<String>>,
    pieces_root: Option<Sha256Hash>,
}

fn parse_file_tree(
    tree: &Dict<'_>,
    name: &str,
    naming: Naming,
    limits: &Limits,
) -> Result<Vec<TreeFile>, MetainfoError> {
    if tree.is_empty() {
        return Err(MetainfoError::NoFiles);
    }
    // A tree holding exactly one file at the top level is a single-file torrent: the file
    // is placed directly in the download directory, not under the torrent's name.
    let single = tree.len() == 1
        && tree
            .iter()
            .next()
            .and_then(|(_, node)| node.as_dict())
            .is_some_and(|d| d.contains_key(b""));
    let root = if single {
        Vec::new()
    } else {
        vec![name.to_owned()]
    };
    let mut prefix = root.clone();
    let mut out = Vec::new();
    walk_tree(tree, &mut prefix, &root, &mut out, naming, limits)?;
    Ok(out)
}

fn walk_tree(
    dir: &Dict<'_>,
    prefix: &mut Vec<String>,
    root: &[String],
    out: &mut Vec<TreeFile>,
    naming: Naming,
    limits: &Limits,
) -> Result<(), MetainfoError> {
    if !dir.sorted_in_input() {
        return Err(MetainfoError::FileTree("keys out of order".into()));
    }
    for (key, node) in dir.iter() {
        if key.is_empty() {
            return Err(MetainfoError::FileTree("empty file name".into()));
        }
        let node = node
            .as_dict()
            .ok_or_else(|| MetainfoError::FileTree("node is not a dictionary".into()))?;
        prefix.push(naming.component(key));
        if let Some(leaf) = node.get(b"") {
            if node.len() != 1 {
                return Err(MetainfoError::FileTree(format!(
                    "`{}` is both a file and a directory",
                    prefix.join("/")
                )));
            }
            if out.len() >= limits.max_files {
                return Err(MetainfoError::TooManyFiles(limits.max_files));
            }
            out.push(parse_tree_file(leaf, prefix, root, naming)?);
        } else {
            if node.is_empty() {
                return Err(MetainfoError::FileTree(format!(
                    "`{}` is an empty directory",
                    prefix.join("/")
                )));
            }
            walk_tree(node, prefix, root, out, naming, limits)?;
        }
        prefix.pop();
    }
    Ok(())
}

fn parse_tree_file(
    leaf: &Value<'_>,
    path: &[String],
    root: &[String],
    naming: Naming,
) -> Result<TreeFile, MetainfoError> {
    let display = || path.join("/");
    let dict = leaf
        .as_dict()
        .ok_or_else(|| MetainfoError::FileTree(format!("`{}` is not a dictionary", display())))?;
    let attributes = FileAttributes::parse(dict.get_bytes(b"attr").unwrap_or_default());
    if attributes.pad {
        return Err(MetainfoError::InvalidPadFile(format!(
            "`{}`: pad files are not allowed in a v2 file tree",
            display()
        )));
    }
    let length = if attributes.symlink && !dict.contains_key(b"length") {
        0
    } else {
        file_length(dict, path)?
    };
    let symlink_target = if attributes.symlink {
        Some(parse_symlink(dict, path, root, naming)?)
    } else {
        None
    };
    let pieces_root = match dict.get(b"pieces root") {
        Some(v) => {
            let root = v
                .as_bytes()
                .and_then(Sha256Hash::from_slice)
                .filter(|r| !r.is_zero())
                .ok_or_else(|| MetainfoError::MissingPiecesRoot(display()))?;
            Some(root)
        }
        None => None,
    };
    if length > 0 && !attributes.symlink && pieces_root.is_none() {
        return Err(MetainfoError::MissingPiecesRoot(display()));
    }
    Ok(TreeFile {
        path: path.to_vec(),
        length,
        attributes,
        symlink_target,
        pieces_root,
    })
}

/// Lay out a v2-only torrent in piece space: each file starts on a piece boundary. Explicit
/// pad entries make the layout identical to the equivalent hybrid torrent's, so the rest of
/// the engine never needs to care which kind it has.
fn align_v2(files: Vec<TreeFile>, piece_length: u64) -> Vec<FileEntry> {
    let mut out = Vec::with_capacity(files.len() * 2);
    let mut offset = 0u64;
    let count = files.len();
    for (i, f) in files.into_iter().enumerate() {
        let length = if f.attributes.symlink { 0 } else { f.length };
        out.push(FileEntry {
            path: f.path,
            length,
            offset: 0,
            attributes: f.attributes,
            symlink_target: f.symlink_target,
            pieces_root: f.pieces_root,
            sha1: None,
        });
        offset += length;
        let rem = offset % piece_length;
        if rem != 0 && i + 1 < count {
            let pad = piece_length - rem;
            out.push(pad_entry(pad));
            offset += pad;
        }
    }
    out
}

fn pad_entry(length: u64) -> FileEntry {
    FileEntry {
        path: vec![".pad".to_owned(), length.to_string()],
        length,
        offset: 0,
        attributes: FileAttributes {
            pad: true,
            ..FileAttributes::default()
        },
        symlink_target: None,
        pieces_root: None,
        sha1: None,
    }
}

/// Check that a hybrid torrent's v1 file list describes the same files as its v2 file tree,
/// with pad files exactly where v2 alignment requires them. Returns the v1 list with the v2
/// roots attached.
fn merge_hybrid(
    mut v1: Vec<FileEntry>,
    v2: &[TreeFile],
    piece_length: u64,
) -> Result<Vec<FileEntry>, MetainfoError> {
    let mut real = 0usize;
    let mut offset = 0u64;
    for file in &mut v1 {
        if file.is_pad() {
            let expected = (piece_length - offset % piece_length) % piece_length;
            if offset.is_multiple_of(piece_length) || file.length != expected {
                return Err(MetainfoError::InvalidPadFile(format!(
                    "pad of {} bytes at offset {offset}",
                    file.length
                )));
            }
            offset += file.length;
            continue;
        }
        let other = v2.get(real).ok_or_else(|| {
            MetainfoError::InconsistentHybrid("v1 list has extra files".to_owned())
        })?;
        real += 1;
        let v1_length = if file.attributes.symlink {
            0
        } else {
            file.length
        };
        let v2_length = if other.attributes.symlink {
            0
        } else {
            other.length
        };
        if file.path != other.path || v1_length != v2_length {
            return Err(MetainfoError::InconsistentHybrid(format!(
                "`{}` ({} bytes) vs `{}` ({} bytes)",
                file.display_path(),
                file.length,
                other.path.join("/"),
                other.length
            )));
        }
        if !offset.is_multiple_of(piece_length) && v1_length > 0 {
            return Err(MetainfoError::InconsistentHybrid(format!(
                "`{}` is not aligned to a piece boundary",
                file.display_path()
            )));
        }
        file.pieces_root = other.pieces_root;
        // An unaligned file must be followed by a pad file unless it is the last one (the
        // tail pad is optional). The alignment check on the next file enforces that.
        offset += file.length;
    }
    if real != v2.len() {
        return Err(MetainfoError::InconsistentHybrid(
            "v2 file tree has extra files".to_owned(),
        ));
    }
    Ok(v1)
}

/// Rename files whose paths would collide on a case-insensitive filesystem, or where a
/// file and a directory would need the same path. Later files get a `.N` suffix.
fn disambiguate(files: &mut [FileEntry], naming: Naming) {
    if naming == Naming::Verbatim {
        return;
    }
    let fold = |name: &str| {
        if naming.folds_case() {
            sanitize::fold_case(name)
        } else {
            name.to_owned()
        }
    };
    let mut file_keys: HashSet<String> = HashSet::new();
    let mut dir_keys: HashSet<String> = HashSet::new();
    for file in files.iter_mut().filter(|f| !f.is_pad()) {
        let mut key = String::new();
        let last = file.path.len() - 1;
        for i in 0..=last {
            let base = key.clone();
            let join = |name: &str| {
                if base.is_empty() {
                    fold(name)
                } else {
                    format!("{base}/{}", fold(name))
                }
            };
            let original = file.path[i].clone();
            let mut candidate = original.clone();
            let mut n = 0;
            loop {
                let k = join(&candidate);
                let taken = if i == last {
                    file_keys.contains(&k) || dir_keys.contains(&k)
                } else {
                    file_keys.contains(&k)
                };
                if !taken {
                    key = k;
                    break;
                }
                n += 1;
                candidate = sanitize::with_counter(&original, n);
            }
            file.path[i] = candidate;
            if i == last {
                file_keys.insert(key.clone());
            } else {
                dir_keys.insert(key.clone());
            }
        }
    }
}

/// Files that share a `pieces root` must be identical, so they must have the same length.
fn check_roots(files: &[FileEntry]) -> Result<(), MetainfoError> {
    let mut seen: std::collections::HashMap<Sha256Hash, u64> = std::collections::HashMap::new();
    for f in files {
        if let Some(root) = f.pieces_root {
            if let Some(&len) = seen.get(&root) {
                if len != f.length {
                    return Err(MetainfoError::InconsistentFileRoots);
                }
            } else {
                seen.insert(root, f.length);
            }
        }
    }
    Ok(())
}

fn parse_bep38(dict: &Dict<'_>) -> (Vec<Sha1Hash>, Vec<String>) {
    let similar = dict
        .get_list(b"similar")
        .map(|l| {
            l.iter()
                .filter_map(|v| v.as_bytes().and_then(Sha1Hash::from_slice))
                .collect()
        })
        .unwrap_or_default();
    let collections = dict
        .get_list(b"collections")
        .map(|l| {
            l.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    (similar, collections)
}

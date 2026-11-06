/// Why a `.torrent` file or info dictionary was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MetainfoError {
    #[error("invalid bencoding: {0}")]
    Bencode(#[from] apitorrent_bencode::Error),
    #[error("the torrent is not a dictionary")]
    NotADictionary,
    #[error("missing or invalid `info` dictionary")]
    MissingInfo,
    #[error("missing or invalid `{0}`")]
    Field(&'static str),
    #[error("invalid piece length {0}")]
    InvalidPieceLength(i64),
    #[error("invalid length {length} for `{path}`")]
    InvalidLength { path: String, length: i64 },
    #[error("unsupported meta version {0}")]
    UnsupportedVersion(i64),
    #[error("`pieces` is not a whole number of SHA-1 hashes")]
    UnalignedPieces,
    #[error("expected {expected} piece hashes, found {found}")]
    PieceCountMismatch { expected: u64, found: u64 },
    #[error("the torrent has no files")]
    NoFiles,
    #[error("the torrent has more than {0} files")]
    TooManyFiles(usize),
    #[error("the torrent has more than {0} pieces")]
    TooManyPieces(u64),
    #[error("the torrent's total size exceeds {0} bytes")]
    TooLarge(u64),
    #[error("invalid file tree: {0}")]
    FileTree(String),
    #[error("`{0}` has no valid `pieces root`")]
    MissingPiecesRoot(String),
    #[error("two files share a `pieces root` but differ in length")]
    InconsistentFileRoots,
    #[error("v1 and v2 file lists disagree: {0}")]
    InconsistentHybrid(String),
    #[error("invalid pad file: {0}")]
    InvalidPadFile(String),
    #[error("invalid piece layer: {0}")]
    InvalidPieceLayer(String),
    #[error("invalid symlink target for `{0}`")]
    InvalidSymlink(String),
}

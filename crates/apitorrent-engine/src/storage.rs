//! Where piece data lives.
//!
//! The engine only ever writes a piece after it has been verified, so a file never holds
//! bytes that failed a hash check. That rule is what makes it safe for several torrents to
//! share one file (see the content library in `docs/architecture.md`).

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::layout::{Layout, Segment};

/// Storage for one torrent's piece space. Implementations must be safe to call from several
/// disk threads at once.
pub trait Storage: Send + Sync + std::fmt::Debug {
    /// Fill `buf` with the bytes at `offset` in piece space. Pad files read as zeros.
    fn read(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;

    /// Write a verified piece.
    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()>;

    /// The current size of each file on disk, or `None` where a file doesn't exist. Used for
    /// a quick check of resume data without hashing.
    fn file_sizes(&self) -> Vec<Option<u64>>;

    /// Flush written data to stable storage.
    fn sync(&self) -> io::Result<()>;

    /// Create the torrent's zero-length files. They hold no pieces, so no write ever
    /// creates them.
    fn create_empty_files(&self) -> io::Result<()>;
}

/// Plain files under a download directory.
#[derive(Debug)]
pub struct FileStorage {
    layout: Layout,
    paths: Vec<PathBuf>,
    handles: Vec<Mutex<Option<Arc<File>>>>,
}

impl FileStorage {
    /// `relative` holds each file's path components, as in
    /// [`apitorrent_metainfo::FileEntry::path`]. They must already be sanitised.
    pub fn new(root: &Path, layout: Layout, relative: &[Vec<String>]) -> Self {
        let paths = relative
            .iter()
            .map(|components| {
                let mut p = root.to_path_buf();
                for c in components {
                    p.push(c);
                }
                long_path(&p)
            })
            .collect::<Vec<_>>();
        let handles = paths.iter().map(|_| Mutex::new(None)).collect();
        Self {
            layout,
            paths,
            handles,
        }
    }

    pub fn path(&self, file: usize) -> &Path {
        &self.paths[file]
    }

    fn handle(&self, file: usize, create: bool) -> io::Result<Option<Arc<File>>> {
        let mut slot = self.handles[file].lock().expect("handle lock poisoned");
        if let Some(h) = slot.as_ref() {
            return Ok(Some(h.clone()));
        }
        let path = &self.paths[file];
        let opened = if create {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
        } else {
            OpenOptions::new().read(true).write(true).open(path)
        };
        match opened {
            Ok(f) => {
                let f = Arc::new(f);
                *slot = Some(f.clone());
                Ok(Some(f))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound && !create => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Close every open file, for example before moving the download.
    pub fn close_all(&self) {
        for h in &self.handles {
            *h.lock().expect("handle lock poisoned") = None;
        }
    }
}

impl Storage for FileStorage {
    fn read(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        for Segment {
            file,
            file_offset,
            buf_offset,
            len,
            pad,
        } in self.layout.segments(offset, buf.len())
        {
            let dst = &mut buf[buf_offset..buf_offset + len];
            if pad {
                dst.fill(0);
                continue;
            }
            match self.handle(file, false)? {
                Some(f) => read_exact_at(&f, dst, file_offset)?,
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("{} does not exist", self.paths[file].display()),
                    ));
                }
            }
        }
        Ok(())
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        for seg in self.layout.segments(offset, data.len()) {
            if seg.pad {
                continue;
            }
            let f = self.handle(seg.file, true)?.expect("created");
            write_all_at(
                &f,
                &data[seg.buf_offset..seg.buf_offset + seg.len],
                seg.file_offset,
            )?;
        }
        Ok(())
    }

    fn file_sizes(&self) -> Vec<Option<u64>> {
        self.paths
            .iter()
            .map(|p| std::fs::metadata(p).ok().map(|m| m.len()))
            .collect()
    }

    fn create_empty_files(&self) -> io::Result<()> {
        for (i, f) in self.layout.files().iter().enumerate() {
            if f.length == 0 && !f.pad && !self.paths[i].exists() {
                self.handle(i, true)?;
            }
        }
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        for h in &self.handles {
            let f = h.lock().expect("handle lock poisoned").clone();
            if let Some(f) = f {
                f.sync_data()?;
            }
        }
        Ok(())
    }
}

/// On Windows, use the verbatim `\\?\` form so paths longer than `MAX_PATH` work.
///
/// A verbatim path skips Windows' own normalisation, so the path is first made absolute
/// and normalised: `/` becomes `\`, and `.` and `..` are resolved. Prefixing a path like
/// `C:/downloads/x` as-is would make `/` part of a file name, and every open would fail.
#[cfg(windows)]
pub(crate) fn long_path(p: &Path) -> PathBuf {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let Ok(abs) = std::path::absolute(p) else {
        return p.to_path_buf();
    };
    let wide: Vec<u16> = abs.as_os_str().encode_wide().collect();
    let bs = u16::from(b'\\');
    let (question, dot) = (u16::from(b'?'), u16::from(b'.'));
    // Already verbatim (`\\?\`) or a device path (`\\.\`).
    if wide.len() >= 4
        && wide[0] == bs
        && wide[1] == bs
        && (wide[2] == question || wide[2] == dot)
        && wide[3] == bs
    {
        return abs;
    }
    let mut out: Vec<u16> = Vec::with_capacity(wide.len() + 8);
    if wide.starts_with(&[bs, bs]) {
        out.extend(r"\\?\UNC\".encode_utf16());
        out.extend_from_slice(&wide[2..]);
    } else {
        out.extend(r"\\?\".encode_utf16());
        out.extend_from_slice(&wide);
    }
    PathBuf::from(OsString::from_wide(&out))
}

#[cfg(not(windows))]
pub(crate) fn long_path(p: &Path) -> PathBuf {
    p.to_path_buf()
}

#[cfg(unix)]
fn read_exact_at(f: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.read_exact_at(buf, offset)
}

#[cfg(unix)]
fn write_all_at(f: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.write_all_at(buf, offset)
}

#[cfg(windows)]
fn read_exact_at(f: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match f.seek_read(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn write_all_at(f: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match f.seek_write(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use apitorrent_metainfo::{FileSource, Torrent, TorrentBuilder, Version};

    #[test]
    fn write_then_read_across_files_and_pads() {
        let bytes = TorrentBuilder::new("t", Version::Hybrid)
            .piece_length(16 * 1024)
            .file(FileSource::from_bytes(&["a"], vec![1; 20_000]))
            .file(FileSource::from_bytes(&["b"], vec![2; 5_000]))
            .build()
            .unwrap();
        let t = Torrent::from_bytes(&bytes).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::from_info(&t.info);
        let rel: Vec<Vec<String>> = t.info.files.iter().map(|f| f.path.clone()).collect();
        let s = FileStorage::new(dir.path(), layout.clone(), &rel);

        // Write the whole piece space, pads included (their bytes are skipped).
        let mut space = vec![0u8; usize::try_from(layout.total_length()).unwrap()];
        for f in &t.info.files {
            let start = usize::try_from(f.offset).unwrap();
            let end = start + usize::try_from(f.length).unwrap();
            let fill = if f.is_pad() {
                0
            } else if f.path.last().unwrap() == "a" {
                1
            } else {
                2
            };
            space[start..end].fill(fill);
        }
        for piece in 0..layout.piece_count() {
            let off = layout.piece_offset(piece);
            let size = layout.piece_size(piece) as usize;
            let start = usize::try_from(off).unwrap();
            s.write(off, &space[start..start + size]).unwrap();
        }
        s.sync().unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("t").join("a")).unwrap(),
            vec![1; 20_000]
        );
        assert_eq!(
            std::fs::read(dir.path().join("t").join("b")).unwrap(),
            vec![2; 5_000]
        );

        let mut back = vec![0xaa; space.len()];
        s.read(0, &mut back).unwrap();
        assert_eq!(back, space);

        let sizes = s.file_sizes();
        assert_eq!(sizes[0], Some(20_000));
        assert_eq!(sizes[1], None, "the pad file is never created");
    }

    /// Found in the first real-world test: a save path written with `/` (as a web UI or
    /// config file sends it) failed every write on Windows.
    #[test]
    fn save_path_with_forward_slashes_and_dot_dot() {
        let bytes = TorrentBuilder::new("t.bin", Version::V1)
            .piece_length(16 * 1024)
            .file(FileSource::from_bytes(&[], vec![7; 1000]))
            .build()
            .unwrap();
        let t = Torrent::from_bytes(&bytes).unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let odd = format!("{}/sub/../sub", dir.path().display()).replace('\\', "/");
        let rel: Vec<Vec<String>> = t.info.files.iter().map(|f| f.path.clone()).collect();
        let s = FileStorage::new(Path::new(&odd), Layout::from_info(&t.info), &rel);
        s.write(0, &[7; 1000]).unwrap();
        s.sync().unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("sub").join("t.bin")).unwrap(),
            vec![7; 1000]
        );
    }

    #[cfg(windows)]
    #[test]
    fn long_path_forms() {
        assert_eq!(
            long_path(Path::new("C:/a/b/../c")),
            Path::new(r"\\?\C:\a\c")
        );
        assert_eq!(
            long_path(Path::new(r"\\server\share\x")),
            Path::new(r"\\?\UNC\server\share\x")
        );
        assert_eq!(long_path(Path::new(r"\\?\C:\a")), Path::new(r"\\?\C:\a"));
    }

    #[test]
    fn reading_missing_files_fails_cleanly() {
        let bytes = TorrentBuilder::new("t", Version::V1)
            .piece_length(16 * 1024)
            .file(FileSource::from_bytes(&[], vec![1; 100]))
            .build()
            .unwrap();
        let t = Torrent::from_bytes(&bytes).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let rel: Vec<Vec<String>> = t.info.files.iter().map(|f| f.path.clone()).collect();
        let s = FileStorage::new(dir.path(), Layout::from_info(&t.info), &rel);
        let mut buf = [0u8; 100];
        assert_eq!(
            s.read(0, &mut buf).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }
}

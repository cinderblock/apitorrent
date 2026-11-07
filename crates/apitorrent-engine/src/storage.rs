//! Where piece data lives.
//!
//! The engine only ever writes a piece after it has been verified, so a file never holds
//! bytes that failed a hash check. That rule is what makes it safe for several torrents to
//! share one file (see the content library in `docs/architecture.md`).
//!
//! Each file has a [`Placement`]. Wanted files live in the save directory. Unwanted files
//! never do: the bytes of theirs that arrive inside boundary pieces go to the torrent's
//! [part store](crate::parts), and a file the user deselects after it has data moves to
//! the torrent's held area, or is deleted.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::layout::{Layout, Segment};
use crate::parts::PartFile;

/// Where one file's bytes live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Placement {
    /// In the save directory: a file the user wants.
    #[default]
    Save,
    /// In the torrent's held area: deselected, but kept, so selecting it again needs no
    /// download.
    Held,
    /// Not a file on disk. Its bytes inside pieces the torrent keeps are in the part store.
    Parts,
}

/// Storage for one torrent's piece space. Implementations must be safe to call from several
/// disk threads at once.
pub trait Storage: Send + Sync + std::fmt::Debug {
    /// Fill `buf` with the bytes at `offset` in piece space. Pad files read as zeros.
    fn read(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;

    /// Write a verified piece.
    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()>;

    /// The current size of each file at its placement, or `None` where it doesn't exist
    /// (always `None` for [`Placement::Parts`]). Used for a quick check of resume data
    /// without hashing.
    fn file_sizes(&self) -> Vec<Option<u64>>;

    /// Each file's placement.
    fn placements(&self) -> Vec<Placement>;

    /// Whether the part store holds a slot for `piece`.
    fn in_parts(&self, piece: u32) -> bool;

    /// Move file `file` to a new placement, carrying its data along.
    ///
    /// Moving to [`Placement::Parts`] deletes the file. Beforehand, its bytes within each
    /// piece in `keep` are copied to the part store; pass the boundary pieces the torrent
    /// has and still needs. Moving out of the part store writes the file's bytes from
    /// there back into a file.
    fn set_placement(&self, file: usize, to: Placement, keep: &[u32]) -> io::Result<()>;

    /// Flush written data to stable storage.
    fn sync(&self) -> io::Result<()>;

    /// Create the torrent's zero-length files. They hold no pieces, so no write ever
    /// creates them.
    fn create_empty_files(&self) -> io::Result<()>;

    /// Which files are links to (or clones of) another torrent's copy, still shared.
    fn linked(&self) -> Vec<bool> {
        Vec::new()
    }

    /// Delete the torrent's held area and part store, and its files in the save directory
    /// too when `save_files` is set.
    fn delete(&self, save_files: bool) -> io::Result<()>;
}

/// Where a torrent's files go.
#[derive(Debug, Clone)]
pub struct StoragePaths {
    /// The save directory; the torrent's file paths are relative to it.
    pub save: PathBuf,
    /// The held area for this torrent. It should be on the same volume as `save`, so that
    /// holding a file is a rename.
    pub held: PathBuf,
    /// The part store file.
    pub parts: PathBuf,
}

#[derive(Debug)]
struct FileSlot {
    /// Held for reading during every I/O on the file, and for writing while it moves.
    placement: RwLock<Placement>,
    handle: Mutex<Option<Arc<File>>>,
    /// The file is a hard link to another torrent's copy (the content library). Writes
    /// that would change it break the link first, so the other torrent's file is never
    /// modified.
    linked: AtomicBool,
    /// Serialises writes to a linked file, so two pieces can't both unshare it and lose
    /// one write to the other's copy.
    unshare: Mutex<()>,
}

/// Plain files under a download directory.
#[derive(Debug)]
pub struct FileStorage {
    layout: Layout,
    save_root: PathBuf,
    held_root: PathBuf,
    save_paths: Vec<PathBuf>,
    held_paths: Vec<PathBuf>,
    files: Vec<FileSlot>,
    parts: PartFile,
    /// Placement changes run one at a time.
    moving: Mutex<()>,
}

impl FileStorage {
    /// `relative` holds each file's path components, as in
    /// [`apitorrent_metainfo::FileEntry::path`]. They must already be sanitised.
    /// `placements` defaults to [`Placement::Save`] for every file.
    pub fn new(
        paths: &StoragePaths,
        layout: Layout,
        relative: &[Vec<String>],
        placements: Option<&[Placement]>,
    ) -> Self {
        let join = |root: &Path, components: &[String]| {
            let mut p = root.to_path_buf();
            for c in components {
                p.push(c);
            }
            long_path(&p)
        };
        let save_paths: Vec<PathBuf> = relative.iter().map(|c| join(&paths.save, c)).collect();
        let held_paths: Vec<PathBuf> = relative.iter().map(|c| join(&paths.held, c)).collect();
        let files = (0..relative.len())
            .map(|i| FileSlot {
                placement: RwLock::new(
                    placements
                        .and_then(|p| p.get(i).copied())
                        .unwrap_or_default(),
                ),
                handle: Mutex::new(None),
                linked: AtomicBool::new(false),
                unshare: Mutex::new(()),
            })
            .collect();
        let parts = PartFile::open(&paths.parts, layout.piece_length(), layout.piece_count());
        Self {
            layout,
            save_root: long_path(&paths.save),
            held_root: long_path(&paths.held),
            save_paths,
            held_paths,
            files,
            parts,
            moving: Mutex::new(()),
        }
    }

    /// Where file `file` is at `placement` (`None` for the part store).
    fn path_at(&self, file: usize, placement: Placement) -> Option<&Path> {
        match placement {
            Placement::Save => Some(&self.save_paths[file]),
            Placement::Held => Some(&self.held_paths[file]),
            Placement::Parts => None,
        }
    }

    fn root_of(&self, placement: Placement) -> &Path {
        match placement {
            Placement::Held => &self.held_root,
            _ => &self.save_root,
        }
    }

    /// Mark files as linked to another torrent's copy (see [`FileSlot::linked`]).
    pub fn set_linked(&self, files: &[usize]) {
        for &f in files {
            if let Some(slot) = self.files.get(f) {
                slot.linked.store(true, Ordering::Release);
            }
        }
    }

    /// Write into a file that may be shared with another torrent. Bytes that are already
    /// there are skipped; if anything would change, the link is broken first by giving
    /// this torrent a private copy of the file.
    fn write_linked(&self, file: usize, path: &Path, src: &[u8], at: u64) -> io::Result<()> {
        let _one_writer = self.files[file]
            .unshare
            .lock()
            .expect("unshare lock poisoned");
        let f = self.handle(file, path, true)?.expect("created");
        if !self.files[file].linked.load(Ordering::Acquire) {
            // Another write unshared it while we waited.
            return write_all_at(&f, src, at);
        }
        let mut existing = vec![0u8; src.len()];
        let same = read_exact_at(&f, &mut existing, at).is_ok() && existing == src;
        if same {
            return Ok(());
        }
        drop(f);
        // Unshare: copy beside it, then rename the copy over the link. The other torrent
        // keeps the original inode.
        self.close(file);
        let tmp = path.with_extension("apitorrent-unshare");
        std::fs::copy(path, &tmp)?;
        OpenOptions::new().write(true).open(&tmp)?.sync_all()?;
        std::fs::rename(&tmp, path)?;
        self.files[file].linked.store(false, Ordering::Release);
        tracing::info!(path = %path.display(), "a shared file differs here; it now has its own copy");
        let f = self.handle(file, path, true)?.expect("created");
        write_all_at(&f, src, at)
    }

    /// The path file `file` has in the save directory.
    pub fn path(&self, file: usize) -> &Path {
        &self.save_paths[file]
    }

    fn handle(&self, file: usize, path: &Path, create: bool) -> io::Result<Option<Arc<File>>> {
        let mut slot = self.files[file]
            .handle
            .lock()
            .expect("handle lock poisoned");
        if let Some(h) = slot.as_ref() {
            return Ok(Some(h.clone()));
        }
        let opened = if create {
            open_or_create(path)
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

    fn close(&self, file: usize) {
        *self.files[file]
            .handle
            .lock()
            .expect("handle lock poisoned") = None;
    }

    /// Close every open file, for example before moving the download.
    pub fn close_all(&self) {
        for i in 0..self.files.len() {
            self.close(i);
        }
    }

    /// Run `f` over the parts of a piece-space segment, split at piece boundaries:
    /// `(piece, offset in piece, range in the segment)`.
    fn per_piece(
        &self,
        piece_space: u64,
        len: usize,
        mut f: impl FnMut(u32, u64, std::ops::Range<usize>) -> io::Result<()>,
    ) -> io::Result<()> {
        let pl = self.layout.piece_length();
        let mut done = 0usize;
        while done < len {
            let at = piece_space + done as u64;
            let piece = u32::try_from(at / pl).expect("piece fits u32");
            let in_piece = at % pl;
            let n = usize::try_from((pl - in_piece).min((len - done) as u64)).expect("fits");
            f(piece, in_piece, done..done + n)?;
            done += n;
        }
        Ok(())
    }

    /// The segments of file `file` within `piece`.
    fn file_in_piece(&self, file: usize, piece: u32) -> Option<Segment> {
        self.layout
            .segments(
                self.layout.piece_offset(piece),
                self.layout.piece_size(piece) as usize,
            )
            .into_iter()
            .find(|s| s.file == file)
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
            let placement = self.files[file]
                .placement
                .read()
                .expect("placement lock poisoned");
            match self.path_at(file, *placement) {
                Some(path) => match self.handle(file, path, false)? {
                    Some(f) => read_exact_at(&f, dst, file_offset)?,
                    None => {
                        return Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("{} does not exist", path.display()),
                        ));
                    }
                },
                None => self.per_piece(offset + buf_offset as u64, len, |piece, at, range| {
                    self.parts.read(piece, at, &mut dst[range])
                })?,
            }
        }
        Ok(())
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        for seg in self.layout.segments(offset, data.len()) {
            if seg.pad {
                continue;
            }
            let src = &data[seg.buf_offset..seg.buf_offset + seg.len];
            let placement = self.files[seg.file]
                .placement
                .read()
                .expect("placement lock poisoned");
            match self.path_at(seg.file, *placement) {
                Some(path) if self.files[seg.file].linked.load(Ordering::Acquire) => {
                    self.write_linked(seg.file, path, src, seg.file_offset)?;
                }
                Some(path) => {
                    let f = self.handle(seg.file, path, true)?.expect("created");
                    write_all_at(&f, src, seg.file_offset)?;
                }
                None => self.per_piece(
                    offset + seg.buf_offset as u64,
                    seg.len,
                    |piece, at, range| self.parts.write(piece, at, &src[range]),
                )?,
            }
        }
        Ok(())
    }

    fn file_sizes(&self) -> Vec<Option<u64>> {
        (0..self.files.len())
            .map(|i| {
                let placement = *self.files[i]
                    .placement
                    .read()
                    .expect("placement lock poisoned");
                self.path_at(i, placement)
                    .and_then(|p| std::fs::metadata(p).ok())
                    .map(|m| m.len())
            })
            .collect()
    }

    fn placements(&self) -> Vec<Placement> {
        self.files
            .iter()
            .map(|f| *f.placement.read().expect("placement lock poisoned"))
            .collect()
    }

    fn in_parts(&self, piece: u32) -> bool {
        self.parts.contains(piece)
    }

    fn set_placement(&self, file: usize, to: Placement, keep: &[u32]) -> io::Result<()> {
        let _one_at_a_time = self.moving.lock().expect("moving lock poisoned");
        let mut placement = self.files[file]
            .placement
            .write()
            .expect("placement lock poisoned");
        let from = *placement;
        if from == to || self.layout.files()[file].pad {
            *placement = to;
            return Ok(());
        }
        self.close(file);
        match (self.path_at(file, from), self.path_at(file, to)) {
            // Save <-> held: a move.
            (Some(src), Some(dst)) => {
                move_file(src, dst)?;
                prune_empty_dirs(src, self.root_of(from));
            }
            // Into the part store: keep the boundary bytes, then delete the file.
            (Some(src), None) => {
                if let Ok(f) = File::open(src) {
                    for &piece in keep {
                        let Some(seg) = self.file_in_piece(file, piece) else {
                            continue;
                        };
                        let mut buf = vec![0u8; seg.len];
                        read_exact_at(&f, &mut buf, seg.file_offset)?;
                        self.parts.write(piece, seg.buf_offset as u64, &buf)?;
                    }
                    self.parts.sync()?;
                    drop(f);
                    std::fs::remove_file(src)?;
                    prune_empty_dirs(src, self.root_of(from));
                }
            }
            // Out of the part store: write what it holds into the file.
            (None, Some(dst)) => {
                let pieces: Vec<(u32, Segment)> = self
                    .parts
                    .pieces()
                    .into_iter()
                    .filter_map(|p| self.file_in_piece(file, p).map(|s| (p, s)))
                    .collect();
                if !pieces.is_empty() {
                    let f = open_or_create(dst)?;
                    for &(piece, seg) in &pieces {
                        let mut buf = vec![0u8; seg.len];
                        self.parts.read(piece, seg.buf_offset as u64, &mut buf)?;
                        write_all_at(&f, &buf, seg.file_offset)?;
                    }
                    f.sync_data()?;
                }
                *placement = to;
                drop(placement);
                // Drop slots no other part-store file still needs.
                for (piece, _) in pieces {
                    let needed = self
                        .layout
                        .segments(
                            self.layout.piece_offset(piece),
                            self.layout.piece_size(piece) as usize,
                        )
                        .iter()
                        .any(|s| {
                            s.file != file
                                && !s.pad
                                && *self.files[s.file]
                                    .placement
                                    .read()
                                    .expect("placement lock poisoned")
                                    == Placement::Parts
                        });
                    if !needed {
                        self.parts.remove(piece)?;
                    }
                }
                return Ok(());
            }
            (None, None) => {}
        }
        *placement = to;
        Ok(())
    }

    fn create_empty_files(&self) -> io::Result<()> {
        for (i, f) in self.layout.files().iter().enumerate() {
            if f.length != 0 || f.pad {
                continue;
            }
            let placement = *self.files[i]
                .placement
                .read()
                .expect("placement lock poisoned");
            if let Some(path) = self.path_at(i, placement)
                && placement == Placement::Save
                && !path.exists()
            {
                self.handle(i, path, true)?;
            }
        }
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        for f in &self.files {
            let h = f.handle.lock().expect("handle lock poisoned").clone();
            if let Some(h) = h {
                h.sync_data()?;
            }
        }
        self.parts.sync()
    }

    fn linked(&self) -> Vec<bool> {
        self.files
            .iter()
            .map(|f| f.linked.load(Ordering::Acquire))
            .collect()
    }

    fn delete(&self, save_files: bool) -> io::Result<()> {
        let _one_at_a_time = self.moving.lock().expect("moving lock poisoned");
        let mut first_error = None;
        for i in 0..self.files.len() {
            let _guard = self.files[i]
                .placement
                .write()
                .expect("placement lock poisoned");
            self.close(i);
            let save = save_files.then_some((&self.save_paths[i], &self.save_root));
            let held = Some((&self.held_paths[i], &self.held_root));
            for (path, root) in [save, held].into_iter().flatten() {
                match std::fs::remove_file(path) {
                    Ok(()) => prune_empty_dirs(path, root),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => {
                        first_error.get_or_insert(e);
                    }
                }
            }
        }
        let _ = std::fs::remove_dir(&self.held_root);
        if let Err(e) = self.parts.delete() {
            first_error.get_or_insert(e);
        }
        first_error.map_or(Ok(()), Err)
    }
}

/// Open a file for reading and writing, creating it (and its directory) if needed. A new
/// file is made sparse on Windows; see [`set_sparse`].
pub(crate) fn open_or_create(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    if f.metadata()?.len() == 0 {
        set_sparse(&f);
    }
    Ok(f)
}

/// Mark a file sparse, so writing far past its end doesn't make NTFS zero-fill the gap
/// first. Pieces arrive in random order, and without this the first write near the end of
/// a large file blocks for as long as it takes to write gigabytes of zeros. Other
/// platforms' filesystems leave holes on their own. Failure (FAT32, exFAT, some network
/// shares) is harmless: writes still work, just slower.
#[cfg(windows)]
#[allow(unsafe_code)]
pub(crate) fn set_sparse(f: &File) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;

    let mut returned = 0u32;
    // SAFETY: the handle is a valid open file for the duration of the call; with no input
    // buffer FSCTL_SET_SPARSE sets the flag, and there is no output buffer or OVERLAPPED.
    let ok = unsafe {
        DeviceIoControl(
            f.as_raw_handle(),
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        tracing::debug!(error = %io::Error::last_os_error(), "could not make a file sparse");
    }
}

#[cfg(not(windows))]
pub(crate) fn set_sparse(_: &File) {}

/// Move a file, falling back to copy and delete across volumes. A missing source is not
/// an error: there is nothing to move.
fn move_file(src: &Path, dst: &Path) -> io::Result<()> {
    if !src.exists() {
        return Ok(());
    }
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir)?;
    }
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(rename_error) => {
            // Cross-volume (EXDEV, ERROR_NOT_SAME_DEVICE) or similar: copy, then delete.
            std::fs::copy(src, dst).map_err(|_| rename_error)?;
            OpenOptions::new().write(true).open(dst)?.sync_all()?;
            std::fs::remove_file(src)
        }
    }
}

/// Remove the now-empty directories between `path` and `root` (both excluded).
fn prune_empty_dirs(path: &Path, root: &Path) {
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d == root || !d.starts_with(root) || std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
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
pub(crate) fn read_exact_at(f: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.read_exact_at(buf, offset)
}

#[cfg(unix)]
pub(crate) fn write_all_at(f: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.write_all_at(buf, offset)
}

#[cfg(windows)]
pub(crate) fn read_exact_at(f: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
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
pub(crate) fn write_all_at(f: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
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

    fn paths(save: &Path) -> StoragePaths {
        StoragePaths {
            save: save.to_path_buf(),
            held: save.join(".held"),
            parts: save.join(".parts"),
        }
    }

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
        let s = FileStorage::new(&paths(dir.path()), layout.clone(), &rel, None);

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
        let s = FileStorage::new(
            &paths(Path::new(&odd)),
            Layout::from_info(&t.info),
            &rel,
            None,
        );
        s.write(0, &[7; 1000]).unwrap();
        s.sync().unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("sub").join("t.bin")).unwrap(),
            vec![7; 1000]
        );
    }

    /// Writing near the end of a new file must not zero-fill everything before it.
    #[cfg(windows)]
    #[test]
    fn new_files_are_sparse() {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big");
        let f = open_or_create(&path).unwrap();
        write_all_at(&f, b"end", 4 << 30).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), (4 << 30) + 3);
        assert_ne!(meta.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE, 0);
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

    /// A file hard-linked from another torrent is never changed through the link: equal
    /// bytes are skipped, and different bytes first give this torrent its own copy.
    #[test]
    fn linked_files_are_copy_on_write() {
        let bytes = TorrentBuilder::new("x.bin", Version::V1)
            .piece_length(16 * 1024)
            .file(FileSource::from_bytes(&[], vec![5; 40_000]))
            .build()
            .unwrap();
        let t = Torrent::from_bytes(&bytes).unwrap();
        let other = tempfile::tempdir().unwrap();
        let mine = tempfile::tempdir().unwrap();
        let original = other.path().join("x.bin");
        std::fs::write(&original, vec![5u8; 40_000]).unwrap();
        std::fs::hard_link(&original, mine.path().join("x.bin")).unwrap();
        let rel: Vec<Vec<String>> = t.info.files.iter().map(|f| f.path.clone()).collect();
        let s = FileStorage::new(&paths(mine.path()), Layout::from_info(&t.info), &rel, None);
        s.set_linked(&[0]);

        s.write(0, &[5; 16 * 1024]).unwrap();
        assert_eq!(s.linked(), [true], "equal bytes keep the link");

        s.write(16 * 1024, &[9; 16 * 1024]).unwrap();
        s.sync().unwrap();
        assert_eq!(s.linked(), [false]);
        let mut expected = vec![5u8; 40_000];
        expected[16 * 1024..32 * 1024].fill(9);
        assert_eq!(std::fs::read(mine.path().join("x.bin")).unwrap(), expected);
        assert_eq!(
            std::fs::read(&original).unwrap(),
            vec![5u8; 40_000],
            "the other torrent's file is untouched"
        );
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
        let s = FileStorage::new(&paths(dir.path()), Layout::from_info(&t.info), &rel, None);
        let mut buf = [0u8; 100];
        assert_eq!(
            s.read(0, &mut buf).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }
}

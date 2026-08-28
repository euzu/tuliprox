use log::warn;
#[cfg(unix)]
pub(crate) use memmap2::Advice;
use memmap2::Mmap;
#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(windows)]
use std::os::windows::fs::FileExt;
use std::{
    ffi::OsString,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
};

/// Windows/memmap2 has no madvise-style Advice API; keep a stub so callers stay portable.
#[cfg(not(unix))]
#[derive(Debug, Clone, Copy)]
pub(crate) enum Advice {
    Normal,
}

#[cfg(unix)]
fn advise_mmap(mmap: &Mmap, advice: Advice, context: &str) {
    if let Err(err) = mmap.advise(advice) {
        warn!("Failed to apply mmap advice {advice:?} for {context}: {err}");
    }
}

#[cfg(not(unix))]
fn advise_mmap(_mmap: &Mmap, _advice: Advice, _context: &str) {}

pub(crate) fn mmap_with_advice(file: &File, advice: Advice, context: &str) -> Option<Mmap> {
    // SAFETY: Every v3 persisted query holds its shared sidecar lock for the mapping lifetime, and v3 writers require
    // the exclusive lock. The v3 temporary verifier maps a private synchronized file that is not mutated while mapped.
    // Legacy v2 callers retain their existing invariant that a mapped file is never truncated in place.
    let mmap = unsafe {
        match Mmap::map(file) {
            Ok(mmap) => mmap,
            Err(err) => {
                warn!("Failed to mmap B+Tree for {context}; falling back to buffered file I/O: {err}");
                return None;
            }
        }
    };
    advise_mmap(&mmap, advice, context);
    Some(mmap)
}

pub(crate) fn read_exact_at_offset(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    file.read_exact_at(buf, offset)?;
    #[cfg(windows)]
    {
        let mut read = 0;
        while read < buf.len() {
            let read_offset = u64::try_from(read)
                .ok()
                .and_then(|read| offset.checked_add(read))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "offset read position overflow"))?;
            let chunk_read = file.seek_read(&mut buf[read..], read_offset)?;
            if chunk_read == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "failed to fill whole buffer"));
            }
            read += chunk_read;
        }
    }
    #[cfg(not(unix))]
    #[cfg(not(windows))]
    {
        let _ = file;
        let _ = buf;
        let _ = offset;
        return Err(io::Error::new(io::ErrorKind::Unsupported, "offset reads are not supported on this platform"));
    }
    Ok(())
}

pub(crate) fn write_all_at_offset(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    file.write_all_at(buf, offset)?;
    #[cfg(windows)]
    {
        let mut written = 0;
        while written < buf.len() {
            let write_offset = u64::try_from(written)
                .ok()
                .and_then(|written| offset.checked_add(written))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "offset write position overflow"))?;
            let chunk_written = file.seek_write(&buf[written..], write_offset)?;
            if chunk_written == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "failed to write whole buffer"));
            }
            written += chunk_written;
        }
    }
    #[cfg(not(unix))]
    #[cfg(not(windows))]
    {
        let _ = file;
        let _ = buf;
        let _ = offset;
        return Err(io::Error::new(io::ErrorKind::Unsupported, "offset writes are not supported on this platform"));
    }
    Ok(())
}

pub fn sidecar_lock_path(filepath: &Path) -> PathBuf {
    if let Some(stem) = filepath.file_stem() {
        let mut name = OsString::from(".");
        name.push(stem);
        name.push(".lock");
        filepath.with_file_name(name)
    } else {
        filepath.with_extension("lock")
    }
}

/// Resolves an existing path, or its existing parent plus the not-yet-created leaf.
///
/// B+Tree staging names are validated before every artifact exists. Canonicalizing the
/// parent still collapses `..` components and symlinked directory aliases without
/// requiring callers to create cleanup-owned files first.
pub(crate) fn resolved_path_identity(path: &Path) -> io::Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(resolved) => Ok(resolved),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let leaf = path.file_name().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("path has no file name for identity resolution: {}", path.display()),
                )
            })?;
            let parent =
                path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
            fs::canonicalize(parent).map(|resolved_parent| resolved_parent.join(leaf)).map_err(|parent_error| {
                io::Error::new(
                    parent_error.kind(),
                    format!("failed to resolve parent directory for path identity {}: {parent_error}", path.display()),
                )
            })
        }
        Err(error) => {
            Err(io::Error::new(error.kind(), format!("failed to resolve path identity {}: {error}", path.display())))
        }
    }
}

pub fn ensure_distinct_sidecar_lock_domains(published: &Path, staging: &Path) -> io::Result<()> {
    let published_lock = sidecar_lock_path(published);
    let staging_lock = sidecar_lock_path(staging);
    let published_identity = resolved_path_identity(&published_lock)?;
    let staging_identity = resolved_path_identity(&staging_lock)?;
    if published_identity == staging_identity {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "published database {} and staging database {} share resolved sidecar lock {}",
                published.display(),
                staging.display(),
                published_identity.display()
            ),
        ));
    }
    Ok(())
}

pub(crate) fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io::Error::new(error.kind(), format!("failed to remove file {}: {error}", path.display()))),
    }
}

/// Extension of the sorted-index sidecar that accompanies a published database.
///
/// The sidecar name is derived from the database name by the storage engine, not
/// by the application: a reader that opens `x.db` has to find `x.idx` beside it.
pub(crate) const FILE_SUFFIX_INDEX: &str = "idx";

/// Path of the sorted-index sidecar belonging to `db_path`.
pub fn get_file_path_for_db_index(db_path: &Path) -> PathBuf {
    db_path.with_extension(FILE_SUFFIX_INDEX)
}

/// Buffer size for the engine's own buffered readers and writers.
pub(crate) const IO_BUFFER_SIZE: usize = 256 * 1024;

/// Only the v2 write path buffers writes, and that path is a fixture builder.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn file_writer<W: io::Write>(w: W) -> io::BufWriter<W> {
    io::BufWriter::with_capacity(IO_BUFFER_SIZE, w)
}

pub(crate) fn file_reader<R: io::Read>(r: R) -> io::BufReader<R> {
    io::BufReader::with_capacity(IO_BUFFER_SIZE, r)
}

/// Move `src` onto `dest`, degrading to a copy when the rename fails.
///
/// This is the semantics the v2 writer has always relied on: a rename that
/// cannot be performed (for example across a filesystem boundary) falls back to
/// a copy, and the source file is left in place for the caller to clean up.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn rename_or_copy(src: &Path, dest: &Path) -> io::Result<()> {
    if fs::rename(src, dest).is_err() {
        fs::copy(src, dest)?;
    }
    Ok(())
}

pub(crate) fn parent_or_dot(path: &Path) -> &Path {
    path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."))
}

pub(crate) fn same_parent_directory(left: &Path, right: &Path) -> bool {
    parent_or_dot(left) == parent_or_dot(right)
}

pub(crate) fn require_same_parent_directory(staging: &Path, published: &Path) -> io::Result<()> {
    if same_parent_directory(staging, published) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "staging path {} and published path {} must share one parent directory",
                staging.display(),
                published.display()
            ),
        ))
    }
}

#[derive(Debug)]
pub enum BPlusTreeError {
    Io(io::Error),
    Corrupted(String),
    InvalidStructure(String),
    KeyNotFound,
}

impl std::fmt::Display for BPlusTreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::Corrupted(msg) => write!(f, "Data corrupted: {msg}"),
            Self::InvalidStructure(msg) => write!(f, "Invalid structure: {msg}"),
            Self::KeyNotFound => write!(f, "Key not found"),
        }
    }
}

impl std::error::Error for BPlusTreeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Corrupted(_) | Self::InvalidStructure(_) | Self::KeyNotFound => None,
        }
    }
}

impl From<io::Error> for BPlusTreeError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl BPlusTreeError {
    pub fn to_io(self) -> io::Error {
        match self {
            Self::Io(error) => error,
            Self::KeyNotFound => io::Error::new(io::ErrorKind::NotFound, "Key not found"),
            error => io::Error::new(io::ErrorKind::InvalidData, error),
        }
    }
}

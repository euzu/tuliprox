use log::warn;
use memmap2::Mmap;
#[cfg(unix)]
pub(crate) use memmap2::Advice;
use std::{
    ffi::OsString,
    fs::File,
    io,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(windows)]
use std::os::windows::fs::FileExt;

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

pub(crate) fn sidecar_lock_path(filepath: &Path) -> PathBuf {
    if let Some(stem) = filepath.file_stem() {
        let mut name = OsString::from(".");
        name.push(stem);
        name.push(".lock");
        filepath.with_file_name(name)
    } else {
        filepath.with_extension("lock")
    }
}

pub(crate) fn ensure_distinct_sidecar_lock_domains(published: &Path, staging: &Path) -> io::Result<()> {
    let published_lock = sidecar_lock_path(published);
    let staging_lock = sidecar_lock_path(staging);
    if published_lock == staging_lock {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "published database {} and staging database {} share sidecar lock {}",
                published.display(),
                staging.display(),
                published_lock.display()
            ),
        ));
    }
    Ok(())
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
    fn from(err: io::Error) -> Self { Self::Io(err) }
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

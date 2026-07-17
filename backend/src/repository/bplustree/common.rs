use log::warn;
use memmap2::{Advice, Mmap};
use std::{
    ffi::OsString,
    fs::File,
    io,
    path::{Path, PathBuf},
};

#[cfg(not(unix))]
use std::io::{Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::FileExt;

fn advise_mmap(mmap: &Mmap, advice: Advice, context: &str) {
    if let Err(err) = mmap.advise(advice) {
        warn!("Failed to apply mmap advice {advice:?} for {context}: {err}");
    }
}

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
    #[cfg(not(unix))]
    {
        let mut file = file;
        let current_pos = file.stream_position()?;
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(buf)?;
        file.seek(SeekFrom::Start(current_pos))?;
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

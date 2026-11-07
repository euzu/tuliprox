//! A stable lease outside the replaceable recovery directory.
//!
//! The page sidecar lock protects individual B+Tree operations. It cannot fence
//! an idle journal handle that may append later, so each live journal also owns
//! a shared lease for its whole lifetime. Offline maintenance needs exclusivity.
use super::RecoveryPaths;
use crate::common::{resolved_path_identity, sidecar_lock_path};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions, TryLockError},
    io,
    path::PathBuf,
};

/// Derives the path to the offline maintenance lease file for the given recovery paths.
pub fn maintenance_lease_path(paths: &RecoveryPaths) -> io::Result<PathBuf> {
    let directory = resolved_path_identity(&paths.directory)?;
    let parent = directory.parent().ok_or_else(|| io::Error::other("recovery directory has no parent"))?;
    let name = directory.file_name().ok_or_else(|| io::Error::other("recovery directory has no name"))?;
    let mut lease = OsString::from(".");
    lease.push(name);
    lease.push(".maintenance.lock");
    Ok(parent.join(lease))
}

/// Opens the lease file beside the recovery directory, refusing a path that aliases a database artifact.
fn open_lease(paths: &RecoveryPaths) -> io::Result<File> {
    if let Some(parent) = paths.directory.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = paths.database.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let path = maintenance_lease_path(paths)?;
    let identity = resolved_path_identity(&path)?;
    if identity == resolved_path_identity(&sidecar_lock_path(&paths.database))?
        || identity == resolved_path_identity(&paths.database)?
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "recovery lease aliases a database artifact"));
    }
    OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)
}

/// Turns lock contention into an operator-facing `WouldBlock` error.
fn lock_error(error: TryLockError) -> io::Error {
    match error {
        TryLockError::WouldBlock => io::Error::new(
            io::ErrorKind::WouldBlock,
            "recovery history is in use or offline maintenance is active; stop all processes using this database before repair",
        ),
        TryLockError::Error(error) => error,
    }
}

/// Pins the recovery location while a journal or schema migration can use it.
/// This never replaces the B+Tree's per-operation sidecar locks.
#[derive(Debug)]
pub struct RecoveryUseGuard {
    _file: File,
}

impl RecoveryUseGuard {
    /// Takes a shared lease; fails with `WouldBlock` while offline maintenance holds the exclusive lease.
    pub fn try_acquire(paths: &RecoveryPaths) -> io::Result<Self> {
        let file = open_lease(paths)?;
        file.try_lock_shared().map_err(lock_error)?;
        Ok(Self { _file: file })
    }
}

/// Exclusive offline access, spanning inspection, archiving and publication.
/// Stored beside the history, not inside the directory that repair replaces.
#[derive(Debug)]
pub struct RecoveryMaintenanceGuard {
    _file: File,
    database: PathBuf,
    directory: PathBuf,
}

impl RecoveryMaintenanceGuard {
    /// Takes the exclusive lease; fails with `WouldBlock` while any journal or other maintenance uses the history.
    pub fn try_acquire(paths: &RecoveryPaths) -> io::Result<Self> {
        let file = open_lease(paths)?;
        file.try_lock().map_err(lock_error)?;
        Ok(Self {
            _file: file,
            database: resolved_path_identity(&paths.database)?,
            directory: resolved_path_identity(&paths.directory)?,
        })
    }

    /// Rejects use of this lease for a database/history pair other than the one it was acquired for.
    pub(super) fn check_paths(&self, paths: &RecoveryPaths) -> io::Result<()> {
        if self.database != resolved_path_identity(&paths.database)?
            || self.directory != resolved_path_identity(&paths.directory)?
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "maintenance lease belongs to another database/history pair",
            ));
        }
        Ok(())
    }
}

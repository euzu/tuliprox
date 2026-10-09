#[cfg(all(not(unix), not(windows)))]
use super::move_target_file_platform;
#[cfg(windows)]
use super::move_target_file_platform;
use super::{CategoryEntry, XtreamRefreshLease};
use shared::error::TuliproxError;
use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
};
#[cfg(not(windows))]
use tuliprox_core::utils::parent_or_dot;
use tuliprox_core::{model::XtreamCategory, utils::require_same_parent_directory};

pub(super) fn publish_staged_file_same_directory(staging: &Path, published: &Path) -> io::Result<()> {
    require_same_parent_directory(staging, published)?;
    let staging_path = tempfile::TempPath::try_from_path(staging).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to prepare staging file {} for publication: {error}", staging.display()),
        )
    })?;
    publish_staged_file_platform(staging_path, published)
}

#[cfg(not(windows))]
fn publish_staged_file_platform(staging_path: tempfile::TempPath, published: &Path) -> io::Result<()> {
    publish_staged_file_with_parent_sync(staging_path, published, sync_published_file_parent)
}

#[cfg(not(windows))]
pub(super) fn publish_staged_file_with_parent_sync(
    staging_path: tempfile::TempPath,
    published: &Path,
    sync_parent: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    staging_path.persist(published).map_err(io::Error::from)?;
    sync_parent(published).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "file {} was published, but its parent directory {} could not be synchronized: {error}",
                published.display(),
                parent_or_dot(published).display()
            ),
        )
    })
}

#[cfg(windows)]
fn publish_staged_file_platform(mut staging_path: tempfile::TempPath, published: &Path) -> io::Result<()> {
    move_target_file_platform(staging_path.as_ref(), published, TargetFileMoveMode::ReplaceDestination)?;

    // MoveFileExW consumed the source path. Prevent TempPath from issuing a
    // redundant delete for a path that no longer exists.
    staging_path.disable_cleanup(true);
    Ok(())
}

#[cfg(unix)]
pub(super) fn sync_published_file_parent(path: &Path) -> io::Result<()> { File::open(parent_or_dot(path))?.sync_all() }

/// Every Windows transaction move uses `MOVEFILE_WRITE_THROUGH`; reaching
/// this barrier therefore means all preceding backup or publication moves
/// have completed durably without a second raw rename.
#[cfg(windows)]
pub(super) fn sync_published_file_parent(_path: &Path) -> io::Result<()> { Ok(()) }

/// There is no supported directory durability barrier for other targets.
/// Callers report this only after the atomic rename has completed.
#[cfg(all(not(unix), not(windows)))]
pub(super) fn sync_published_file_parent(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "parent-directory synchronization is unsupported on this platform"))
}

/// Owns the staging category file plus its `flock`, so the lock is released
/// even when a later step (`serde_json::to_writer`, `sync_all`) returns an
/// error. The unlock runs in `Drop` and is logged on failure; closing the
/// underlying `File` releases the OS-level lock either way.
struct LockedCategoryFile {
    file: File,
    path: PathBuf,
}

impl LockedCategoryFile {
    fn create(path: &Path) -> io::Result<Self> {
        let file = File::create(path)?;
        file.lock()?;
        Ok(Self { file, path: path.to_path_buf() })
    }

    pub(super) fn sync_all(&self) -> io::Result<()> { self.file.sync_all() }
}

impl Drop for LockedCategoryFile {
    fn drop(&mut self) {
        if let Err(error) = self.file.unlock() {
            log::warn!(
                "Failed to unlock staging category file {}: {error}; the OS will release it on close",
                self.path.display()
            );
        }
    }
}

pub(super) async fn save_xtream_categories_to_file(
    refresh_lease: XtreamRefreshLease,
    categories: &[XtreamCategory],
) -> Result<(), TuliproxError> {
    let cat_entries: Vec<CategoryEntry> = categories
        .iter()
        .map(|c| CategoryEntry { category_id: c.category_id, category_name: c.category_name.clone(), parent_id: 0 })
        .collect();

    tokio::task::spawn_blocking(move || {
        let staging_path = &refresh_lease.paths().staging_categories;
        let locked = LockedCategoryFile::create(staging_path).map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Failed to create or lock staging category file {}: {error}",
                staging_path.display()
            ))
        })?;
        serde_json::to_writer(&locked.file, &cat_entries).map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Failed to write staging category file {}: {error}",
                staging_path.display()
            ))
        })?;
        locked.sync_all().map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Failed to synchronize staging category file {}: {error}",
                staging_path.display()
            ))
        })?;
        Ok(())
    })
    .await
    .map_err(|e| TuliproxError::RepositoryXtream(format!("Spawn error {e}")))?
}

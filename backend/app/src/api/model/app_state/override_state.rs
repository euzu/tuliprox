//! Process-wide access to the manual override files and to the applied-revision store.
//!
//! Both have to exist exactly once per process: the repository holds the per-file locks that
//! serialize the read-modify-write cycles of concurrent saves, and the status store caches the
//! applied revisions in memory. They are created lazily by the first request that needs them, so
//! a deployment without override files pays nothing.
//!
//! The directory of the repository derives from the configuration path and the directory of the
//! status document from the storage path; both are fixed for the lifetime of the process, which
//! is what makes a single cached instance correct.

use super::AppState;
use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
};
use tuliprox_core::utils::resolve_override_dir;
use tuliprox_repository::{OverrideRepository, OverrideStatusStore};

/// The override repository and status store of this process.
#[derive(Default)]
pub struct OverrideState {
    repository: OnceLock<Arc<OverrideRepository>>,
    status: OnceLock<Arc<OverrideStatusStore>>,
}

impl OverrideState {
    /// The repository of `override_dir`, created on first use.
    pub fn repository(&self, override_dir: PathBuf) -> Arc<OverrideRepository> {
        Arc::clone(self.repository.get_or_init(|| Arc::new(OverrideRepository::new(override_dir))))
    }

    /// The status store of `storage_dir`, created on first use.
    pub fn status(&self, storage_dir: PathBuf) -> Arc<OverrideStatusStore> {
        Arc::clone(self.status.get_or_init(|| Arc::new(OverrideStatusStore::new(storage_dir))))
    }
}

impl AppState {
    /// The override file repository every request shares, so its per-file locks hold.
    pub fn override_repository(&self) -> Arc<OverrideRepository> {
        let paths = self.app_config.paths.load();
        self.overrides.repository(resolve_override_dir(&paths.config_path))
    }

    /// The store recording which override revisions the last successful runs applied.
    pub fn override_status_store(&self) -> Arc<OverrideStatusStore> {
        self.overrides.status(self.app_config.resolve_storage_dir())
    }
}

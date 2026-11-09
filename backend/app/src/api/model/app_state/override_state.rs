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
    sync::{Arc, OnceLock, RwLock},
};
use tuliprox_core::utils::resolve_override_dir;
use tuliprox_repository::{OverrideRepository, OverrideStatusStore};

/// The override repository and status store of this process.
#[derive(Default)]
pub struct OverrideState {
    repository: OnceLock<Arc<OverrideRepository>>,
    status: RwLock<Option<(PathBuf, Arc<OverrideStatusStore>)>>,
}

impl OverrideState {
    /// The repository of `override_dir`, created on first use.
    pub fn repository(&self, override_dir: PathBuf) -> Arc<OverrideRepository> {
        Arc::clone(self.repository.get_or_init(|| Arc::new(OverrideRepository::new(override_dir))))
    }

    /// The status store of `storage_dir`, created on first use.
    /// Rebuilt when the resolved path changes.
    pub fn status(&self, storage_dir: PathBuf) -> Arc<OverrideStatusStore> {
        if let Ok(guard) = self.status.read() {
            if let Some((ref cached_dir, ref store)) = *guard {
                if *cached_dir == storage_dir {
                    return Arc::clone(store);
                }
            }
        }
        let mut guard = match self.status.write() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some((ref cached_dir, ref store)) = *guard {
            if *cached_dir == storage_dir {
                return Arc::clone(store);
            }
        }
        let store = Arc::new(OverrideStatusStore::new(storage_dir.clone()));
        *guard = Some((storage_dir, Arc::clone(&store)));
        store
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_override_status_rebuilds_on_path_change() {
        let state = OverrideState::default();
        let dir1 = PathBuf::from("/tmp/storage1");
        let dir2 = PathBuf::from("/tmp/storage2");

        let store1 = state.status(dir1.clone());
        let store1_cached = state.status(dir1);
        assert!(Arc::ptr_eq(&store1, &store1_cached));

        let store2 = state.status(dir2);
        assert!(!Arc::ptr_eq(&store1, &store2));
    }
}

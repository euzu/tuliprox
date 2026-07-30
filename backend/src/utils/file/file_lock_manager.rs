use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::{fmt, io};
use std::time::Duration;
use tokio::sync::{Mutex, RwLock};
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard};
use shared::error::str_to_io_error;
use path_clean::PathClean;
use crate::api::model::AppState;

#[derive(Clone, PartialEq, Eq, Hash)]
enum LockKey {
    Path(PathBuf),
    Str(String),
}

#[derive(Clone)]
pub struct FileLockManager {
    locks: Arc<Mutex<HashMap<LockKey, Weak<RwLock<()>>>>>,
    internal_write_revisions: Arc<Mutex<HashMap<PathBuf, blake3::Hash>>>,
}

impl FileLockManager {
    pub fn new() -> Self {
        Self {
            locks: Arc::new(Mutex::new(HashMap::new())),
            internal_write_revisions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Removes all entries from the internal locks map whose `RwLock` has been dropped.
    ///
    /// Each entry in the `HashMap` is stored as a `Weak<RwLock<()>>`. This method iterates
    /// over all keys and removes the ones that cannot be upgraded to a strong `Arc` anymore.
    /// This helps prevent unbounded growth of the locks map for dynamic string keys.
    pub async fn prune_unused_locks(&self) {
        let mut locks = self.locks.lock().await;
        let initial_count = locks.len();
        // Retain only entries that can still be upgraded (i.e., there is at least one active guard)
        locks.retain(|_key, weak_lock| weak_lock.upgrade().is_some());
        let removed = initial_count - locks.len();
        if removed > 0 {
            log::debug!("Pruned {removed} unused file locks ({} remaining)", locks.len());
        }
        drop(locks);

        let tracked_revisions = self
            .internal_write_revisions
            .lock()
            .await
            .iter()
            .map(|(path, revision)| (path.clone(), *revision))
            .collect::<Vec<_>>();
        let mut missing_revisions = Vec::new();
        for (path, revision) in tracked_revisions {
            if !matches!(tokio::fs::try_exists(&path).await, Ok(true)) {
                missing_revisions.push((path, revision));
            }
        }
        if !missing_revisions.is_empty() {
            let mut revisions = self.internal_write_revisions.lock().await;
            for (path, revision) in missing_revisions {
                if revisions.get(&path) == Some(&revision) {
                    revisions.remove(&path);
                }
            }
        }
    }

    pub async fn mark_internal_write_revision(&self, path: &Path) -> io::Result<()> {
        let content = tokio::fs::read(path).await?;
        self.internal_write_revisions.lock().await.insert(normalize_path(path), blake3::hash(&content));
        Ok(())
    }

    pub async fn is_internal_write_revision(&self, path: &Path) -> bool {
        let normalized = normalize_path(path);
        let Ok(content) = tokio::fs::read(path).await else {
            self.internal_write_revisions.lock().await.remove(&normalized);
            return false;
        };
        let revision = blake3::hash(&content);
        let mut revisions = self.internal_write_revisions.lock().await;
        if revisions.get(&normalized) == Some(&revision) {
            true
        } else {
            revisions.remove(&normalized);
            false
        }
    }

    // Acquires a read lock for the specified file and returns a FileReadGuard.
    pub async fn read_lock(&self, path: &Path) -> FileReadGuard {
        let file_lock = self.get_or_create_lock(Self::get_lock_key_for_path(path)).await;
        let guard = Arc::clone(&file_lock).read_owned().await;
        FileReadGuard::new(guard)
    }

    // Acquires a write lock for the specified file and returns a FileWriteGuard.
    pub async fn write_lock(&self, path: &Path) -> FileWriteGuard {
        let file_lock = self.get_or_create_lock(Self::get_lock_key_for_path(path)).await;
        let guard = Arc::clone(&file_lock).write_owned().await;
        FileWriteGuard::new(guard)
    }

    // Tries to acquire a write lock for the specified file and returns a FileWriteGuard.
    pub async fn try_write_lock(&self, path: &Path) -> io::Result<FileWriteGuard> {
        let file_lock = self.get_or_create_lock(Self::get_lock_key_for_path(path)).await;
        match Arc::clone(&file_lock).try_write_owned() {
            Ok(lock_guard) => Ok(FileWriteGuard::new(lock_guard)),
            Err(_) => Err(str_to_io_error("Failed to acquire write lock"))
        }
    }

    /// Acquires a write lock using a raw string key instead of a normalized `Path`.
    ///
    /// Unlike the standard path-based locks, this method does **not** perform any
    /// path normalization or conversion. The string is used directly as the lock key,
    /// which can be useful for non-file-based identifiers or dynamic keys.
    pub async fn write_lock_str(&self, text: &str) -> FileWriteGuard {
        let lock_key = LockKey::Str(text.to_string());
        let file_lock = self.get_or_create_lock(lock_key).await;
        let guard = Arc::clone(&file_lock).write_owned().await;
        FileWriteGuard::new(guard)
    }

    /// Tries to acquire a write lock using a raw string key instead of a normalized `Path`.
    ///
    /// Unlike the standard path-based locks, this method does **not** perform any
    /// path normalization or conversion. The string is used directly as the lock key,
    /// which can be useful for non-file-based identifiers or dynamic keys.
    ///
    /// Returns immediately with an error if the lock is currently held, rather than
    /// waiting for it to become available.
    pub async fn try_write_lock_str(&self, text: &str) -> io::Result<FileWriteGuard> {
        let lock_key = LockKey::Str(text.to_string());
        let file_lock = self.get_or_create_lock(lock_key).await;
        match Arc::clone(&file_lock).try_write_owned() {
            Ok(lock_guard) => Ok(FileWriteGuard::new(lock_guard)),
            Err(_) => Err(str_to_io_error("Failed to acquire write lock"))
        }
    }


    fn get_lock_key_for_path(path: &Path) -> LockKey {
        let normalized_path = normalize_path(path);
        LockKey::Path(normalized_path)
    }

    // Helper function: retrieves or creates a lock for a file.
    async fn get_or_create_lock(&self, lock_key: LockKey) -> Arc<RwLock<()>> {
        let mut locks = self.locks.lock().await;


        if let Some(weak_lock) = locks.get(&lock_key) {
            if let Some(strong_lock) = weak_lock.upgrade() {
                return strong_lock;
            }
            locks.remove(&lock_key);
        }

        let file_lock = Arc::new(RwLock::new(()));
        locks.insert(lock_key, Arc::downgrade(&file_lock));
        file_lock
    }
}

impl Default for FileLockManager {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for FileLockManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileLockManager")
            // .field("locks", &self.locks.lock().await.keys().collect::<Vec<_>>())
            .finish()
    }
}

// Define FileReadGuard to hold both the lock reference and the actual read guard.
pub struct FileReadGuard {
    _guard: OwnedRwLockReadGuard<()>,
}

impl FileReadGuard {
    fn new(guard: OwnedRwLockReadGuard<()>) -> Self {
        Self { _guard: guard }
    }
}

// Define FileWriteGuard to hold both the lock reference and the actual write guard.
pub struct FileWriteGuard {
    _guard: OwnedRwLockWriteGuard<()>,
}

impl FileWriteGuard {
    fn new(guard: OwnedRwLockWriteGuard<()>) -> Self {
        Self { _guard: guard }
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }

    let base = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir().unwrap_or_else(|_| PathBuf::from("./")).join(path)
    };

    base.clean()
}

pub fn exec_file_lock_prune(app_state: &Arc<AppState>) {
    let app_state = Arc::clone(app_state);
    tokio::spawn({
        async move {
            loop {
                tokio::time::sleep(Duration::from_mins(1)).await;
                app_state.app_config.file_locks.prune_unused_locks().await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::FileLockManager;

    #[tokio::test]
    async fn internal_write_revision_matches_only_unchanged_content() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("source.yml");
        let locks = FileLockManager::new();
        tokio::fs::write(&path, b"first").await?;

        locks.mark_internal_write_revision(&path).await?;
        assert!(locks.is_internal_write_revision(&path).await);

        tokio::fs::write(&path, b"second").await?;
        assert!(!locks.is_internal_write_revision(&path).await);
        assert!(!locks.is_internal_write_revision(&path).await);
        Ok(())
    }
}

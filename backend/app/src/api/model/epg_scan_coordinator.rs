//! Shared limits for full scans of the per-target EPG databases: bounded concurrency, cooperative cancellation and a
//! cache of the channel-id sets.

use lru::LruCache;
use shared::error::TuliproxError;
use std::{
    collections::HashSet,
    fs::Metadata,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::SystemTime,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

/// Full EPG scans that may run at the same time; further callers wait without holding a thread or a lock.
///
/// Two, so one slow scan of a large EPG does not starve the scan of another target.
pub(crate) const EPG_SCAN_PERMITS: usize = 2;

/// Channel-id sets kept at most; the least recently used set is dropped when the cache is full.
pub(crate) const EPG_KEY_CACHE_ENTRIES: usize = 8;

/// Identity of an EPG database snapshot: `(database_id, generation)` of the b+tree header.
pub(crate) type EpgSnapshotIdentity = ([u8; 16], u64);

/// Channel ids (storage keys) of an EPG database.
pub(crate) type EpgKeySet = Arc<HashSet<Arc<str>>>;

/// Returns an error when the caller of a blocking scan is gone; scans call this once per entry and stop on it.
pub(crate) fn check_scan_cancelled(cancel: &CancellationToken) -> Result<(), TuliproxError> {
    if cancel.is_cancelled() {
        Err(TuliproxError::RepositoryEpg("EPG scan cancelled".to_string()))
    } else {
        Ok(())
    }
}

/// File facts that change when an EPG database is replaced: length, modification time and, on unix, device and inode.
///
/// A database is published by writing a staged file and renaming it over the old one, so a rewrite always yields a new
/// inode and modification time; comparing the stamp therefore tells a cache lookup that the cached snapshot is still
/// the file on disk without opening it. Without an inode (other platforms) a rewrite within one modification-time tick
/// that keeps the length is theoretically invisible; the header identity checked on the slow path stays authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EpgFileStamp {
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl EpgFileStamp {
    /// `None` when the platform reports no modification time, so the stamp could not tell a rewrite.
    pub(crate) fn from_metadata(metadata: &Metadata) -> Option<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Some(Self {
            len: metadata.len(),
            modified: metadata.modified().ok()?,
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        })
    }

    /// Stamp of the file currently at `path`; `None` when it cannot be inspected.
    pub(crate) fn read(path: &Path) -> Option<Self> { Self::from_metadata(&std::fs::metadata(path).ok()?) }
}

struct CachedKeys {
    identity: EpgSnapshotIdentity,
    /// Stamp of the file the keys were read from; `None` when it could not be determined.
    stamp: Option<EpgFileStamp>,
    keys: EpgKeySet,
}

/// Bounded least-recently-used cache of channel-id sets, keyed by database path and valid for one database snapshot.
pub(crate) struct EpgKeyCache {
    entries: Mutex<LruCache<PathBuf, CachedKeys>>,
}

impl EpgKeyCache {
    fn new(max_entries: usize) -> Self {
        let capacity = NonZeroUsize::new(max_entries).unwrap_or(NonZeroUsize::MIN);
        Self { entries: Mutex::new(LruCache::new(capacity)) }
    }

    fn lock(&self) -> MutexGuard<'_, LruCache<PathBuf, CachedKeys>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The cached set of `path` when it was read from a file that still has the `stamp`; promotes the entry.
    ///
    /// Needs no database access, so callers can answer a hit without a scan slot or the file lock.
    pub(crate) fn get_unchanged(&self, path: &Path, stamp: &EpgFileStamp) -> Option<EpgKeySet> {
        self.lock()
            .get(path)
            .filter(|cached| cached.stamp.as_ref() == Some(stamp))
            .map(|cached| Arc::clone(&cached.keys))
    }

    /// The cached set of `path`, if it was read from the database snapshot `identity`; promotes the entry.
    ///
    /// The same snapshot in a file with another `stamp` (e.g. touched or copied) is still valid: the stamp is refreshed
    /// so the next lookup is answered without opening the database.
    pub(crate) fn get(
        &self,
        path: &Path,
        identity: EpgSnapshotIdentity,
        stamp: Option<EpgFileStamp>,
    ) -> Option<EpgKeySet> {
        let mut entries = self.lock();
        let cached = entries.get_mut(path).filter(|cached| cached.identity == identity)?;
        cached.stamp = stamp;
        Some(Arc::clone(&cached.keys))
    }

    /// Number of channel ids cached for `path`, valid or stale, as a capacity hint for the next scan; does not promote.
    pub(crate) fn cached_len(&self, path: &Path) -> usize {
        self.lock().peek(path).map_or(0, |cached| cached.keys.len())
    }

    /// Caches the set of `path`; the least recently used entry is dropped when the cache is full.
    pub(crate) fn insert(
        &self,
        path: &Path,
        identity: EpgSnapshotIdentity,
        stamp: Option<EpgFileStamp>,
        keys: EpgKeySet,
    ) {
        let mut entries = self.lock();
        let entry = CachedKeys { identity, stamp, keys };
        match entries.get_mut(path) {
            Some(cached) => *cached = entry,
            None => {
                entries.put(path.to_path_buf(), entry);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize { self.lock().len() }

    /// Whether `path` is cached, without promoting it.
    #[cfg(test)]
    pub(crate) fn contains(&self, path: &Path) -> bool { self.lock().peek(path).is_some() }
}

/// Instance-scoped limits for full EPG scans: at most [`EPG_SCAN_PERMITS`] run at once and the channel-id sets are
/// cached while the database is unchanged.
pub(crate) struct EpgScanCoordinator {
    permits: Arc<Semaphore>,
    key_cache: Arc<EpgKeyCache>,
}

impl Default for EpgScanCoordinator {
    fn default() -> Self { Self::with_limits(EPG_SCAN_PERMITS, EPG_KEY_CACHE_ENTRIES) }
}

impl EpgScanCoordinator {
    fn with_limits(permits: usize, cache_entries: usize) -> Self {
        Self { permits: Arc::new(Semaphore::new(permits)), key_cache: Arc::new(EpgKeyCache::new(cache_entries)) }
    }

    /// Waits for a scan slot. Cancel-safe: a dropped waiter holds nothing.
    pub(crate) async fn acquire(&self) -> Result<OwnedSemaphorePermit, TuliproxError> {
        Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|err| TuliproxError::RepositoryEpg(format!("EPG scan limiter closed: {err}")))
    }

    /// Handle for scan closures that run on a blocking thread and therefore cannot borrow the coordinator.
    pub(crate) fn key_cache(&self) -> Arc<EpgKeyCache> { Arc::clone(&self.key_cache) }

    #[cfg(test)]
    pub(crate) fn available_permits(&self) -> usize { self.permits.available_permits() }
}

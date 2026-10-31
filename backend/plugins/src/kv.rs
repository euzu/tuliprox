//! The per-plugin KV store over `tuliprox-btree` (plan §18 Q10).
//!
//! One file per plugin, `<data_dir>/<id>/kv.db`, a `BPlusTree<String, Vec<u8>>`
//! behind one mutex-guarded handle that all of the plugin's instances share. It
//! opens lazily on first use. All tree I/O runs on `spawn_blocking`.
//!
//! The tree is opened in batch mode: `kv-set` / `kv-delete` stage into one open
//! write transaction and become durable at [`KvHandle::commit`], which the host
//! runs at the end of every guest call. An error on the tree drops the handle's
//! open tree (and with it any uncommitted writes); the next operation reopens
//! the file and re-seeds the byte count from what is on disk.

use std::{
    collections::HashMap,
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard, PoisonError,
    },
};
use tuliprox_btree::{BPlusTree, BPlusTreeQuery, BPlusTreeUpdate, FlushPolicy};
use tuliprox_plugin_api::bindings::tuliprox::plugin::host::KvError;

/// Bytes (Σ key + value) one plugin may store.
pub(crate) const KV_QUOTA_BYTES: u64 = 50 * 1024 * 1024;
pub(crate) const KV_MAX_KEY_BYTES: usize = 1024;
pub(crate) const KV_MAX_VALUE_BYTES: usize = 1024 * 1024;
/// The file is never compacted below this size, however much of it is garbage.
const KV_COMPACT_FLOOR_BYTES: u64 = 1024 * 1024;

type Tree = BPlusTreeUpdate<String, Vec<u8>>;

/// Usage of one plugin's store, for stats.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct KvUsage {
    pub(crate) used_bytes: u64,
    pub(crate) quota_bytes: u64,
}

/// What one lock of the store guards.
#[derive(Default)]
struct KvState {
    /// `None` until first use, and again after an I/O error.
    tree: Option<Tree>,
    /// Σ key + value bytes of the live entries, uncommitted writes included.
    used_bytes: u64,
    /// File size right after the last compaction (0 before the first).
    compacted_len: u64,
}

struct KvInner {
    path: PathBuf,
    quota_bytes: u64,
    state: Mutex<KvState>,
    /// Mirror of `KvState::used_bytes` for the synchronous [`KvHandle::usage`].
    used_bytes: AtomicU64,
}

impl KvInner {
    /// Locks the state; a panic while it was held leaves the tree in an unknown
    /// state, so it is dropped and reopened on the next operation.
    fn lock(&self) -> MutexGuard<'_, KvState> {
        self.state.lock().unwrap_or_else(|poisoned| {
            let mut state = poisoned.into_inner();
            state.tree = None;
            state
        })
    }
}

/// One plugin's store: `<data_dir>/<id>/kv.db`, opened on first use, one
/// mutex-guarded handle shared by all of the plugin's instances.
#[derive(Clone)]
pub(crate) struct KvHandle {
    inner: Arc<KvInner>,
}

impl fmt::Debug for KvHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KvHandle").field("path", &self.inner.path).finish_non_exhaustive()
    }
}

impl KvHandle {
    fn new(path: PathBuf, quota_bytes: u64) -> Self {
        Self {
            inner: Arc::new(KvInner {
                path,
                quota_bytes,
                state: Mutex::new(KvState::default()),
                used_bytes: AtomicU64::new(0),
            }),
        }
    }

    pub(crate) async fn get(&self, key: String) -> Result<Option<Vec<u8>>, KvError> {
        check_key(&key)?;
        self.blocking(move |state, inner| state.run(&inner.path, |tree, _used| tree.query(&key).map(Ok).map_err(to_io)))
            .await
    }

    pub(crate) async fn set(&self, key: String, value: Vec<u8>) -> Result<(), KvError> {
        check_key(&key)?;
        if value.len() > KV_MAX_VALUE_BYTES {
            return Err(KvError::ValueTooLarge);
        }
        self.blocking(move |state, inner| {
            let quota = inner.quota_bytes;
            state.run(&inner.path, |tree, used| {
                let old = tree.query(&key).map_err(to_io)?.map_or(0, |old| entry_bytes(&key, &old));
                let new = entry_bytes(&key, &value);
                let next = used.saturating_sub(old).saturating_add(new);
                // Shrinking an entry is always allowed, even over quota.
                if next > quota && next > *used {
                    return Ok(Err(KvError::QuotaExceeded));
                }
                let _ = tree.upsert(&key, &value)?;
                *used = next;
                Ok(Ok(()))
            })
        })
        .await
    }

    pub(crate) async fn delete(&self, key: String) -> Result<(), KvError> {
        check_key(&key)?;
        self.blocking(move |state, inner| {
            state.run(&inner.path, |tree, used| {
                if let Some(old) = tree.query(&key).map_err(to_io)? {
                    let _ = tree.delete(&key)?;
                    *used = used.saturating_sub(entry_bytes(&key, &old));
                }
                Ok(Ok(()))
            })
        })
        .await
    }

    /// Make the writes of the hook call that just ended durable. Compacts the
    /// file when it has grown past twice the live bytes.
    pub(crate) async fn commit(&self) -> Result<(), KvError> {
        self.blocking(|state, inner| state.commit(&inner.path)).await
    }

    /// Live bytes as of the last operation (0 until the file is first used).
    pub(crate) fn usage(&self) -> KvUsage {
        KvUsage { used_bytes: self.inner.used_bytes.load(Ordering::Relaxed), quota_bytes: self.inner.quota_bytes }
    }

    /// Runs `op` on the locked state on the blocking pool.
    async fn blocking<T, F>(&self, op: F) -> Result<T, KvError>
    where
        T: Send + 'static,
        F: FnOnce(&mut KvState, &KvInner) -> Result<T, KvError> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut state = inner.lock();
            let result = op(&mut state, &inner);
            inner.used_bytes.store(state.used_bytes, Ordering::Relaxed);
            result
        })
        .await
        .map_err(|err| KvError::Unavailable(format!("the plugin KV task failed: {err}")))?
    }
}

impl KvState {
    /// Opens the tree if needed, runs `op` and maps a tree error to
    /// `unavailable`, dropping the tree so the next call reopens it. `op`
    /// returns its own business errors (`quota-exceeded`) in the inner result.
    fn run<T>(
        &mut self,
        path: &Path,
        op: impl FnOnce(&mut Tree, &mut u64) -> io::Result<Result<T, KvError>>,
    ) -> Result<T, KvError> {
        if self.tree.is_none() {
            match open(path) {
                Ok((tree, used_bytes)) => {
                    self.tree = Some(tree);
                    self.used_bytes = used_bytes;
                }
                Err(err) => return Err(unavailable(&err)),
            }
        }
        let Some(tree) = self.tree.as_mut() else { return Err(unavailable("the tree is not open")) };
        match op(tree, &mut self.used_bytes) {
            Ok(result) => result,
            Err(err) => {
                self.tree = None;
                Err(unavailable(&err))
            }
        }
    }

    fn commit(&mut self, path: &Path) -> Result<(), KvError> {
        // Nothing open means nothing staged: an error dropped the tree already.
        let Some(tree) = self.tree.as_mut() else { return Ok(()) };
        if let Err(err) = tree.commit() {
            self.tree = None;
            return Err(unavailable(&err));
        }
        self.compact_if_bloated(path);
        Ok(())
    }

    /// The writes are already durable; a failed compaction only costs disk.
    fn compact_if_bloated(&mut self, path: &Path) {
        let Some(tree) = self.tree.as_mut() else { return };
        let Ok(len) = fs::metadata(path).map(|meta| meta.len()) else { return };
        // `compacted_len` doubles as a brake: store pages carry overhead, so a
        // store of tiny entries stays "bloated" by the 2x rule even when fresh.
        let threshold =
            self.used_bytes.saturating_mul(2).max(KV_COMPACT_FLOOR_BYTES).max(self.compacted_len.saturating_mul(2));
        if len <= threshold {
            return;
        }
        match tree.compact() {
            Ok(()) => self.compacted_len = fs::metadata(path).map_or(0, |meta| meta.len()),
            Err(err) => {
                log::warn!("plugin KV compaction of {} failed: {err}", path.display());
                self.tree = None;
            }
        }
    }
}

/// Opens (creating if absent) the store file and seeds the live byte count from
/// its entries.
fn open(path: &Path) -> io::Result<(Tree, u64)> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    if !path.try_exists()? {
        let _ = BPlusTree::<String, Vec<u8>>::new().store(path)?;
    }
    let mut used_bytes = 0_u64;
    {
        let mut query = BPlusTreeQuery::<String, Vec<u8>>::try_new(path)?;
        for entry in query.iter() {
            let (key, value) = entry?;
            used_bytes = used_bytes.saturating_add(entry_bytes(&key, &value));
        }
    }
    let mut tree = Tree::try_new(path)?;
    tree.set_flush_policy(FlushPolicy::Batch);
    Ok((tree, used_bytes))
}

fn entry_bytes(key: &str, value: &[u8]) -> u64 { u64::try_from(key.len() + value.len()).unwrap_or(u64::MAX) }

fn check_key(key: &str) -> Result<(), KvError> {
    if key.len() > KV_MAX_KEY_BYTES {
        Err(KvError::KeyTooLarge)
    } else {
        Ok(())
    }
}

fn to_io(err: impl fmt::Display) -> io::Error { io::Error::other(err.to_string()) }

fn unavailable(err: &(impl fmt::Display + ?Sized)) -> KvError {
    KvError::Unavailable(format!("the plugin KV store failed: {err}"))
}

/// Every plugin's store, by id. Kept across registry reloads: the btree's
/// sidecar lock allows one writer, so a handle is never opened twice.
#[derive(Debug)]
pub(crate) struct KvStores {
    data_dir: PathBuf,
    quota_bytes: u64,
    handles: Mutex<HashMap<Arc<str>, KvHandle>>,
}

impl KvStores {
    pub(crate) fn new(data_dir: PathBuf) -> Self {
        Self { data_dir, quota_bytes: KV_QUOTA_BYTES, handles: Mutex::default() }
    }

    /// The handle of `id`; the file opens lazily on its first operation.
    pub(crate) fn handle(&self, id: &Arc<str>) -> KvHandle {
        let mut handles = self.handles.lock().unwrap_or_else(PoisonError::into_inner);
        handles
            .entry(Arc::clone(id))
            .or_insert_with(|| KvHandle::new(self.data_dir.join(id.as_ref()).join("kv.db"), self.quota_bytes))
            .clone()
    }

    /// Drop the shared handle of `id`, so an uninstall can delete its file and
    /// a reinstall opens a fresh one. Outside holders keep the handle they
    /// have until they let go.
    pub(crate) fn forget(&self, id: &str) { self.handles.lock().unwrap_or_else(PoisonError::into_inner).remove(id); }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stores(dir: &Path, quota_bytes: u64) -> KvStores {
        KvStores { data_dir: dir.to_path_buf(), quota_bytes, handles: Mutex::default() }
    }

    fn id() -> Arc<str> { Arc::from("demo") }

    #[tokio::test]
    async fn set_get_delete_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kv = KvStores::new(dir.path().to_path_buf()).handle(&id());
        assert_eq!(kv.get("a".to_string()).await.expect("get"), None);
        kv.set("a".to_string(), b"one".to_vec()).await.expect("set");
        // Reads see the staged write before it is committed.
        assert_eq!(kv.get("a".to_string()).await.expect("get"), Some(b"one".to_vec()));
        kv.commit().await.expect("commit");
        kv.delete("a".to_string()).await.expect("delete");
        kv.delete("never-set".to_string()).await.expect("delete of a missing key");
        assert_eq!(kv.get("a".to_string()).await.expect("get"), None);
        assert!(dir.path().join("demo").join("kv.db").exists());
    }

    #[tokio::test]
    async fn handles_are_shared_per_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stores = KvStores::new(dir.path().to_path_buf());
        let first = stores.handle(&id());
        let second = stores.handle(&id());
        first.set("k".to_string(), vec![1]).await.expect("set");
        assert_eq!(second.get("k".to_string()).await.expect("get"), Some(vec![1]));
        let other = stores.handle(&Arc::from("other"));
        assert_eq!(other.get("k".to_string()).await.expect("get"), None);
    }

    #[tokio::test]
    async fn committed_writes_survive_a_reopen_and_seed_the_usage() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let kv = stores(dir.path(), 1000).handle(&id());
            kv.set("alpha".to_string(), vec![7; 10]).await.expect("set");
            kv.set("beta".to_string(), vec![8; 20]).await.expect("set");
            kv.set("alpha".to_string(), vec![9; 12]).await.expect("overwrite");
            kv.delete("beta".to_string()).await.expect("delete");
            kv.commit().await.expect("commit");
            assert_eq!(kv.usage().used_bytes, 5 + 12);
        }
        let kv = stores(dir.path(), 1000).handle(&id());
        assert_eq!(kv.usage().used_bytes, 0, "nothing is read before the first operation");
        assert_eq!(kv.get("alpha".to_string()).await.expect("get"), Some(vec![9; 12]));
        assert_eq!(kv.get("beta".to_string()).await.expect("get"), None);
        assert_eq!(kv.usage().used_bytes, 5 + 12, "seeded from the entries on disk");
        assert_eq!(kv.usage().quota_bytes, 1000);
    }

    #[tokio::test]
    async fn uncommitted_writes_are_lost_with_the_handle() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let kv = stores(dir.path(), 1000).handle(&id());
            kv.set("kept".to_string(), vec![1]).await.expect("set");
            kv.commit().await.expect("commit");
            kv.set("lost".to_string(), vec![2]).await.expect("set");
        }
        let kv = stores(dir.path(), 1000).handle(&id());
        assert_eq!(kv.get("kept".to_string()).await.expect("get"), Some(vec![1]));
        assert_eq!(kv.get("lost".to_string()).await.expect("get"), None);
    }

    #[tokio::test]
    async fn quota_counts_key_plus_value_and_accounts_overwrites_by_delta() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kv = stores(dir.path(), 100).handle(&id());
        kv.set("a".to_string(), vec![0; 59]).await.expect("60 of 100");
        // The overwrite grows the entry by 30 (60 -> 90), not by its full 90.
        kv.set("a".to_string(), vec![0; 89]).await.expect("overwrite accounts the delta");
        assert_eq!(kv.usage().used_bytes, 90);
        assert!(matches!(kv.set("b".to_string(), vec![0; 10]).await, Err(KvError::QuotaExceeded)));
        assert_eq!(kv.usage().used_bytes, 90, "a rejected write changes nothing");
        assert_eq!(kv.get("b".to_string()).await.expect("get"), None);
        // Exactly the quota is allowed.
        kv.set("b".to_string(), vec![0; 9]).await.expect("100 of 100");
        // Shrinking is allowed, and frees room.
        kv.set("a".to_string(), vec![0; 9]).await.expect("shrink");
        kv.delete("b".to_string()).await.expect("delete");
        assert_eq!(kv.usage().used_bytes, 10);
        kv.set("c".to_string(), vec![0; 79]).await.expect("room after the delete");
    }

    #[tokio::test]
    async fn oversized_keys_and_values_are_rejected_before_any_io() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kv = KvStores::new(dir.path().to_path_buf()).handle(&id());
        let long_key = "k".repeat(KV_MAX_KEY_BYTES + 1);
        assert!(matches!(kv.set(long_key.clone(), vec![]).await, Err(KvError::KeyTooLarge)));
        assert!(matches!(kv.get(long_key.clone()).await, Err(KvError::KeyTooLarge)));
        assert!(matches!(kv.delete(long_key).await, Err(KvError::KeyTooLarge)));
        assert!(matches!(kv.set("k".to_string(), vec![0; KV_MAX_VALUE_BYTES + 1]).await, Err(KvError::ValueTooLarge)));
        assert!(!dir.path().join("demo").exists(), "the file is not created by rejected calls");
        // The limits themselves are allowed.
        kv.set("k".repeat(KV_MAX_KEY_BYTES), vec![0; KV_MAX_VALUE_BYTES]).await.expect("at the limits");
    }

    #[tokio::test]
    async fn an_unusable_store_path_is_reported_as_unavailable() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A file where the plugin's directory should be.
        fs::write(dir.path().join("demo"), b"not a directory").expect("write");
        let kv = KvStores::new(dir.path().to_path_buf()).handle(&id());
        assert!(matches!(kv.get("a".to_string()).await, Err(KvError::Unavailable(_))));
        assert!(matches!(kv.set("a".to_string(), vec![1]).await, Err(KvError::Unavailable(_))));
        kv.commit().await.expect("nothing open, nothing to commit");
    }
}

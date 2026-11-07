//! The per-plugin KV store over `tuliprox-btree`.
//!
//! One file per plugin, `<data_dir>/<id>/kv.db`, a `BPlusTree<String, Vec<u8>>`
//! behind one mutex-guarded handle that all of the plugin's instances share. It
//! opens lazily on first use. All tree I/O runs on `spawn_blocking`.
//!
//! `kv-set` / `kv-delete` of a guest call are buffered in that call's
//! [`KvWrites`] (one per `Store`); reads see the call's own buffer first. At the
//! end of the call [`KvHandle::apply`] writes the buffer to the tree and commits
//! it under the handle's mutex, so a running call never commits what another
//! call staged. The tree is opened in batch mode and holds an open write
//! transaction only inside `apply`. An error on the tree drops the handle's
//! open tree (and with it the writes of that `apply`); the next operation
//! reopens the file and re-seeds the byte count from what is on disk.

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
    /// Σ key + value bytes of the live entries.
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

    /// The size of `key`'s entry (`None` when absent) and the live bytes of
    /// the whole store, in one lock.
    async fn probe(&self, key: String) -> Result<(Option<u64>, u64), KvError> {
        self.blocking(move |state, inner| {
            state.run(&inner.path, |tree, used| {
                let entry = tree.query(&key).map_err(to_io)?.map(|value| entry_bytes(&key, &value));
                Ok(Ok((entry, *used)))
            })
        })
        .await
    }

    /// Writes `writes` (`None` is a delete) and makes them durable, under one
    /// lock so nothing else is staged meanwhile. The quota is checked again
    /// against what is stored now, since other calls committed since the guest
    /// wrote; a write that would exceed it is skipped. Returns how many were
    /// skipped. Compacts the file when it has grown past twice the live bytes.
    pub(crate) async fn apply(&self, mut writes: Vec<KvWrite>) -> Result<usize, KvError> {
        // Deletes first, so the room they free counts for the sets.
        writes.sort_by_key(|(_, value)| value.is_some());
        self.blocking(move |state, inner| {
            let quota = inner.quota_bytes;
            let skipped = state.run(&inner.path, |tree, used| {
                let mut skipped = 0;
                for (key, value) in &writes {
                    let old = tree.query(key).map_err(to_io)?;
                    let old_bytes = old.as_ref().map_or(0, |old| entry_bytes(key, old));
                    match value {
                        Some(value) => {
                            let next = used.saturating_sub(old_bytes).saturating_add(entry_bytes(key, value));
                            // Shrinking an entry is always allowed, even over quota.
                            if next > quota && next > *used {
                                skipped += 1;
                                continue;
                            }
                            let _ = tree.upsert(key, value)?;
                            *used = next;
                        }
                        None if old.is_some() => {
                            let _ = tree.delete(key)?;
                            *used = used.saturating_sub(old_bytes);
                        }
                        None => {}
                    }
                }
                Ok(Ok(skipped))
            })?;
            state.commit(&inner.path)?;
            Ok(skipped)
        })
        .await
    }

    /// Committed live bytes as of the last operation (0 until the file is first used).
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

/// One buffered write: `None` deletes the key.
pub(crate) type KvWrite = (String, Option<Vec<u8>>);

/// The KV writes of one guest call (one per `Store`), applied by
/// [`KvHandle::apply`] when the call ends. Until then only this call sees them.
#[derive(Debug, Default)]
pub(crate) struct KvWrites {
    pending: HashMap<String, Option<Vec<u8>>>,
    /// What the buffer adds to (negative: frees from) the committed usage.
    delta: i64,
}

impl KvWrites {
    pub(crate) fn is_empty(&self) -> bool { self.pending.is_empty() }

    /// The buffered writes; the buffer is empty afterwards.
    pub(crate) fn take(&mut self) -> Vec<KvWrite> {
        self.delta = 0;
        self.pending.drain().collect()
    }

    /// The call's own write if there is one, else the stored value.
    pub(crate) async fn get(&self, kv: &KvHandle, key: String) -> Result<Option<Vec<u8>>, KvError> {
        check_key(&key)?;
        match self.pending.get(&key) {
            Some(buffered) => Ok(buffered.clone()),
            None => kv.get(key).await,
        }
    }

    /// Buffers a set. The quota is checked against the committed usage plus
    /// this buffer's delta; an overwrite counts by its delta.
    pub(crate) async fn set(&mut self, kv: &KvHandle, key: String, value: Vec<u8>) -> Result<(), KvError> {
        check_key(&key)?;
        if value.len() > KV_MAX_VALUE_BYTES {
            return Err(KvError::ValueTooLarge);
        }
        let (stored, committed) = kv.probe(key.clone()).await?;
        let old = self.visible_bytes(&key, stored);
        let next =
            self.delta.saturating_add(signed(entry_bytes(&key, &value))).saturating_sub(signed(old.unwrap_or(0)));
        // Shrinking is always allowed, even over quota.
        if next > self.delta && i128::from(committed) + i128::from(next) > i128::from(kv.inner.quota_bytes) {
            return Err(KvError::QuotaExceeded);
        }
        self.delta = next;
        self.pending.insert(key, Some(value));
        Ok(())
    }

    /// Buffers a delete; deleting a missing key does nothing.
    pub(crate) async fn delete(&mut self, kv: &KvHandle, key: String) -> Result<(), KvError> {
        check_key(&key)?;
        let (stored, _) = kv.probe(key.clone()).await?;
        if let Some(old) = self.visible_bytes(&key, stored) {
            self.delta = self.delta.saturating_sub(signed(old));
            self.pending.insert(key, None);
        }
        Ok(())
    }

    /// The size of `key`'s entry as this call sees it (`None`: absent); `stored`
    /// is the size in the store.
    fn visible_bytes(&self, key: &str, stored: Option<u64>) -> Option<u64> {
        match self.pending.get(key) {
            Some(buffered) => buffered.as_ref().map(|value| entry_bytes(key, value)),
            None => stored,
        }
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

fn signed(bytes: u64) -> i64 { i64::try_from(bytes).unwrap_or(i64::MAX) }

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

    /// Buffers `key = value` in `writes` and applies the buffer, like one call.
    async fn put(kv: &KvHandle, key: &str, value: Vec<u8>) -> Result<(), KvError> {
        let mut writes = KvWrites::default();
        writes.set(kv, key.to_string(), value).await?;
        kv.apply(writes.take()).await.map(|_| ())
    }

    async fn remove(kv: &KvHandle, key: &str) {
        let mut writes = KvWrites::default();
        writes.delete(kv, key.to_string()).await.expect("delete");
        kv.apply(writes.take()).await.expect("apply");
    }

    #[tokio::test]
    async fn set_get_delete_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kv = KvStores::new(dir.path().to_path_buf()).handle(&id());
        let mut writes = KvWrites::default();
        assert_eq!(writes.get(&kv, "a".to_string()).await.expect("get"), None);
        writes.set(&kv, "a".to_string(), b"one".to_vec()).await.expect("set");
        // The call sees its own write before it is applied; the store does not.
        assert_eq!(writes.get(&kv, "a".to_string()).await.expect("get"), Some(b"one".to_vec()));
        assert_eq!(kv.get("a".to_string()).await.expect("get"), None);
        kv.apply(writes.take()).await.expect("apply");
        assert_eq!(kv.get("a".to_string()).await.expect("get"), Some(b"one".to_vec()));

        let mut writes = KvWrites::default();
        writes.delete(&kv, "a".to_string()).await.expect("delete");
        writes.delete(&kv, "never-set".to_string()).await.expect("delete of a missing key");
        assert_eq!(writes.get(&kv, "a".to_string()).await.expect("get"), None, "own delete hides the stored value");
        assert_eq!(kv.get("a".to_string()).await.expect("get"), Some(b"one".to_vec()), "not applied yet");
        kv.apply(writes.take()).await.expect("apply");
        assert_eq!(kv.get("a".to_string()).await.expect("get"), None);
        assert!(dir.path().join("demo").join("kv.db").exists());
    }

    #[tokio::test]
    async fn handles_are_shared_per_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stores = KvStores::new(dir.path().to_path_buf());
        let first = stores.handle(&id());
        let second = stores.handle(&id());
        put(&first, "k", vec![1]).await.expect("set");
        assert_eq!(second.get("k".to_string()).await.expect("get"), Some(vec![1]));
        let other = stores.handle(&Arc::from("other"));
        assert_eq!(other.get("k".to_string()).await.expect("get"), None);
    }

    #[tokio::test]
    async fn committed_writes_survive_a_reopen_and_seed_the_usage() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let kv = stores(dir.path(), 1000).handle(&id());
            let mut writes = KvWrites::default();
            writes.set(&kv, "alpha".to_string(), vec![7; 10]).await.expect("set");
            writes.set(&kv, "beta".to_string(), vec![8; 20]).await.expect("set");
            writes.set(&kv, "alpha".to_string(), vec![9; 12]).await.expect("overwrite");
            writes.delete(&kv, "beta".to_string()).await.expect("delete");
            kv.apply(writes.take()).await.expect("apply");
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
    async fn unapplied_writes_are_lost_with_the_buffer() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let kv = stores(dir.path(), 1000).handle(&id());
            put(&kv, "kept", vec![1]).await.expect("set");
            let mut writes = KvWrites::default();
            writes.set(&kv, "lost".to_string(), vec![2]).await.expect("set");
        }
        let kv = stores(dir.path(), 1000).handle(&id());
        assert_eq!(kv.get("kept".to_string()).await.expect("get"), Some(vec![1]));
        assert_eq!(kv.get("lost".to_string()).await.expect("get"), None);
    }

    #[tokio::test]
    async fn interleaved_calls_commit_only_their_own_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kv = stores(dir.path(), 1000).handle(&id());
        let (mut first, mut second) = (KvWrites::default(), KvWrites::default());
        first.set(&kv, "first".to_string(), vec![1]).await.expect("set");
        second.set(&kv, "second".to_string(), vec![2]).await.expect("set");
        // The first call ends and commits while the second is still running.
        kv.apply(first.take()).await.expect("apply");
        assert_eq!(kv.get("first".to_string()).await.expect("get"), Some(vec![1]));
        assert_eq!(kv.get("second".to_string()).await.expect("get"), None, "the other call's write is not durable");
        assert_eq!(second.get(&kv, "second".to_string()).await.expect("get"), Some(vec![2]));
        drop(second);
        drop(kv);
        let kv = stores(dir.path(), 1000).handle(&id());
        assert_eq!(kv.get("first".to_string()).await.expect("get"), Some(vec![1]));
        assert_eq!(kv.get("second".to_string()).await.expect("get"), None);
    }

    #[tokio::test]
    async fn a_failed_apply_does_not_drop_another_calls_buffer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kv = stores(dir.path(), 1000).handle(&id());
        put(&kv, "old", vec![0]).await.expect("set");
        let (mut first, mut second) = (KvWrites::default(), KvWrites::default());
        first.set(&kv, "first".to_string(), vec![1]).await.expect("set");
        second.set(&kv, "second".to_string(), vec![2]).await.expect("set");
        // A tree error: the open tree is gone and the file cannot be reopened.
        let path = dir.path().join("demo").join("kv.db");
        let moved = dir.path().join("kv.db.moved");
        kv.inner.lock().tree = None;
        fs::rename(&path, &moved).expect("rename");
        fs::create_dir(&path).expect("mkdir");
        kv.apply(first.take()).await.expect_err("the first call's apply fails");
        assert!(!second.is_empty(), "the other call's buffer is intact");
        fs::remove_dir(&path).expect("rmdir");
        fs::rename(&moved, &path).expect("rename back");
        assert_eq!(second.get(&kv, "second".to_string()).await.expect("get"), Some(vec![2]));
        kv.apply(second.take()).await.expect("the second call applies once the store is back");
        assert_eq!(kv.get("second".to_string()).await.expect("get"), Some(vec![2]));
        assert_eq!(kv.get("first".to_string()).await.expect("get"), None, "the failed apply wrote nothing");
        assert_eq!(kv.get("old".to_string()).await.expect("get"), Some(vec![0]));
    }

    #[tokio::test]
    async fn quota_counts_key_plus_value_and_accounts_overwrites_by_delta() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kv = stores(dir.path(), 100).handle(&id());
        put(&kv, "a", vec![0; 59]).await.expect("60 of 100");
        // The overwrite grows the entry by 30 (60 -> 90), not by its full 90.
        put(&kv, "a", vec![0; 89]).await.expect("overwrite accounts the delta");
        assert_eq!(kv.usage().used_bytes, 90);
        assert!(matches!(put(&kv, "b", vec![0; 10]).await, Err(KvError::QuotaExceeded)));
        assert_eq!(kv.usage().used_bytes, 90, "a rejected write changes nothing");
        assert_eq!(kv.get("b".to_string()).await.expect("get"), None);
        // Exactly the quota is allowed.
        put(&kv, "b", vec![0; 9]).await.expect("100 of 100");
        // Shrinking is allowed, and frees room.
        put(&kv, "a", vec![0; 9]).await.expect("shrink");
        remove(&kv, "b").await;
        assert_eq!(kv.usage().used_bytes, 10);
        put(&kv, "c", vec![0; 79]).await.expect("room after the delete");
    }

    #[tokio::test]
    async fn quota_counts_the_calls_buffered_delta_on_top_of_the_committed_usage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kv = stores(dir.path(), 100).handle(&id());
        put(&kv, "a", vec![0; 49]).await.expect("50 of 100");
        let mut writes = KvWrites::default();
        writes.set(&kv, "b".to_string(), vec![0; 29]).await.expect("80 of 100");
        // 80 buffered + 30 more would be 110.
        assert!(matches!(writes.set(&kv, "c".to_string(), vec![0; 29]).await, Err(KvError::QuotaExceeded)));
        // An overwrite of the buffered key counts by its delta: 30 -> 50, 100 of 100.
        writes.set(&kv, "b".to_string(), vec![0; 49]).await.expect("overwrite within the buffer");
        // A delete frees room again, a stored one as well as a buffered one.
        writes.delete(&kv, "a".to_string()).await.expect("delete of a stored key");
        writes.set(&kv, "c".to_string(), vec![0; 29]).await.expect("room after the delete");
        writes.delete(&kv, "c".to_string()).await.expect("delete of a buffered key");
        assert_eq!(kv.apply(writes.take()).await.expect("apply"), 0);
        assert_eq!(kv.usage().used_bytes, 50, "only `b` is left");
    }

    #[tokio::test]
    async fn apply_skips_a_write_that_no_longer_fits_the_quota() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kv = stores(dir.path(), 100).handle(&id());
        let (mut first, mut second) = (KvWrites::default(), KvWrites::default());
        first.set(&kv, "a".to_string(), vec![0; 69]).await.expect("70 of 100");
        second.set(&kv, "b".to_string(), vec![0; 69]).await.expect("70 of 100 for this call, too");
        assert_eq!(kv.apply(first.take()).await.expect("apply"), 0);
        assert_eq!(kv.apply(second.take()).await.expect("apply"), 1, "the second no longer fits");
        assert_eq!(kv.usage().used_bytes, 70);
        assert_eq!(kv.get("b".to_string()).await.expect("get"), None);
    }

    #[tokio::test]
    async fn oversized_keys_and_values_are_rejected_before_any_io() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kv = KvStores::new(dir.path().to_path_buf()).handle(&id());
        let mut writes = KvWrites::default();
        let long_key = "k".repeat(KV_MAX_KEY_BYTES + 1);
        assert!(matches!(writes.set(&kv, long_key.clone(), vec![]).await, Err(KvError::KeyTooLarge)));
        assert!(matches!(writes.get(&kv, long_key.clone()).await, Err(KvError::KeyTooLarge)));
        assert!(matches!(writes.delete(&kv, long_key).await, Err(KvError::KeyTooLarge)));
        assert!(matches!(
            writes.set(&kv, "k".to_string(), vec![0; KV_MAX_VALUE_BYTES + 1]).await,
            Err(KvError::ValueTooLarge)
        ));
        assert!(!dir.path().join("demo").exists(), "the file is not created by rejected calls");
        // The limits themselves are allowed.
        writes.set(&kv, "k".repeat(KV_MAX_KEY_BYTES), vec![0; KV_MAX_VALUE_BYTES]).await.expect("at the limits");
    }

    #[tokio::test]
    async fn an_unusable_store_path_is_reported_as_unavailable() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A file where the plugin's directory should be.
        fs::write(dir.path().join("demo"), b"not a directory").expect("write");
        let kv = KvStores::new(dir.path().to_path_buf()).handle(&id());
        let mut writes = KvWrites::default();
        assert!(matches!(writes.get(&kv, "a".to_string()).await, Err(KvError::Unavailable(_))));
        assert!(matches!(writes.set(&kv, "a".to_string(), vec![1]).await, Err(KvError::Unavailable(_))));
        assert!(writes.is_empty());
        kv.apply(writes.take()).await.expect_err("the store cannot open");
    }
}

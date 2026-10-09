use super::{invalid_input, BPlusTreeSerialWriter, BPlusTreeUpdate, FlushPolicy};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

impl<K, V> BPlusTreeSerialWriter<K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone + Send + 'static,
    V: Serialize + for<'de> Deserialize<'de> + Send + 'static,
{
    pub fn new(filepath: &Path, flush_policy: FlushPolicy) -> io::Result<Self> {
        let mut updater = BPlusTreeUpdate::try_new_with_backoff(filepath)?;
        updater.set_flush_policy(flush_policy);
        Ok(Self {
            updater: Arc::new(Mutex::new(updater)),
            flush_policy,
            dirty: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(AtomicBool::new(false)),
            background_error: Arc::new(Mutex::new(None)),
            background_handle: Mutex::new(None),
        })
    }

    pub fn upsert_prepared(&self, items: Vec<(K, Vec<u8>)>) -> io::Result<u64> {
        let result = self.updater.lock().upsert_batch_encoded(items);
        if result.is_ok() {
            self.dirty.store(self.flush_policy == FlushPolicy::Batch, Ordering::Release);
        }
        result
    }

    pub fn upsert(&self, items: &[(&K, &V)]) -> io::Result<u64> {
        self.upsert_prepared(BPlusTreeUpdate::<K, V>::prepare_upsert_batch(items)?)
    }

    pub fn start_background_commit(&self, interval: Duration) -> io::Result<()> {
        if self.flush_policy != FlushPolicy::Batch {
            return Err(invalid_input("background commit requires batch flush policy"));
        }
        if interval.is_zero() {
            return Err(invalid_input("background commit interval must be greater than zero"));
        }
        let mut slot = self.background_handle.lock();
        if slot.is_some() {
            return Ok(());
        }
        self.shutdown.store(false, Ordering::Release);
        let updater = Arc::clone(&self.updater);
        let dirty = Arc::clone(&self.dirty);
        let shutdown = Arc::clone(&self.shutdown);
        let background_error = Arc::clone(&self.background_error);
        *slot = Some(
            std::thread::Builder::new()
                .name(String::from("bplustree-commit"))
                .spawn(move || {
                    while !shutdown.load(Ordering::Acquire) {
                        std::thread::park_timeout(interval);
                        if shutdown.load(Ordering::Acquire) {
                            break;
                        }
                        commit_if_dirty(&updater, &dirty, &background_error);
                    }
                    commit_if_dirty(&updater, &dirty, &background_error);
                })
                .map_err(io::Error::other)?,
        );
        Ok(())
    }

    pub fn stop_background_commit(&self) -> io::Result<()> {
        self.shutdown.store(true, Ordering::Release);
        if let Some(handle) = self.background_handle.lock().take() {
            handle.thread().unpark();
            handle.join().map_err(|_| io::Error::other("background B+Tree commit thread panicked"))?;
        }
        if let Some(error) = self.background_error.lock().take() {
            return Err(error);
        }
        Ok(())
    }

    pub fn flush_now(&self) -> io::Result<()> { self.commit() }

    pub fn commit(&self) -> io::Result<()> {
        self.updater.lock().commit()?;
        self.dirty.store(false, Ordering::Release);
        Ok(())
    }

    pub fn shutdown(&self) -> io::Result<()> {
        self.stop_background_commit()?;
        self.commit()
    }
}

fn commit_if_dirty<K, V>(
    updater: &Mutex<BPlusTreeUpdate<K, V>>,
    dirty: &AtomicBool,
    background_error: &Mutex<Option<io::Error>>,
) where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de>,
{
    if !dirty.swap(false, Ordering::AcqRel) {
        return;
    }
    if let Err(error) = updater.lock().commit() {
        dirty.store(true, Ordering::Release);
        log::error!("Background B+Tree commit failed: {error}");
        *background_error.lock() = Some(error);
    }
}

impl<K, V> Drop for BPlusTreeSerialWriter<K, V> {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if self.dirty.load(Ordering::Acquire) {
            log::warn!("Dropping dirty B+Tree serial writer without an explicit shutdown");
        }
        if let Some(handle) = self.background_handle.lock().take() {
            handle.thread().unpark();
            drop(handle);
        }
    }
}

use super::{
    commit_ordered_page_refs_under_existing_lock, encode_value, invalid_data, invalid_input, query_transaction,
    recover_pending_under_existing_lock, stage_delete, stage_upsert, validate_transaction_links, verify_full,
    with_exclusive_sidecar, BPlusTree, BPlusTreeMetadata, BPlusTreeQuery, BPlusTreeUpdate, DatabaseHeader,
    DatabaseImage, ExclusiveSidecarGuard, FlushPolicy, WalOperationError, WalOutcome, WriteScratch, WriteTransaction,
    PAGE_SIZE,
};
use crate::{
    codec::{binary_serialize, binary_serialize_into},
    common::{read_exact_at_offset, BPlusTreeError},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs::File, io, marker::PhantomData, path::Path};

pub(super) struct ActiveBatch {
    pub(super) base: DatabaseImage,
    pub(super) transaction: WriteTransaction,
    pub(super) _guard: ExclusiveSidecarGuard,
}

impl<K, V> BPlusTreeUpdate<K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de>,
{
    pub fn try_new(filepath: &Path) -> io::Result<Self> {
        let mut query = BPlusTreeQuery::<K, V>::try_new(filepath)?;
        let _ = verify_full(&mut query)?;
        let database_id = query.header.database_id;
        let verified_generation = query.header.generation;
        let verified_next_page_id = query.header.next_page_id;
        drop(query);
        Ok(Self {
            filepath: filepath.to_path_buf(),
            database_id,
            verified_generation,
            verified_next_page_id,
            flush_policy: FlushPolicy::Immediate,
            active: None,
            scratch: WriteScratch::default(),
            _types: PhantomData,
        })
    }

    pub fn try_new_with_backoff(filepath: &Path) -> io::Result<Self> { Self::try_new(filepath) }

    pub fn try_new_with_backoff_stats(filepath: &Path) -> io::Result<(Self, u64)> {
        Self::try_new(filepath).map(|updater| (updater, 0))
    }

    pub fn set_flush_policy(&mut self, policy: FlushPolicy) { self.flush_policy = policy; }

    pub(super) fn ensure_transaction(&mut self) -> io::Result<()> {
        if self.active.is_some() {
            return Ok(());
        }
        let guard = ExclusiveSidecarGuard::acquire(&self.filepath)?;
        recover_pending_under_existing_lock(&self.filepath)?;
        let (base, header) = DatabaseImage::open(&self.filepath)?;
        if header.database_id != self.database_id
            || header.generation != self.verified_generation
            || header.next_page_id != self.verified_next_page_id
        {
            let mut query = BPlusTreeQuery::<K, V>::from_file_unlocked(File::open(&self.filepath)?)?;
            let _ = verify_full(&mut query)?;
            self.database_id = query.header.database_id;
            self.verified_generation = query.header.generation;
            self.verified_next_page_id = query.header.next_page_id;
        }
        let transaction = WriteTransaction::new(header, base.as_slice().len())?;
        self.active = Some(ActiveBatch { base, transaction, _guard: guard });
        Ok(())
    }

    pub fn update(&mut self, key: &K, value: V) -> Result<u64, BPlusTreeError> {
        self.upsert(key, &value).map_err(Into::into)
    }

    pub fn update_batch(&mut self, items: &[(&K, &V)]) -> Result<u64, BPlusTreeError> {
        self.upsert_batch(items).map_err(Into::into)
    }

    pub fn prepare_upsert_batch(items: &[(&K, &V)]) -> io::Result<Vec<(K, Vec<u8>)>> {
        let mut prepared = Vec::new();
        prepared.try_reserve_exact(items.len()).map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
        for (key, value) in items {
            prepared.push(((*key).clone(), binary_serialize(value)?));
        }
        prepared.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(prepared)
    }

    pub fn upsert_batch_prepared_with_backoff(filepath: &Path, items: &[(&K, &V)]) -> io::Result<u64> {
        let prepared = Self::prepare_upsert_batch(items)?;
        let mut updater = Self::try_new_with_backoff(filepath)?;
        updater.upsert_batch_encoded(prepared)
    }

    pub fn upsert_batch(&mut self, items: &[(&K, &V)]) -> io::Result<u64> {
        let prepared = match Self::prepare_upsert_batch(items) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.active = None;
                return Err(error);
            }
        };
        self.upsert_batch_encoded(prepared)
    }

    pub fn upsert_batch_encoded(&mut self, items: Vec<(K, Vec<u8>)>) -> io::Result<u64> {
        if items.is_empty() {
            return self.upsert_batch(&[]);
        }
        let policy = self.flush_policy;
        self.flush_policy = FlushPolicy::Batch;
        let mut root = 0;
        for (key, value) in items {
            match self.upsert_serialized(&key, &value) {
                Ok(next_root) => root = next_root,
                Err(error) => {
                    self.active = None;
                    self.flush_policy = policy;
                    return Err(error);
                }
            }
        }
        self.flush_policy = policy;
        if policy == FlushPolicy::Immediate {
            self.commit()?;
        }
        Ok(root)
    }

    pub fn upsert(&mut self, key: &K, value: &V) -> io::Result<u64> {
        let result = self.upsert_inner(key, value);
        if result.is_err() {
            self.active = None;
        }
        result
    }

    fn upsert_inner(&mut self, key: &K, value: &V) -> io::Result<u64> {
        self.scratch.key.clear();
        binary_serialize_into(&mut self.scratch.key, key)?;
        self.scratch.value.clear();
        binary_serialize_into(&mut self.scratch.value, value)?;
        self.ensure_transaction()?;
        let stored = match encode_value(&self.scratch.value, &mut self.scratch.compression) {
            Ok(stored) => stored,
            Err(error) => {
                self.active = None;
                return Err(error);
            }
        };
        let active = self.active.as_mut().ok_or_else(|| invalid_data("write transaction is missing"))?;
        let staged = stage_upsert::<K>(
            &mut active.transaction,
            active.base.as_slice(),
            key,
            &self.scratch.key,
            u32::try_from(self.scratch.value.len()).map_err(|_| invalid_input("serialized value exceeds u32"))?,
            stored.compression(),
            stored.as_slice(),
            &mut self.scratch.cell,
        );
        if let Err(error) = staged {
            self.active = None;
            return Err(error);
        }
        let root = self
            .active
            .as_ref()
            .ok_or_else(|| invalid_data("write transaction is missing"))?
            .transaction
            .next_header
            .root_page_id;
        if self.flush_policy == FlushPolicy::Immediate {
            self.commit()?;
        }
        Ok(root)
    }

    fn upsert_serialized(&mut self, key: &K, raw_value: &[u8]) -> io::Result<u64> {
        let result = self.upsert_serialized_inner(key, raw_value);
        if result.is_err() {
            self.active = None;
        }
        result
    }

    fn upsert_serialized_inner(&mut self, key: &K, raw_value: &[u8]) -> io::Result<u64> {
        self.scratch.key.clear();
        binary_serialize_into(&mut self.scratch.key, key)?;
        self.ensure_transaction()?;
        let stored = match encode_value(raw_value, &mut self.scratch.compression) {
            Ok(stored) => stored,
            Err(error) => {
                self.active = None;
                return Err(error);
            }
        };
        let active = self.active.as_mut().ok_or_else(|| invalid_data("write transaction is missing"))?;
        let staged = stage_upsert::<K>(
            &mut active.transaction,
            active.base.as_slice(),
            key,
            &self.scratch.key,
            u32::try_from(raw_value.len()).map_err(|_| invalid_input("serialized value exceeds u32"))?,
            stored.compression(),
            stored.as_slice(),
            &mut self.scratch.cell,
        );
        if let Err(error) = staged {
            self.active = None;
            return Err(error);
        }
        let root = self
            .active
            .as_ref()
            .ok_or_else(|| invalid_data("write transaction is missing"))?
            .transaction
            .next_header
            .root_page_id;
        if self.flush_policy == FlushPolicy::Immediate {
            self.commit()?;
        }
        Ok(root)
    }

    pub fn delete(&mut self, key: &K) -> io::Result<bool> {
        let result = self.delete_inner(key);
        if result.is_err() {
            self.active = None;
        }
        result
    }

    fn delete_inner(&mut self, key: &K) -> io::Result<bool> {
        let started = self.active.is_none();
        self.scratch.key.clear();
        binary_serialize_into(&mut self.scratch.key, key)?;
        self.ensure_transaction()?;
        let active = self.active.as_mut().ok_or_else(|| invalid_data("write transaction is missing"))?;
        let deleted = stage_delete::<K>(
            &mut active.transaction,
            active.base.as_slice(),
            key,
            &self.scratch.key,
            &mut self.scratch.cell,
        );
        let deleted = match deleted {
            Ok(deleted) => deleted,
            Err(error) => {
                self.active = None;
                return Err(error);
            }
        };
        if !deleted && started {
            self.active = None;
        } else if deleted && self.flush_policy == FlushPolicy::Immediate {
            self.commit()?;
        }
        Ok(deleted)
    }

    pub fn delete_batch(&mut self, keys: &[&K]) -> io::Result<usize> {
        if keys.is_empty() {
            return Ok(0);
        }
        let policy = self.flush_policy;
        self.flush_policy = FlushPolicy::Batch;
        let mut deleted = 0usize;
        let mut ordered = Vec::new();
        ordered.try_reserve_exact(keys.len()).map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
        ordered.extend_from_slice(keys);
        ordered.sort();
        for key in ordered {
            match self.delete(key) {
                Ok(true) => deleted = deleted.checked_add(1).ok_or_else(|| invalid_input("delete count overflow"))?,
                Ok(false) => {}
                Err(error) => {
                    self.active = None;
                    self.flush_policy = policy;
                    return Err(error);
                }
            }
        }
        self.flush_policy = policy;
        if policy == FlushPolicy::Immediate {
            self.commit()?;
        }
        Ok(deleted)
    }

    pub fn get_metadata(&self) -> io::Result<BPlusTreeMetadata> {
        if let Some(active) = &self.active {
            return Ok(active.transaction.next_header.metadata.clone());
        }
        BPlusTreeQuery::<K, V>::try_new(&self.filepath).map(|query| query.header.metadata)
    }

    pub fn set_metadata(&mut self, metadata: &BPlusTreeMetadata) -> io::Result<()> {
        let started = self.active.is_none();
        self.ensure_transaction()?;
        let active = self.active.as_mut().ok_or_else(|| invalid_data("write transaction is missing"))?;
        if active.transaction.next_header.metadata == *metadata {
            if started {
                self.active = None;
            }
            return Ok(());
        }
        active.transaction.next_header.metadata = metadata.clone();
        if self.flush_policy == FlushPolicy::Immediate {
            self.commit()?;
        }
        Ok(())
    }

    pub fn query(&mut self, key: &K) -> Result<Option<V>, BPlusTreeError> {
        if let Some(active) = &mut self.active {
            return query_transaction::<K, V>(
                &active.transaction,
                active.base.as_slice(),
                key,
                &mut self.scratch.read_value,
            )
            .map_err(Into::into);
        }
        BPlusTreeQuery::<K, V>::try_new(&self.filepath).and_then(|mut query| query.query_io(key)).map_err(Into::into)
    }

    pub fn commit(&mut self) -> io::Result<()> {
        let Some(mut active) = self.active.take() else { return Ok(()) };
        validate_transaction_links::<K>(&active.transaction, active.base.as_slice())?;
        let prepared = active.transaction.prepared_pages()?;
        if prepared.is_empty() {
            return Ok(());
        }
        let committed_header = DatabaseHeader::decode(prepared[0].1)?;
        let committed_generation = committed_header.generation;
        let committed_next_page_id = committed_header.next_page_id;
        let result = commit_ordered_page_refs_under_existing_lock(&self.filepath, &prepared);
        match result {
            Ok(()) => {
                self.verified_generation = committed_generation;
                self.verified_next_page_id = committed_next_page_id;
                Ok(())
            }
            Err(error) => {
                let outcome = error
                    .get_ref()
                    .and_then(|source| source.downcast_ref::<WalOperationError>())
                    .map(WalOperationError::outcome);
                if outcome == Some(WalOutcome::CommittedCleanupPending) {
                    self.verified_generation = committed_generation;
                    self.verified_next_page_id = committed_next_page_id;
                }
                if let Err(recovery_error) = recover_pending_under_existing_lock(&self.filepath) {
                    log::error!(
                        "B+Tree commit failed and recovery remains pending for {}: {recovery_error}",
                        self.filepath.display()
                    );
                }
                Err(error)
            }
        }
    }

    pub fn compact(&mut self) -> io::Result<()> {
        self.commit()?;
        let filepath = self.filepath.clone();
        let header = with_exclusive_sidecar(&filepath, || {
            recover_pending_under_existing_lock(&filepath)?;
            let mut query = BPlusTreeQuery::<K, V>::from_file_unlocked(File::open(&filepath)?)?;
            let _ = verify_full(&mut query)?;
            let metadata = query.header.metadata.clone();
            let mut entries = BTreeMap::new();
            for entry in query.iter() {
                let (key, value) = entry?;
                let _ = entries.insert(key, value);
            }
            drop(query);

            let mut replacement = BPlusTree { entries, metadata, dirty: true };
            let _ = replacement.store_exclusive(&filepath)?;
            let file = File::open(&filepath)?;
            let mut page = [0; PAGE_SIZE];
            read_exact_at_offset(&file, &mut page, 0)?;
            DatabaseHeader::decode(&page)
        })?;
        self.database_id = header.database_id;
        self.verified_generation = header.generation;
        self.verified_next_page_id = header.next_page_id;
        Ok(())
    }
}

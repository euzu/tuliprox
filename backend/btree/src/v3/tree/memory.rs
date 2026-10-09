use super::{
    build_pages, invalid_data, invalid_input, publish_database, publish_database_and_invalidate_sorted_index,
    recover_pending_under_existing_lock, sync_parent_directory, temporary_path, verify_full, with_exclusive_sidecar,
    BPlusTree, BPlusTreeMetadata, BPlusTreeQuery, DatabaseHeader, PageSink, StoredDatabase, VerificationReport,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io,
    path::Path,
};

impl<K: Ord, V> Default for BPlusTree<K, V> {
    fn default() -> Self { Self::new() }
}

impl<K: Ord, V> BPlusTree<K, V> {
    pub const fn new() -> Self { Self { entries: BTreeMap::new(), metadata: BPlusTreeMetadata::Empty, dirty: true } }

    pub fn get_metadata(&self) -> &BPlusTreeMetadata { &self.metadata }

    pub fn set_metadata(&mut self, metadata: BPlusTreeMetadata) {
        self.metadata = metadata;
        self.dirty = true;
    }

    pub fn is_empty(&self) -> bool { self.entries.is_empty() }

    pub fn len(&self) -> usize { self.entries.len() }

    pub fn insert(&mut self, key: K, value: V) {
        let _ = self.entries.insert(key, value);
        self.dirty = true;
    }

    pub fn query(&self, key: &K) -> Option<&V> { self.entries.get(key) }

    pub fn find_le(&self, key: &K) -> Option<(&K, &V)> { self.entries.range(..=key).next_back() }

    pub fn iter(&self) -> std::collections::btree_map::Iter<'_, K, V> { self.entries.iter() }
}

impl<'a, K: Ord, V> IntoIterator for &'a BPlusTree<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = std::collections::btree_map::Iter<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter { self.entries.iter() }
}

impl<K, V> BPlusTree<K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de>,
{
    pub fn store(&mut self, filepath: &Path) -> io::Result<u64> {
        with_exclusive_sidecar(filepath, || {
            recover_pending_under_existing_lock(filepath)?;
            if self.dirty {
                self.store_exclusive(filepath).map(|stored| stored.root_page_id)
            } else {
                Ok(0)
            }
        })
    }

    pub(crate) fn store_verified(&mut self, filepath: &Path) -> io::Result<VerificationReport> {
        with_exclusive_sidecar(filepath, || {
            recover_pending_under_existing_lock(filepath)?;
            if !self.dirty {
                return Err(invalid_input("verified store requires a dirty tree"));
            }
            self.store_exclusive(filepath).map(|stored| stored.verification)
        })
    }

    pub(super) fn store_exclusive(&mut self, filepath: &Path) -> io::Result<StoredDatabase> {
        self.store_exclusive_with_directory_sync(filepath, sync_parent_directory)
    }

    pub(super) fn store_exclusive_with_directory_sync(
        &mut self,
        filepath: &Path,
        sync_published_directory: impl FnOnce(&Path) -> io::Result<()>,
    ) -> io::Result<StoredDatabase> {
        let temp_path = temporary_path(filepath)?;
        // The builder streams into the temp file, so it exists from here on — every error
        // path below has to remove it again.
        let prepared = (|| {
            let mut sink = PageSink::new(OpenOptions::new().write(true).create_new(true).open(&temp_path)?);
            let root_page_id = build_pages(&self.entries, &mut sink)?;
            let header = DatabaseHeader {
                root_page_id,
                next_page_id: sink.next_page_id,
                free_page_head: 0,
                generation: 1,
                database_id: *uuid::Uuid::new_v4().as_bytes(),
                metadata: self.metadata.clone(),
            }
            .encode()?;
            sink.write(0, &header)?;
            sink.file.sync_all()?;
            drop(sink);
            let mut query = BPlusTreeQuery::<K, V>::from_file_unlocked(File::open(&temp_path)?)?;
            let verification = verify_full(&mut query)?;
            drop(query);
            Ok((root_page_id, verification))
        })();
        let (root_page_id, verification) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = std::fs::remove_file(&temp_path);
                return Err(error);
            }
        };
        publish_database_and_invalidate_sorted_index(&temp_path, filepath, sync_published_directory)?;
        self.dirty = false;
        Ok(StoredDatabase { root_page_id, verification })
    }

    pub fn store_with_index<SortKey, F>(&mut self, filepath: &Path, sort_key_extractor: F) -> io::Result<u64>
    where
        SortKey: Ord + Serialize,
        F: Fn(&V) -> SortKey,
    {
        self.store_with_index_result(filepath, sort_key_extractor)
            .map(|stored| stored.map_or(0, |stored| stored.root_page_id))
    }

    pub(crate) fn store_with_index_verified<SortKey, F>(
        &mut self,
        filepath: &Path,
        sort_key_extractor: F,
    ) -> io::Result<VerificationReport>
    where
        SortKey: Ord + Serialize,
        F: Fn(&V) -> SortKey,
    {
        self.store_with_index_result(filepath, sort_key_extractor)?
            .map(|stored| stored.verification)
            .ok_or_else(|| invalid_input("verified indexed store requires a dirty tree"))
    }

    fn store_with_index_result<SortKey, F>(
        &mut self,
        filepath: &Path,
        sort_key_extractor: F,
    ) -> io::Result<Option<StoredDatabase>>
    where
        SortKey: Ord + Serialize,
        F: Fn(&V) -> SortKey,
    {
        with_exclusive_sidecar(filepath, || {
            recover_pending_under_existing_lock(filepath)?;
            if !self.dirty {
                return Ok(None);
            }
            let stored = self.store_exclusive(filepath)?;
            Self::store_index_exclusive(filepath, sort_key_extractor, stored.verification.live_entries)?;
            Ok(Some(stored))
        })
    }

    fn store_index_exclusive<SortKey, F>(
        filepath: &Path,
        sort_key_extractor: F,
        expected_entries: u64,
    ) -> io::Result<()>
    where
        SortKey: Ord + Serialize,
        F: Fn(&V) -> SortKey,
    {
        let mut query = BPlusTreeQuery::<K, V>::from_file_unlocked(File::open(filepath)?)?;
        let (database_id, generation) = query.snapshot_identity();
        let mut entries = query
            .collect_with_locators()?
            .into_iter()
            .map(|(key, value, locator)| (sort_key_extractor(&value), key, locator))
            .collect::<Vec<_>>();
        if u64::try_from(entries.len()).map_err(|_| invalid_data("sorted-index entry count exceeds u64"))?
            != expected_entries
        {
            return Err(invalid_data("sorted-index source entry count differs from verified database"));
        }
        entries.sort_unstable_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
        drop(query);

        let index_path = crate::common::get_file_path_for_db_index(filepath);
        let temporary = temporary_path(&index_path)?;
        let prepared = (|| {
            let mut writer = crate::sorted_index::v4::Writer::new(&temporary, database_id, generation)?;
            for (sort_key, primary_key, locator) in &entries {
                writer.push(sort_key, primary_key, *locator)?;
            }
            let _ = writer.finish()?;
            Ok(())
        })();
        if let Err(error) = prepared {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
        publish_database(&temporary, &index_path, sync_parent_directory).map_err(io::Error::from)
    }
}

impl<K, V> BPlusTree<K, V>
where
    K: Ord + for<'de> Deserialize<'de>,
    V: for<'de> Deserialize<'de>,
{
    pub fn load(filepath: &Path) -> io::Result<Self> {
        let mut query = BPlusTreeQuery::<K, V>::try_new(filepath)?;
        let metadata = query.header.metadata.clone();
        let mut entries = BTreeMap::new();
        for entry in query.iter() {
            let (key, value) = entry?;
            let _ = entries.insert(key, value);
        }
        Ok(Self { entries, metadata, dirty: false })
    }
}

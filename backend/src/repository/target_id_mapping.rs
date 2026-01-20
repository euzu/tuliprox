use crate::repository::bplustree::{BPlusTree, BPlusTreeMetadata, BPlusTreeUpdate};
use chrono::Local;
use log::error;
use serde::{Deserialize, Serialize};
use shared::error::TuliproxError;
use shared::info_err;
use shared::model::PlaylistItemType;
use shared::model::UUIDType;
use std::cmp::max;
use std::collections::HashMap;
use std::io::Error;
use std::path::{Path, PathBuf};

// TODO make configurable
const EXPIRATION_DURATION: i64 = 86400;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct VirtualIdRecord {
    pub virtual_id: u32,
    pub provider_id: u32,
    pub uuid: UUIDType,
    pub item_type: PlaylistItemType,
    pub parent_virtual_id: u32, // only for series to hold series info id.
    pub last_updated: i64,
}

impl VirtualIdRecord {
    pub(crate) fn new(provider_id: u32, virtual_id: u32, item_type: PlaylistItemType, parent_virtual_id: u32, uuid: UUIDType) -> Self {
        let last_updated = Local::now().timestamp();
        Self { virtual_id, provider_id, uuid, item_type, parent_virtual_id, last_updated }
    }

    pub fn is_expired(&self) -> bool {
        (Local::now().timestamp() - self.last_updated) > EXPIRATION_DURATION
    }

    pub fn copy_update_timestamp(&self) -> Self {
        Self::new(self.provider_id, self.virtual_id, self.item_type, self.parent_virtual_id, self.uuid)
    }
}


/// Helper to get UUID index path from primary path
fn get_uuid_index_path(path: &Path) -> PathBuf {
    path.with_extension("uuid.db")
}

/// Ensure B+tree file exists, creating empty if needed
fn ensure_tree_file<K, V>(path: &Path) -> std::io::Result<()>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    if !path.exists() {
        BPlusTree::<K, V>::new().store(path)?;
    }
    Ok(())
}

pub struct TargetIdMapping {
    virtual_id_counter: u32,
    // Disk-based handles
    disk_by_virtual_id: BPlusTreeUpdate<u32, VirtualIdRecord>,
    disk_by_uuid: BPlusTreeUpdate<UUIDType, u32>,
    // In-memory working sets (Always populated)
    mem_by_uuid: HashMap<UUIDType, u32>,
    mem_by_virtual_id: HashMap<u32, VirtualIdRecord>,
    // Batch buffers for efficient disk writes
    pending_virtual_id_upserts: HashMap<u32, VirtualIdRecord>,
    pending_uuid_upserts: HashMap<UUIDType, u32>,
    path: PathBuf,
}


impl TargetIdMapping {

    pub fn new(path: &Path, _use_memory_cache: bool) -> Result<Self, TuliproxError> {
        let uuid_index_path = get_uuid_index_path(path);

        // Ensure both tree files exist
        ensure_tree_file::<u32, VirtualIdRecord>(path)
            .map_err(|e| info_err!("Failed to create primary tree at {}: {e}", path.display()))?;
        ensure_tree_file::<UUIDType, u32>(&uuid_index_path)
            .map_err(|e| info_err!("Failed to create UUID index at {}: {e}", uuid_index_path.display()))?;

        // Open disk-based update handles
        let disk_by_virtual_id = match BPlusTreeUpdate::<u32, VirtualIdRecord>::try_new(path) {
            Ok(tree) => tree,
            Err(e) => {
                error!("Failed to open primary tree at {}: {e}", path.display());
                // Create fresh and try again
                let _ = BPlusTree::<u32, VirtualIdRecord>::new().store(path);
                BPlusTreeUpdate::try_new(path).map_err(|_| info_err!("Failed to create primary tree after retry"))?
            }
        };

        let disk_by_uuid = match BPlusTreeUpdate::<UUIDType, u32>::try_new(&uuid_index_path) {
            Ok(tree) => tree,
            Err(e) => {
                error!("Failed to open UUID index at {}: {e}", uuid_index_path.display());
                // Create fresh and try again
                let _ = BPlusTree::<UUIDType, u32>::new().store(&uuid_index_path);
                BPlusTreeUpdate::try_new(&uuid_index_path).map_err(|_| info_err!("Failed to create UUID index after retry"))?
            }
        };

        let mut virtual_id_counter: u32 = 0;

        // Load primary tree into memory
        let tree: BPlusTree<u32, VirtualIdRecord> = BPlusTree::load(path)
            .map_err(|e| {
                error!("Failed to load primary tree at {}, starting fresh: {e}", path.display());
                e
            })
            .unwrap_or_else(|_| BPlusTree::new());

        // Traverse the primary tree to populate in-memory maps
        let mut mem_by_uuid = HashMap::new();
        let mut mem_by_virtual_id = HashMap::new();

        tree.traverse(|keys, values| {
            if let Some(max_key) = keys.iter().max() {
                virtual_id_counter = max(virtual_id_counter, *max_key);
            }
            for v in values {
                mem_by_uuid.insert(v.uuid, v.virtual_id);
                mem_by_virtual_id.insert(v.virtual_id, v.clone());
            }
        });

        Ok(Self {
            virtual_id_counter,
            disk_by_virtual_id,
            disk_by_uuid,
            mem_by_uuid,
            mem_by_virtual_id,
            pending_virtual_id_upserts: HashMap::new(),
            pending_uuid_upserts: HashMap::new(),
            path: path.to_path_buf(),
        })
    }

    pub fn get_and_update_virtual_id(&mut self, uuid: &UUIDType, provider_id: u32, item_type: PlaylistItemType, parent_virtual_id: u32) -> u32 {

        // Lookup existing virtual_id in memory
        let existing_virtual_id = self.mem_by_uuid.get(uuid).copied();

        match existing_virtual_id {
            None => {
                // New entry: allocate new virtual_id
                self.virtual_id_counter += 1;
                let virtual_id = self.virtual_id_counter;
                let record = VirtualIdRecord::new(provider_id, virtual_id, item_type, parent_virtual_id, *uuid);

                // Buffer for disk write
                self.pending_virtual_id_upserts.insert(virtual_id, record.clone());
                self.pending_uuid_upserts.insert(*uuid, virtual_id);

                // Update memory maps
                self.mem_by_uuid.insert(*uuid, virtual_id);
                self.mem_by_virtual_id.insert(virtual_id, record);

                virtual_id
            }
            Some(virtual_id) => {
                // Existing entry: check if update needed
                // Check against in-memory record
                let needs_update = match self.mem_by_virtual_id.get(&virtual_id) {
                    Some(record) => {
                        record.provider_id == provider_id &&
                            (record.item_type != item_type || record.parent_virtual_id != parent_virtual_id)
                    }
                    None => false, // Should not happen if maps are consistent
                };

                if needs_update {
                    let new_record = VirtualIdRecord::new(provider_id, virtual_id, item_type, parent_virtual_id, *uuid);
                    self.pending_virtual_id_upserts.insert(virtual_id, new_record.clone());
                    // Update in-memory map
                    self.mem_by_virtual_id.insert(virtual_id, new_record);
                }

                virtual_id
            }
        }
    }

    pub fn persist(&mut self) -> Result<(), Error> {
        if self.has_pending_changes() {
            // Flush pending virtual_id upserts
            if !self.pending_virtual_id_upserts.is_empty() {
                let mut batch: Vec<(&u32, &VirtualIdRecord)> = self.pending_virtual_id_upserts
                    .iter()
                    .collect();
                batch.sort_by_key(|(k, _)| **k);
                self.disk_by_virtual_id.upsert_batch(&batch)?;
                self.pending_virtual_id_upserts.clear();
            }

            // Flush pending UUID index upserts
            if !self.pending_uuid_upserts.is_empty() {
                let mut batch: Vec<(&UUIDType, &u32)> = self.pending_uuid_upserts
                    .iter()
                    .collect();
                batch.sort_by_key(|(k, _)| *k);
                self.disk_by_uuid.upsert_batch(&batch)?;
                self.pending_uuid_upserts.clear();
            }

            // Persist virtual_id_counter via B+Tree header metadata
            self.disk_by_virtual_id
                .set_metadata(&BPlusTreeMetadata::TargetIdMapping(self.virtual_id_counter))
                .map_err(|e| {
                    error!("Failed to write virtual_id_counter to tree header at {}: {e}", self.path.display());
                    e
                })?;
        }
        Ok(())
    }

    /// Check if there are pending changes
    pub fn has_pending_changes(&self) -> bool {
        !self.pending_virtual_id_upserts.is_empty() || !self.pending_uuid_upserts.is_empty()
    }
}

impl Drop for TargetIdMapping {
    fn drop(&mut self) {
        if self.has_pending_changes() {
            if let Err(err) = self.persist() {
                error!("Failed to persist target id mapping {} err:{err}", &self.path.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::PlaylistItemType;
    use tempfile::tempdir;

    #[test]
    fn test_disk_only_mode() -> Result<(), TuliproxError> {
        let dir = tempdir().map_err(|_| info_err!("Failed to create temp dir"))?;
        let path = dir.path().join("id_mapping.db");

        // Create mapping in disk-only mode
        let uuid1 = UUIDType::default();
        {
            let mut mapping = TargetIdMapping::new(&path)?;
            let vid1 = mapping.get_and_update_virtual_id(&uuid1, 100, PlaylistItemType::Live, 0);
            assert_eq!(vid1, 1);
            mapping.persist().map_err(|_| info_err!("Failed to persist mapping"))?;
        }

        // Reopen and verify persistence
        {
            let mut mapping = TargetIdMapping::new(&path)?;
            let vid1_again = mapping.get_and_update_virtual_id(&uuid1, 100, PlaylistItemType::Live, 0);
            assert_eq!(vid1_again, 1); // Should get same virtual_id
        }

        Ok(())
    }

    #[test]
    fn test_memory_cache_mode() -> Result<(), TuliproxError> {
        let dir = tempdir().map_err(|_| info_err!("Failed to create temp dir"))?;
        let path = dir.path().join("id_mapping_mem.db");

        let uuid1 = UUIDType::default();
        {
            let mut mapping = TargetIdMapping::new(&path)?;
            let vid1 = mapping.get_and_update_virtual_id(&uuid1, 100, PlaylistItemType::Video, 0);
            assert_eq!(vid1, 1);
            mapping.persist().map_err(|err| info_err!("{err}"))?;
        }

        // Reopen with memory cache and verify
        {
            let mut mapping = TargetIdMapping::new(&path)?;
            let vid1_again = mapping.get_and_update_virtual_id(&uuid1, 100, PlaylistItemType::Video, 0);
            assert_eq!(vid1_again, 1);
        }

        Ok(())
    }
}

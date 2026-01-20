use crate::repository::bplustree::{BPlusTree, BPlusTreeMetadata, BPlusTreeUpdate};
use chrono::Local;
use log::error;
use serde::{Deserialize, Serialize};
use shared::error::TuliproxError;
use shared::info_err;
use shared::model::PlaylistItemType;
use shared::model::UUIDType;
use std::cmp::max;
use std::collections::BTreeMap;
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


/// Dual-mode `TargetIdMapping` supporting both memory-cached and disk-only operations.
///
/// - `use_memory_cache=true`: Loads UUID index into RAM for O(1) lookups, writes to both memory and disk
/// - `use_memory_cache=false`: Uses disk-based B+tree queries (O(log n)), writes to disk only
pub struct TargetIdMapping {
    virtual_id_counter: u32,
    use_memory_cache: bool,
    // Disk-based handles
    disk_by_virtual_id: BPlusTreeUpdate<u32, VirtualIdRecord>,
    disk_by_uuid: BPlusTreeUpdate<UUIDType, u32>,
    // In-memory cache (only populated when use_memory_cache=true)
    mem_by_uuid: Option<BTreeMap<UUIDType, u32>>,
    // Batch buffers for efficient disk writes
    pending_virtual_id_upserts: Vec<(u32, VirtualIdRecord)>,
    pending_uuid_upserts: Vec<(UUIDType, u32)>,
    path: PathBuf,
}


impl TargetIdMapping {
    /// Create a new `TargetIdMapping` with dual-mode support.
    ///
    /// - `use_memory_cache=true`: Full memory mode with O(1) lookups
    /// - `use_memory_cache=false`: Disk-only mode with O(log n) lookups
    pub fn new(path: &Path, use_memory_cache: bool) -> Result<Self, TuliproxError> {
        let uuid_index_path = get_uuid_index_path(path);

        let uuid_index_existed = uuid_index_path.exists();

        // Ensure both tree files exist
        ensure_tree_file::<u32, VirtualIdRecord>(path)
            .map_err(|e| info_err!("Failed to create primary tree at {}: {e}", path.display()))?;
        ensure_tree_file::<UUIDType, u32>(&uuid_index_path)
            .map_err(|e| info_err!("Failed to create UUID index at {}: {e}", uuid_index_path.display()))?;

        // Open disk-based update handles
        let mut disk_by_virtual_id = match BPlusTreeUpdate::<u32, VirtualIdRecord>::try_new(path) {
            Ok(tree) => tree,
            Err(e) => {
                error!("Failed to open primary tree at {}: {e}", path.display());
                // Create fresh and try again
                let _ = BPlusTree::<u32, VirtualIdRecord>::new().store(path);
                BPlusTreeUpdate::try_new(path).map_err(|_| info_err!("Failed to create primary tree after retry"))?
            }
        };

        let mut disk_by_uuid = match BPlusTreeUpdate::<UUIDType, u32>::try_new(&uuid_index_path) {
            Ok(tree) => tree,
            Err(e) => {
                error!("Failed to open UUID index at {}: {e}", uuid_index_path.display());
                // Create fresh and try again
                let _ = BPlusTree::<UUIDType, u32>::new().store(&uuid_index_path);
                BPlusTreeUpdate::try_new(&uuid_index_path).map_err(|_| info_err!("Failed to create UUID index after retry"))?
            }
        };

        let mut virtual_id_counter: u32 = 0;
        let mut meta_found = false;

        // Try load from header metadata (using disk handle to avoid full tree load)
        if let Ok(BPlusTreeMetadata::TargetIdMapping(val)) = disk_by_virtual_id.get_metadata() {
            virtual_id_counter = val;
            meta_found = true;
        }

        let mut uuid_index_entries: Vec<(UUIDType, u32)> = Vec::new();
        let needs_uuid_rebuild = !uuid_index_existed;

        // Determines if we MUST traverse the tree
        // We traverse if:
        // 1. We need to populate memory cache (use_memory_cache)
        // 2. We need to rebuild UUID index (needs_uuid_rebuild)
        // 3. We didn't find metadata (so we need to find max id manually)
        let needs_traversal = use_memory_cache || needs_uuid_rebuild || !meta_found;

        let mem_by_uuid = if needs_traversal {
            // Load primary tree only if traversal is needed
            let tree: BPlusTree<u32, VirtualIdRecord> = BPlusTree::load(path)
                .map_err(|e| {
                    error!("Failed to load primary tree at {}, starting fresh: {e}", path.display());
                    e
                })
                .unwrap_or_else(|_| BPlusTree::new());

            // Traverse the primary tree to get UUID mappings and max ID
            let mut uuid_map = BTreeMap::new();
            tree.traverse(|keys, values| {
                if let Some(max_key) = keys.iter().max() {
                    virtual_id_counter = max(virtual_id_counter, *max_key);
                }
                for v in values {
                    uuid_map.insert(v.uuid, v.virtual_id);
                    if needs_uuid_rebuild {
                        uuid_index_entries.push((v.uuid, v.virtual_id));
                    }
                }
            });

            if use_memory_cache {
                Some(uuid_map)
            } else {
                None
            }
        } else {
            // Fast path! No traversal needed
            None
        };

        // Rebuild UUID index if it was missing
        if needs_uuid_rebuild && !uuid_index_entries.is_empty() {
            let batch: Vec<(&UUIDType, &u32)> = uuid_index_entries
                .iter()
                .map(|(k, v)| (k, v))
                .collect();
            if let Err(e) = disk_by_uuid.upsert_batch(&batch) {
                error!("Failed to rebuild UUID index: {e}");
            }
        }

        Ok(Self {
            virtual_id_counter,
            use_memory_cache,
            disk_by_virtual_id,
            disk_by_uuid,
            mem_by_uuid,
            pending_virtual_id_upserts: Vec::new(),
            pending_uuid_upserts: Vec::new(),
            path: path.to_path_buf(),
        })
    }

    pub fn get_and_update_virtual_id(&mut self, uuid: &UUIDType, provider_id: u32, item_type: PlaylistItemType, parent_virtual_id: u32) -> u32 {

        // Lookup existing virtual_id
        let existing_virtual_id = if self.use_memory_cache {
            self.mem_by_uuid.as_ref().and_then(|m| m.get(uuid).copied())
        } else {
            self.disk_by_uuid.query(uuid).ok().flatten()
        };

        match existing_virtual_id {
            None => {
                // New entry: allocate new virtual_id
                self.virtual_id_counter += 1;
                let virtual_id = self.virtual_id_counter;
                let record = VirtualIdRecord::new(provider_id, virtual_id, item_type, parent_virtual_id, *uuid);

                // Buffer for disk write
                self.pending_virtual_id_upserts.push((virtual_id, record));
                self.pending_uuid_upserts.push((*uuid, virtual_id));

                // Update memory cache if enabled
                if let Some(ref mut mem_map) = self.mem_by_uuid {
                    mem_map.insert(*uuid, virtual_id);
                }

                virtual_id
            }
            Some(virtual_id) => {
                // Existing entry: check if update needed
                // For update checks, we use the primary tree
                let needs_update = match self.disk_by_virtual_id.query(&virtual_id) {
                    Ok(Some(record)) => {
                        record.provider_id == provider_id &&
                            (record.item_type != item_type || record.parent_virtual_id != parent_virtual_id)
                    }
                    Ok(None) => false,
                    Err(e) => {
                        error!("Failed to query record for virtual_id {virtual_id}: {e}");
                        false
                    }
                };

                if needs_update {
                    let new_record = VirtualIdRecord::new(provider_id, virtual_id, item_type, parent_virtual_id, *uuid);
                    self.pending_virtual_id_upserts.push((virtual_id, new_record));
                }

                virtual_id
            }
        }
    }

    pub fn persist(&mut self) -> Result<(), Error> {
        if self.has_pending_changes() {
            // Flush pending virtual_id upserts
            if !self.pending_virtual_id_upserts.is_empty() {
                let batch: Vec<(&u32, &VirtualIdRecord)> = self.pending_virtual_id_upserts
                    .iter()
                    .map(|(k, v)| (k, v))
                    .collect();
                self.disk_by_virtual_id.upsert_batch(&batch)?;
                self.pending_virtual_id_upserts.clear();
            }

            // Flush pending UUID index upserts
            if !self.pending_uuid_upserts.is_empty() {
                let batch: Vec<(&UUIDType, &u32)> = self.pending_uuid_upserts
                    .iter()
                    .map(|(k, v)| (k, v))
                    .collect();
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
            let mut mapping = TargetIdMapping::new(&path, false)?;
            let vid1 = mapping.get_and_update_virtual_id(&uuid1, 100, PlaylistItemType::Live, 0);
            assert_eq!(vid1, 1);
            mapping.persist().map_err(|_| info_err!("Failed to persist mapping"))?;
        }

        // Reopen and verify persistence
        {
            let mut mapping = TargetIdMapping::new(&path, false)?;
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
            let mut mapping = TargetIdMapping::new(&path, true)?;
            let vid1 = mapping.get_and_update_virtual_id(&uuid1, 100, PlaylistItemType::Video, 0);
            assert_eq!(vid1, 1);
            mapping.persist().map_err(|err| info_err!("{err}"))?;
        }

        // Reopen with memory cache and verify
        {
            let mut mapping = TargetIdMapping::new(&path, true)?;
            let vid1_again = mapping.get_and_update_virtual_id(&uuid1, 100, PlaylistItemType::Video, 0);
            assert_eq!(vid1_again, 1);
        }

        Ok(())
    }
}

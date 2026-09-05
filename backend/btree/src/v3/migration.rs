use super::{BPlusTree, BPlusTreeMetadata};
use crate::v2;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    ops::Bound,
    path::{Path, PathBuf},
};

const LEGACY_V1: u32 = 1;
const LEGACY_V2: u32 = 2;
const METADATA_LEN_OFFSET: u64 = 16;
const METADATA_MAX_SIZE: u32 = 4000;
const HEADER_FLAG_HAS_METADATA_FLAGS: u32 = 1 << 31;
const HEADER_FLAG_HAS_TOMBSTONES: u32 = 1 << 30;
const HEADER_METADATA_LEN_MASK: u32 = !(HEADER_FLAG_HAS_METADATA_FLAGS | HEADER_FLAG_HAS_TOMBSTONES);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MigrationValidation {
    pub entries: usize,
    pub database_id: [u8; 16],
    pub generation: u64,
    pub corrupted_entries: usize,
}

pub fn storage_version(path: &Path) -> io::Result<Option<u32>> {
    let mut file = File::open(path)?;
    if file.metadata()?.len() < 8 {
        return Ok(None);
    }
    let mut header = [0; 8];
    file.read_exact(&mut header)?;
    if &header[0..4] != b"BTRE" {
        return Ok(None);
    }
    Ok(Some(u32::from_le_bytes(
        header[4..8].try_into().map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
    )))
}

fn migration_source(path: &Path, version: u32) -> io::Result<Option<PathBuf>> {
    if version == LEGACY_V2 {
        return Ok(None);
    }
    if version != LEGACY_V1 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "unsupported legacy B+Tree version"));
    }
    let name = path.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "database has no name"))?;
    let temporary = path.with_file_name(format!("{}.{}.v3.tmp", name.to_string_lossy(), uuid::Uuid::new_v4()));
    std::fs::copy(path, &temporary)?;
    let normalized = normalize_v1_copy(&temporary);
    if let Err(error) = normalized {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(Some(temporary))
}

fn normalize_v1_copy(path: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    file.seek(SeekFrom::Start(METADATA_LEN_OFFSET))?;
    let mut encoded = [0; 4];
    file.read_exact(&mut encoded)?;
    let raw = u32::from_le_bytes(encoded);
    let metadata_len = raw & HEADER_METADATA_LEN_MASK;
    if metadata_len > METADATA_MAX_SIZE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "legacy metadata exceeds header capacity"));
    }
    let normalized = (metadata_len | HEADER_FLAG_HAS_METADATA_FLAGS) & !HEADER_FLAG_HAS_TOMBSTONES;
    file.seek(SeekFrom::Start(METADATA_LEN_OFFSET))?;
    file.write_all(&normalized.to_le_bytes())?;
    file.seek(SeekFrom::Start(4))?;
    file.write_all(&LEGACY_V2.to_le_bytes())?;
    file.sync_all()
}

fn legacy_metadata(path: &Path) -> io::Result<BPlusTreeMetadata> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(METADATA_LEN_OFFSET))?;
    let mut encoded_len = [0; 4];
    file.read_exact(&mut encoded_len)?;
    let metadata_len = u32::from_le_bytes(encoded_len) & HEADER_METADATA_LEN_MASK;
    if metadata_len > METADATA_MAX_SIZE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "legacy metadata exceeds header capacity"));
    }
    let metadata_len =
        usize::try_from(metadata_len).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut encoded = vec![0; metadata_len];
    file.read_exact(&mut encoded)?;
    match encoded.as_slice() {
        [] => Ok(BPlusTreeMetadata::Empty),
        [1, value0, value1, value2, value3] => {
            Ok(BPlusTreeMetadata::TargetIdMapping(u32::from_le_bytes([*value0, *value1, *value2, *value3])))
        }
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "unsupported legacy B+Tree metadata")),
    }
}

pub fn migrate_v2_typed<K, V>(source: &Path) -> io::Result<MigrationValidation>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    migrate_v2_typed_inner::<K, V, V, _, _, _>(
        source,
        std::convert::identity,
        BPlusTree::store_verified,
        |_, _| Ok(()),
        false,
    )
}

pub fn scan_v2_entries<K, V>(path: &Path) -> io::Result<(usize, usize)>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    let version =
        storage_version(path)?.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not a B+Tree database"))?;
    let normalized_source = if version == LEGACY_V1 {
        let temporary = tempfile::NamedTempFile::new()?;
        std::fs::copy(path, temporary.path())?;
        normalize_v1_copy(temporary.path())?;
        Some(temporary)
    } else if version == LEGACY_V2 {
        None
    } else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "unsupported legacy B+Tree version"));
    };
    let read_path = normalized_source.as_ref().map_or(path, tempfile::NamedTempFile::path);
    let mut legacy = v2::BPlusTreeQuery::<K, V>::try_new(read_path)?;
    let mut live = 0usize;
    let mut corrupt = 0usize;
    for entry in legacy.range_iter(Bound::Unbounded, Bound::Unbounded) {
        match entry {
            Ok(_) => live += 1,
            Err(crate::common::BPlusTreeError::UnreadableValue(_)) => corrupt += 1,
            Err(error) => return Err(error.to_io()),
        }
    }
    Ok((live, corrupt))
}

pub fn migrate_v2_typed_with_index<K, V, SortKey, F>(source: &Path, sort_key: F) -> io::Result<MigrationValidation>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
    SortKey: Ord + Serialize + for<'de> Deserialize<'de>,
    F: Fn(&V) -> SortKey,
{
    migrate_v2_typed_inner::<K, V, V, _, _, _>(
        source,
        std::convert::identity,
        |tree, destination| tree.store_with_index_verified(destination, sort_key),
        |destination, entries| {
            let query = super::BPlusTreeQuery::<K, V>::try_new(destination)?;
            let index = crate::common::get_file_path_for_db_index(destination);
            let mut iterator = crate::sorted_index::v4::OwnedIterator::<K, V, SortKey>::open(query, &index)?;
            let indexed_entries = iterator.try_fold(0usize, |count, entry| {
                let _ = entry?;
                count
                    .checked_add(1)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "index validation count overflow"))
            })?;
            if indexed_entries != entries {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "migrated sorted-index entry count mismatch"));
            }
            Ok(())
        },
        true,
    )
}

pub fn migrate_v2_typed_map<K, SourceV, DestinationV, Map>(source: &Path, map: Map) -> io::Result<MigrationValidation>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    SourceV: Serialize + for<'de> Deserialize<'de> + Clone,
    DestinationV: Serialize + for<'de> Deserialize<'de> + Clone,
    Map: FnMut(SourceV) -> DestinationV,
{
    migrate_v2_typed_inner::<K, SourceV, DestinationV, _, _, _>(
        source,
        map,
        BPlusTree::store_verified,
        |_, _| Ok(()),
        false,
    )
}

fn migrate_v2_typed_inner<K, SourceV, DestinationV, Map, Store, Validate>(
    source: &Path,
    mut map: Map,
    store: Store,
    validate: Validate,
    indexed: bool,
) -> io::Result<MigrationValidation>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    SourceV: Serialize + for<'de> Deserialize<'de> + Clone,
    DestinationV: Serialize + for<'de> Deserialize<'de> + Clone,
    Map: FnMut(SourceV) -> DestinationV,
    Store: FnOnce(&mut BPlusTree<K, DestinationV>, &Path) -> io::Result<super::tree::VerificationReport>,
    Validate: FnOnce(&Path, usize) -> io::Result<()>,
{
    let version = storage_version(source)?.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not a B+Tree"))?;
    let normalized_source = migration_source(source, version)?;
    let read_path = normalized_source.as_deref().unwrap_or(source);
    let destination = migration_destination(source)?;
    let destination_index = crate::common::get_file_path_for_db_index(&destination);
    let converted = (|| {
        let mut legacy = v2::BPlusTreeQuery::<K, SourceV>::try_new(read_path)?;
        let metadata = legacy_metadata(read_path)?;
        let expected_metadata = metadata.clone();
        let mut tree = BPlusTree::new();
        tree.set_metadata(metadata);
        let mut entries = 0usize;
        let mut corrupted_entries = 0usize;
        for entry in legacy.range_iter(Bound::Unbounded, Bound::Unbounded) {
            let (key, value) = match entry {
                Ok(item) => item,
                Err(crate::common::BPlusTreeError::UnreadableValue(err)) => {
                    log::warn!("Skipping unreadable/corrupted entry in legacy B+Tree {}: {err}", read_path.display());
                    corrupted_entries += 1;
                    continue;
                }
                Err(err) => return Err(err.to_io()),
            };
            tree.insert(key, map(value));
            entries = entries
                .checked_add(1)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "migration entry count overflow"))?;
        }
        drop(legacy);
        if entries == 0 && corrupted_entries > 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("all {corrupted_entries} entries in legacy database were corrupted"),
            ));
        }
        if corrupted_entries > 0 {
            log::warn!(
                "Legacy migration of {} completed with {} healthy entries ({} corrupted entries skipped)",
                read_path.display(),
                entries,
                corrupted_entries
            );
        }
        if tree.len() != entries {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "legacy migration produced duplicate keys"));
        }

        let verification = store(&mut tree, &destination)?;
        if verification.live_entries
            != u64::try_from(entries)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "migration entry count exceeds u64"))?
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "migrated database entry count differs from full verification",
            ));
        }
        let query = super::BPlusTreeQuery::<K, DestinationV>::try_new(&destination)?;
        let (database_id, generation) = query.snapshot_identity();
        if database_id == [0; 16] {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "migrated database identity is zero"));
        }
        if generation != 1 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "migrated database generation is not one"));
        }
        if query.snapshot_metadata() != &expected_metadata {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "migrated database metadata mismatch"));
        }
        drop(query);
        validate(&destination, entries)?;
        if indexed {
            publish(&destination_index, &crate::common::get_file_path_for_db_index(source))?;
        }
        publish(&destination, source)?;
        Ok(MigrationValidation { entries, database_id, generation, corrupted_entries })
    })();
    if let Some(temporary) = normalized_source {
        let _ = std::fs::remove_file(temporary);
    }
    cleanup_destination(&destination);
    converted
}

fn migration_destination(path: &Path) -> io::Result<PathBuf> {
    let name = path.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "database has no name"))?;
    Ok(path.with_file_name(format!("{}.{}.v3.tmp", name.to_string_lossy(), uuid::Uuid::new_v4())))
}

fn publish(temporary: &Path, destination: &Path) -> io::Result<()> {
    let temporary = tempfile::TempPath::try_from_path(temporary)?;
    temporary.persist(destination).map_err(io::Error::from)?;
    super::wal::sync_parent_directory(destination)
}

fn cleanup_destination(destination: &Path) {
    let _ = std::fs::remove_file(destination);
    let _ = std::fs::remove_file(crate::common::get_file_path_for_db_index(destination));
    let _ = std::fs::remove_file(crate::common::sidecar_lock_path(destination));
}

#[cfg(test)]
mod tests {
    use super::{
        super::page::{page_open_count, reset_page_open_count},
        *,
    };

    #[test]
    fn typed_migration_validates_the_v3_destination_once() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("single-pass-v2.db");
        let baseline_path = dir.path().join("single-pass-v3-baseline.db");
        let mut legacy = v2::BPlusTree::new();
        legacy.insert(1u32, String::from("one"));
        legacy.store(&path)?;

        let mut baseline = BPlusTree::new();
        baseline.insert(1u32, String::from("one"));
        reset_page_open_count();
        baseline.store(&baseline_path)?;
        let single_verified_store_opens = page_open_count();

        reset_page_open_count();
        let validation = migrate_v2_typed::<u32, String>(&path)?;
        assert_eq!(validation.entries, 1);
        assert_eq!(page_open_count(), single_verified_store_opens);
        Ok(())
    }

    #[test]
    fn typed_v2_and_v1_sources_become_verified_v3_databases() -> io::Result<()> {
        for version in [LEGACY_V2, LEGACY_V1] {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join(format!("source-v{version}.db"));
            let mut legacy = v2::BPlusTree::new();
            legacy.insert(1u32, String::from("one"));
            legacy.insert(2u32, String::from("two"));
            legacy.store(&path)?;
            if version == LEGACY_V1 {
                let mut file = OpenOptions::new().write(true).open(&path)?;
                file.seek(SeekFrom::Start(4))?;
                file.write_all(&LEGACY_V1.to_le_bytes())?;
                file.sync_all()?;
            }

            let validation = migrate_v2_typed::<u32, String>(&path)?;
            assert_eq!(validation.entries, 2);
            assert_eq!(validation.generation, 1);
            assert_eq!(storage_version(&path)?, Some(3));
            let mut query = super::super::BPlusTreeQuery::<u32, String>::try_new(&path)?;
            assert_eq!(
                query.iter().collect::<io::Result<Vec<_>>>()?,
                vec![(1, String::from("one")), (2, String::from("two"))]
            );
        }
        Ok(())
    }

    #[test]
    fn legacy_scan_supports_v1_without_modifying_the_source() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("inspect-v1.db");
        let mut legacy = v2::BPlusTree::new();
        legacy.insert(1u32, String::from("one"));
        legacy.insert(2u32, String::from("two"));
        legacy.store(&path)?;
        let mut file = OpenOptions::new().write(true).open(&path)?;
        file.seek(SeekFrom::Start(4))?;
        file.write_all(&LEGACY_V1.to_le_bytes())?;
        file.sync_all()?;
        drop(file);
        let before = std::fs::read(&path)?;

        assert_eq!(scan_v2_entries::<u32, String>(&path)?, (2, 0));
        assert_eq!(std::fs::read(&path)?, before);
        Ok(())
    }

    #[test]
    fn typed_migration_accepts_verified_historical_fence_key_nodes() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("historical-fence-v2.db");
        let mut legacy = v2::BPlusTree::new_with_orders(2, 2);
        for key in 0..80u32 {
            legacy.insert(key, format!("value-{key}"));
        }
        assert!(legacy.add_historical_fence_key());
        legacy.store(&path)?;

        let validation = migrate_v2_typed::<u32, String>(&path)?;
        assert_eq!(validation.entries, 80);
        let mut query = super::super::BPlusTreeQuery::<u32, String>::try_new(&path)?;
        assert_eq!(query.iter().collect::<io::Result<Vec<_>>>()?.len(), 80);
        Ok(())
    }

    #[test]
    fn typed_migration_accepts_unaligned_cow_root() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("unaligned-root-v2.db");
        let mut legacy = v2::BPlusTree::new_with_orders(4, 4);
        for key in 0..40u32 {
            legacy.insert(key, format!("value-{key}"));
        }
        legacy.store(&path)?;

        let mut bytes = std::fs::read(&path)?;
        let root_offset = u64::from_le_bytes(
            bytes[8..16].try_into().map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        );
        let root_start =
            usize::try_from(root_offset).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let root_end = root_start
            .checked_add(v2::PAGE_SIZE_USIZE)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "fixture root end overflow"))?;
        let root = bytes
            .get(root_start..root_end)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "fixture root is truncated"))?
            .to_vec();
        bytes.push(0);
        let relocated_root =
            u64::try_from(bytes.len()).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        bytes.extend_from_slice(&root);
        bytes[8..16].copy_from_slice(&relocated_root.to_le_bytes());
        std::fs::write(&path, bytes)?;

        let validation = migrate_v2_typed::<u32, String>(&path)?;
        assert_eq!(validation.entries, 40);
        Ok(())
    }

    #[test]
    fn typed_migration_rejects_missing_root_child_as_corruption() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("missing-root-child-v2.db");
        let mut legacy = v2::BPlusTree::new_with_orders(2, 2);
        for key in 0..20u32 {
            legacy.insert(key, format!("value-{key}"));
        }
        assert!(legacy.remove_last_root_child());
        legacy.store(&path)?;

        assert!(migrate_v2_typed::<u32, String>(&path).is_err());
        assert_eq!(storage_version(&path)?, Some(LEGACY_V2));
        Ok(())
    }

    #[test]
    fn typed_migration_read_failure_keeps_the_legacy_source() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("corrupt-v2.db");
        let mut legacy = v2::BPlusTree::new();
        legacy.insert(1u32, String::from("one"));
        legacy.store(&path)?;
        let mut corrupted = std::fs::read(&path)?;
        corrupted.truncate(v2::PAGE_SIZE_USIZE + 8);
        std::fs::write(&path, &corrupted)?;

        assert!(migrate_v2_typed::<u32, String>(&path).is_err());
        assert_eq!(std::fs::read(&path)?, corrupted);
        Ok(())
    }

    #[test]
    fn typed_migration_rebuilds_and_validates_the_sorted_index() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("indexed-v2.db");
        let mut legacy = v2::BPlusTree::new();
        legacy.insert(1u32, String::from("bbb"));
        legacy.insert(2u32, String::from("a"));
        legacy.store(&path)?;

        let validation = migrate_v2_typed_with_index::<u32, String, usize, _>(&path, String::len)?;
        assert_eq!(validation.entries, 2);
        let query = super::super::BPlusTreeQuery::<u32, String>::try_new(&path)?;
        let index = crate::common::get_file_path_for_db_index(&path);
        let values = crate::sorted_index::v4::OwnedIterator::<u32, String, usize>::open(query, &index)?
            .collect::<io::Result<Vec<_>>>()?;
        assert_eq!(values, vec![(2, String::from("a")), (1, String::from("bbb"))]);
        Ok(())
    }

    #[test]
    fn typed_migration_skips_corrupted_value_and_migrates_healthy_records() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("single-value-corrupted-v2.db");
        let mut legacy = v2::BPlusTree::new();
        let large1 = "A".repeat(500);
        let large2 = "B".repeat(500);
        let large3 = "C".repeat(500);
        legacy.insert(1u32, large1.clone());
        legacy.insert(2u32, large2.clone());
        legacy.insert(3u32, large3.clone());
        legacy.store(&path)?;

        let mut bytes = std::fs::read(&path)?;
        let serialized = crate::codec::binary_serialize(&large2)?;
        let mut pattern = vec![v2::COMPRESSION_FLAG_LZ4];
        pattern.extend_from_slice(&(serialized.len() as u32).to_le_bytes());
        // Find the second occurrence (key 2)
        let mut occurrences = Vec::new();
        for pos in 0..bytes.len().saturating_sub(pattern.len()) {
            if bytes[pos..pos + pattern.len()] == pattern {
                occurrences.push(pos);
            }
        }
        assert!(occurrences.len() >= 2, "found occurrences: {:?}", occurrences.len());
        let pos = occurrences[1];
        // pos is flag (0x01)
        // pos+1..pos+5 is prepended length
        // pos+5 is first token in LZ4.
        // If we set pos+5 = 0x1f (1 literal, 15+ match), pos+6 = 'B' (literal), pos+7..pos+9 = 0x0000 (offset = 0!)
        bytes[pos + 5] = 0x1f;
        bytes[pos + 6] = b'B';
        bytes[pos + 7] = 0x00;
        bytes[pos + 8] = 0x00;
        std::fs::write(&path, &bytes)?;

        let validation = migrate_v2_typed::<u32, String>(&path)?;
        assert_eq!(validation.entries, 2);
        assert_eq!(validation.corrupted_entries, 1);
        let mut query = super::super::BPlusTreeQuery::<u32, String>::try_new(&path)?;
        assert_eq!(query.query(&1).map_err(io::Error::other)?, Some(large1));
        assert_eq!(query.query(&2).map_err(io::Error::other)?, None);
        assert_eq!(query.query(&3).map_err(io::Error::other)?, Some(large3));
        Ok(())
    }
}

use super::{
    database_header, empty_leaf, invalid_data, invalid_input, publish_database, search_leaf, sync_parent_directory,
    try_exclusive_sidecar, verify_full, wal_path, BPlusTree, BPlusTreeMetadata, BPlusTreeQuery, BPlusTreeUpdate,
    ConditionalSerialize, FlushPolicy, LeafCellRef, LeafValueRef, NEXT_PAGE_ID, PAGE_ID,
};
use crate::{
    codec::binary_serialize,
    common::BPlusTreeError,
    v3::{
        format::{encode_inline_leaf_cell, encode_internal_cell, encode_tombstone_leaf_cell, Compression},
        page::SlottedPage,
    },
};
use serde::Deserialize;
use std::{fs, io, path::Path};

#[test]
fn new_key_is_inserted_into_existing_leaf_without_growth() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("leaf-insert.db");
    let mut tree = BPlusTree::new();
    tree.insert(1u32, String::from("one"));
    tree.insert(3u32, String::from("three"));
    tree.store(&path)?;
    let before = database_header(&path)?;
    let before_len = fs::metadata(&path)?.len();

    let mut updater = BPlusTreeUpdate::<u32, String>::try_new(&path)?;
    updater.upsert(&2, &String::from("two"))?;

    let after = database_header(&path)?;
    assert_eq!(after.generation, before.generation + 1);
    assert_eq!(after.root_page_id, before.root_page_id);
    assert_eq!(fs::metadata(&path)?.len(), before_len);
    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    assert_eq!(
        query.iter().collect::<io::Result<Vec<_>>>()?,
        vec![(1, "one".into()), (2, "two".into()), (3, "three".into())]
    );
    let _ = verify_full(&mut query)?;
    Ok(())
}

#[test]
fn delete_writes_tombstone_and_reinsert_reuses_the_leaf() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("tombstone.db");
    let mut tree = BPlusTree::new();
    tree.insert(7u32, String::from("original"));
    tree.store(&path)?;
    let before = database_header(&path)?;
    let before_len = fs::metadata(&path)?.len();

    let mut updater = BPlusTreeUpdate::<u32, String>::try_new(&path)?;
    assert!(updater.delete(&7)?);
    assert_eq!(updater.query(&7).map_err(BPlusTreeError::to_io)?, None);
    updater.upsert(&7, &String::from("restored"))?;

    let after = database_header(&path)?;
    assert_eq!(after.generation, before.generation + 2);
    assert_eq!(after.root_page_id, before.root_page_id);
    assert_eq!(fs::metadata(&path)?.len(), before_len);
    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    assert_eq!(query.query(&7).map_err(BPlusTreeError::to_io)?, Some(String::from("restored")));
    assert_eq!(verify_full(&mut query)?.live_entries, 1);
    Ok(())
}

#[test]
fn identical_batch_metadata_is_a_true_noop() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("metadata-noop.db");
    let mut tree = BPlusTree::<u32, String>::new();
    tree.set_metadata(BPlusTreeMetadata::TargetIdMapping(42));
    tree.store(&path)?;
    let before = fs::read(&path)?;

    let mut updater = BPlusTreeUpdate::<u32, String>::try_new(&path)?;
    updater.set_flush_policy(FlushPolicy::Batch);
    updater.set_metadata(&BPlusTreeMetadata::TargetIdMapping(42))?;
    assert!(updater.active.is_none());
    updater.commit()?;

    assert_eq!(fs::read(&path)?, before);
    assert!(!wal_path(&path).try_exists()?);
    Ok(())
}

#[test]
fn database_with_index_extension_is_never_deleted_as_derived_data() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("tree.idx");
    let mut tree = BPlusTree::new();
    tree.insert(1u32, String::from("old"));
    tree.store(&path)?;

    let mut updater = BPlusTreeUpdate::<u32, String>::try_new(&path)?;
    updater.upsert(&1, &String::from("new"))?;

    assert!(path.try_exists()?);
    assert!(!wal_path(&path).try_exists()?);
    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    assert_eq!(query.query(&1).map_err(BPlusTreeError::to_io)?, Some(String::from("new")));
    let _ = verify_full(&mut query)?;
    Ok(())
}

#[test]
fn failed_batch_mutation_discards_the_whole_overlay() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("poisoned-batch.db");
    let mut tree = BPlusTree::new();
    tree.insert(String::from("key"), String::from("old"));
    tree.store(&path)?;
    let before = fs::read(&path)?;

    let mut updater = BPlusTreeUpdate::<String, String>::try_new(&path)?;
    updater.set_flush_policy(FlushPolicy::Batch);
    updater.upsert(&String::from("key"), &String::from("staged"))?;
    assert!(updater.upsert(&"x".repeat(2_100), &String::from("invalid")).is_err());
    updater.commit()?;

    assert_eq!(fs::read(&path)?, before);
    assert!(!wal_path(&path).try_exists()?);
    let mut query = BPlusTreeQuery::<String, String>::try_new(&path)?;
    assert_eq!(query.query(&String::from("key")).map_err(BPlusTreeError::to_io)?, Some(String::from("old")));
    Ok(())
}

#[test]
fn batch_serialization_error_discards_earlier_items() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("batch-serialization.db");
    let initial = ConditionalSerialize { value: String::from("initial"), fail: false };
    let mut tree = BPlusTree::new();
    tree.insert(0u32, initial);
    tree.store(&path)?;
    let before = fs::read(&path)?;
    let first = ConditionalSerialize { value: String::from("first"), fail: false };
    let failing = ConditionalSerialize { value: String::from("second"), fail: true };

    let mut updater = BPlusTreeUpdate::<u32, ConditionalSerialize>::try_new(&path)?;
    assert!(updater.upsert_batch(&[(&1, &first), (&2, &failing)]).is_err());
    updater.commit()?;

    assert_eq!(fs::read(&path)?, before);
    assert!(!wal_path(&path).try_exists()?);
    let mut query = BPlusTreeQuery::<u32, ConditionalSerialize>::try_new(&path)?;
    assert_eq!(query.query(&1).map_err(BPlusTreeError::to_io)?, None);
    assert_eq!(query.query(&2).map_err(BPlusTreeError::to_io)?, None);
    Ok(())
}

#[test]
fn exclusive_sidecar_probe_child() -> io::Result<()> {
    let Some(path) = std::env::var_os("TULIPROX_V3_LOCK_PROBE_PATH") else {
        return Ok(());
    };
    let expected = std::env::var("TULIPROX_V3_LOCK_PROBE_EXPECTED")
        .map_err(|error| io::Error::other(format!("missing child probe expectation: {error}")))?;
    let acquired = try_exclusive_sidecar(Path::new(&path))?;
    match expected.as_str() {
        "acquired" => assert!(acquired),
        "blocked" => assert!(!acquired),
        _ => return Err(io::Error::other(format!("unknown child probe expectation: {expected}"))),
    }
    Ok(())
}

#[test]
fn typed_leaf_cells_round_trip_and_validate_value_crc() -> io::Result<()> {
    let mut cell = Vec::new();
    encode_inline_leaf_cell(b"key", 5, Compression::None, b"value", &mut cell)?;
    let decoded = LeafCellRef::decode(&cell, PAGE_ID, NEXT_PAGE_ID)?;
    assert_eq!(decoded.key_bytes, b"key");
    match decoded.value {
        LeafValueRef::Inline { compression, logical_len, stored, crc32 } => {
            assert_eq!(compression, Compression::None);
            assert_eq!(logical_len, 5);
            assert_eq!(stored, b"value");
            assert_eq!(crc32, crc32fast::hash(b"value"));
        }
        _ => return Err(io::Error::other("expected inline value")),
    }

    let last = cell.len().checked_sub(1).ok_or_else(|| io::Error::other("empty test cell"))?;
    cell[last] ^= 1;
    invalid_data(LeafCellRef::decode(&cell, PAGE_ID, NEXT_PAGE_ID))
}

#[test]
fn encoded_key_limit_is_2004_bytes() -> io::Result<()> {
    let mut cell = Vec::new();
    encode_tombstone_leaf_cell(&vec![b'k'; 2004], &mut cell)?;
    assert_eq!(cell.len() + 4, 2032);
    invalid_input(encode_tombstone_leaf_cell(&vec![b'k'; 2005], &mut cell))?;

    encode_internal_cell(&vec![b'k'; 2004], 9, PAGE_ID, NEXT_PAGE_ID, &mut cell)?;
    invalid_input(encode_internal_cell(&vec![b'k'; 2005], 9, PAGE_ID, NEXT_PAGE_ID, &mut cell))
}

#[test]
fn typed_search_decodes_only_binary_search_candidates() -> io::Result<()> {
    let mut cells = Vec::new();
    for key in 0u8..8 {
        let mut cell = Vec::new();
        encode_inline_leaf_cell(&binary_serialize(&key)?, 1, Compression::None, &[key], &mut cell)?;
        cells.push(cell);
    }
    if let Some(byte) = cells.get_mut(0).and_then(|cell| cell.get_mut(24)) {
        *byte = 0xc1;
    }

    let mut bytes = empty_leaf(PAGE_ID, NEXT_PAGE_ID)?;
    let mut mutable = SlottedPage::open(bytes.as_mut_slice(), PAGE_ID, NEXT_PAGE_ID)?;
    mutable.rebuild_ordered(cells.iter().map(Vec::as_slice))?;
    let page = SlottedPage::open(bytes.as_slice(), PAGE_ID, NEXT_PAGE_ID)?;
    assert_eq!(search_leaf(&page, &7u8)?, Ok(7));
    invalid_data(search_leaf(&page, &0u8))
}

#[derive(Debug)]
pub(in crate::v3::tree::tests) struct RejectValueDeserialize;

impl<'de> Deserialize<'de> for RejectValueDeserialize {
    fn deserialize<D: serde::Deserializer<'de>>(_deserializer: D) -> Result<Self, D::Error> {
        Err(serde::de::Error::custom("value deserialization must not run"))
    }
}

#[test]
fn contains_and_len_read_only_leaf_descriptors() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("descriptor-only.db");
    let mut tree = BPlusTree::new();
    tree.insert(1u32, String::from("one"));
    tree.insert(2u32, String::from("two"));
    tree.store(&path)?;

    let mut query = BPlusTreeQuery::<u32, RejectValueDeserialize>::try_new(&path)?;
    assert!(query.contains_live_key(&1).map_err(BPlusTreeError::to_io)?);
    assert!(!query.contains_live_key(&3).map_err(BPlusTreeError::to_io)?);
    assert_eq!(query.len().map_err(BPlusTreeError::to_io)?, 2);
    Ok(())
}

#[test]
fn publish_failure_removes_the_temporary_file() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let destination = dir.path().join("destination-directory");
    fs::create_dir(&destination)?;
    let temporary = dir.path().join("database.tx.v3.tmp");
    fs::write(&temporary, b"new")?;

    let error = publish_database(&temporary, &destination, sync_parent_directory)
        .expect_err("publishing over a directory must fail");
    assert!(!error.database_was_published());
    assert!(!temporary.exists());
    Ok(())
}

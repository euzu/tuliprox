use super::{
    assert_writer_is_blocked, choose_internal_split, choose_leaf_split, database_header, decompress_value_in_place,
    empty_leaf, finish_writer, internal_key_decode_count, invalid_data, invalid_input, page_byte_range, random_value,
    read_leaf_value, reset_internal_key_decode_count, search_leaf, spawn_replacement_writer, try_exclusive_sidecar,
    used_leaf_bytes, validate_locator, verify_full, wait_for_exclusive_sidecar, BPlusTree, BPlusTreeQuery,
    BPlusTreeUpdate, DatabaseHeader, InternalCellRef, LeafCellRef, LeafValueRef, Locator, NEXT_PAGE_ID, PAGE_ID,
    SLOT_LEN,
};
use crate::{
    codec::binary_serialize,
    common::BPlusTreeError,
    v3::{
        format::{
            encode_inline_leaf_cell, encode_internal_cell, encode_overflow_leaf_cell, encode_tombstone_leaf_cell,
            encode_value, Compression, PageType, OVERFLOW_PAYLOAD_LEN, PAGE_HEADER_LEN, PAGE_SIZE,
        },
        page::{encode_free_page, encode_overflow_page, page_open_count, reset_page_open_count, SlottedPage},
    },
};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fs, io, path::Path, process::Command};

#[test]
fn point_query_validates_a_single_leaf_once() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("point-query-page-opens.db");
    let mut tree = BPlusTree::new();
    tree.insert(7u32, String::from("value"));
    tree.store(&path)?;

    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    reset_page_open_count();
    assert_eq!(query.query(&7).map_err(BPlusTreeError::to_io)?, Some(String::from("value")));
    assert_eq!(page_open_count(), 1);
    Ok(())
}

#[test]
fn repeated_point_query_reuses_snapshot_page_validation_and_internal_keys() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("repeated-point-query.db");
    let mut tree = BPlusTree::new();
    for key in 0..2_000u32 {
        tree.insert(key, format!("value-{key:04}"));
    }
    tree.store(&path)?;

    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    reset_page_open_count();
    reset_internal_key_decode_count();
    assert_eq!(query.query(&1_337).map_err(BPlusTreeError::to_io)?, Some(String::from("value-1337")));
    let first_page_opens = page_open_count();
    let first_internal_decodes = internal_key_decode_count();
    if first_page_opens < 2 || first_internal_decodes == 0 {
        return Err(io::Error::other("test fixture must contain internal pages"));
    }

    assert_eq!(query.query(&1_337).map_err(BPlusTreeError::to_io)?, Some(String::from("value-1337")));
    assert_eq!(page_open_count(), first_page_opens);
    assert_eq!(internal_key_decode_count(), first_internal_decodes);
    Ok(())
}

#[test]
fn cloned_query_reuses_snapshot_page_validation_and_internal_keys() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("cloned-point-query.db");
    let mut tree = BPlusTree::new();
    for key in 0..2_000u32 {
        tree.insert(key, format!("value-{key:04}"));
    }
    tree.store(&path)?;

    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    reset_page_open_count();
    reset_internal_key_decode_count();
    assert_eq!(query.query(&1_337).map_err(BPlusTreeError::to_io)?, Some(String::from("value-1337")));
    let first_page_opens = page_open_count();
    let first_internal_decodes = internal_key_decode_count();
    if first_page_opens < 2 || first_internal_decodes == 0 {
        return Err(io::Error::other("test fixture must contain internal pages"));
    }

    let mut cloned = query.try_clone()?;
    assert_eq!(cloned.query(&1_337).map_err(BPlusTreeError::to_io)?, Some(String::from("value-1337")));
    assert_eq!(page_open_count(), first_page_opens);
    assert_eq!(internal_key_decode_count(), first_internal_decodes);
    Ok(())
}

#[test]
fn repeated_query_le_reuses_snapshot_page_validation_and_internal_keys() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("repeated-query-le.db");
    let mut tree = BPlusTree::new();
    for key in 0..2_000u32 {
        tree.insert(key, format!("value-{key:04}"));
    }
    tree.store(&path)?;

    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    reset_page_open_count();
    reset_internal_key_decode_count();
    assert_eq!(query.query_le(&1_337).map_err(BPlusTreeError::to_io)?, Some(String::from("value-1337")));
    let first_page_opens = page_open_count();
    let first_internal_decodes = internal_key_decode_count();
    if first_page_opens < 2 || first_internal_decodes == 0 {
        return Err(io::Error::other("test fixture must contain internal pages"));
    }

    assert_eq!(query.query_le(&1_337).map_err(BPlusTreeError::to_io)?, Some(String::from("value-1337")));
    assert_eq!(page_open_count(), first_page_opens);
    assert_eq!(internal_key_decode_count(), first_internal_decodes);
    Ok(())
}

#[test]
fn locator_collection_validates_each_leaf_once() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("locator-page-opens.db");
    let mut tree = BPlusTree::new();
    for key in 0..3u32 {
        tree.insert(key, format!("value-{key}"));
    }
    tree.store(&path)?;

    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    reset_page_open_count();
    assert_eq!(query.collect_with_locators()?.len(), 3);
    assert_eq!(page_open_count(), 1);
    Ok(())
}

#[test]
fn equal_size_inline_update_rewrites_without_appending_pages() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("equal-inline.db");
    let mut tree = BPlusTree::new();
    tree.insert(7u32, String::from("old"));
    tree.store(&path)?;
    let before = database_header(&path)?;
    let before_len = fs::metadata(&path)?.len();

    let mut updater = BPlusTreeUpdate::<u32, String>::try_new(&path)?;
    updater.update(&7, String::from("new")).map_err(BPlusTreeError::to_io)?;
    drop(updater);

    let after = database_header(&path)?;
    assert_eq!(after.generation, before.generation + 1);
    assert_eq!(after.root_page_id, before.root_page_id);
    assert_eq!(fs::metadata(&path)?.len(), before_len);
    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    assert_eq!(query.query(&7).map_err(BPlusTreeError::to_io)?, Some(String::from("new")));
    let _ = verify_full(&mut query)?;
    Ok(())
}

pub(in crate::v3::tree::tests) fn overflow_head(path: &Path, key: u32) -> io::Result<u64> {
    let mut query = BPlusTreeQuery::<u32, Vec<u8>>::try_new(path)?;
    let leaf = query.locate_leaf(&key)?;
    let next_page_id = query.header.next_page_id;
    query.with_page(leaf, |bytes, _| {
        let page = SlottedPage::open(bytes, leaf, next_page_id)?;
        let index = search_leaf(&page, &key)?.map_err(|_| io::Error::other("test key is missing"))?;
        match LeafCellRef::decode(page.cell(index)?, leaf, next_page_id)?.value {
            LeafValueRef::Overflow { head, .. } => Ok(head),
            LeafValueRef::Inline { .. } | LeafValueRef::Tombstone => {
                Err(io::Error::other("test value is not overflow-backed"))
            }
        }
    })
}

#[test]
fn overflow_update_reuses_head_and_frees_unused_tail() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("overflow-reuse.db");
    let large = random_value();
    let smaller = large.get(..5_000).ok_or_else(|| io::Error::other("random test value is too short"))?.to_vec();
    let mut tree = BPlusTree::new();
    tree.insert(7u32, large);
    tree.store(&path)?;
    let before = database_header(&path)?;
    let before_len = fs::metadata(&path)?.len();
    let before_head = overflow_head(&path, 7)?;

    let mut updater = BPlusTreeUpdate::<u32, Vec<u8>>::try_new(&path)?;
    updater.update(&7, smaller.clone()).map_err(BPlusTreeError::to_io)?;

    let after = database_header(&path)?;
    assert_eq!(after.generation, before.generation + 1);
    assert_eq!(fs::metadata(&path)?.len(), before_len);
    assert_eq!(overflow_head(&path, 7)?, before_head);
    assert_ne!(after.free_page_head, 0);
    let mut query = BPlusTreeQuery::<u32, Vec<u8>>::try_new(&path)?;
    assert_eq!(query.query(&7).map_err(BPlusTreeError::to_io)?, Some(smaller));
    let _ = verify_full(&mut query)?;
    Ok(())
}

#[test]
fn inline_to_overflow_update_allocates_new_pages() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("overflow-allocation.db");
    let mut tree = BPlusTree::new();
    tree.insert(7u32, vec![7]);
    tree.store(&path)?;
    let before = database_header(&path)?;
    let before_len = fs::metadata(&path)?.len();
    let large = random_value();

    let mut updater = BPlusTreeUpdate::<u32, Vec<u8>>::try_new(&path)?;
    updater.update(&7, large.clone()).map_err(BPlusTreeError::to_io)?;

    let after = database_header(&path)?;
    assert_eq!(after.generation, before.generation + 1);
    assert!(fs::metadata(&path)?.len() > before_len);
    assert_ne!(overflow_head(&path, 7)?, 0);
    let mut query = BPlusTreeQuery::<u32, Vec<u8>>::try_new(&path)?;
    assert_eq!(query.query(&7).map_err(BPlusTreeError::to_io)?, Some(large));
    let _ = verify_full(&mut query)?;
    Ok(())
}

#[test]
fn leaf_split_repairs_siblings_and_creates_a_new_root() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("leaf-split.db");
    let value: String = random_value()
        .get(..200)
        .ok_or_else(|| io::Error::other("random test value is too short"))?
        .iter()
        .map(|byte| char::from(33 + byte % 90))
        .collect();
    let mut tree = BPlusTree::new();
    for key in 0..17u32 {
        tree.insert(key, value.clone());
    }
    tree.store(&path)?;
    let before = database_header(&path)?;
    let before_len = fs::metadata(&path)?.len();

    let mut updater = BPlusTreeUpdate::<u32, String>::try_new(&path)?;
    updater.upsert(&17, &value)?;

    let after = database_header(&path)?;
    assert_eq!(after.generation, before.generation + 1);
    assert_ne!(after.root_page_id, before.root_page_id);
    assert_eq!(fs::metadata(&path)?.len(), before_len + 2 * u64::try_from(PAGE_SIZE).map_err(io::Error::other)?);
    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    let report = verify_full(&mut query)?;
    assert_eq!(report.live_entries, 18);
    assert_eq!(query.query(&17).map_err(BPlusTreeError::to_io)?, Some(value));
    Ok(())
}

#[test]
fn splitting_a_non_rightmost_leaf_repairs_the_former_neighbor() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("middle-leaf-split.db");
    let value = random_value().get(..200).ok_or_else(|| io::Error::other("random test value is too short"))?.to_vec();
    let mut tree = BPlusTree::new();
    for key in (0..800u32).step_by(2) {
        tree.insert(key, value.clone());
    }
    tree.store(&path)?;
    let before = database_header(&path)?;
    let mut query = BPlusTreeQuery::<u32, Vec<u8>>::try_new(&path)?;
    let left_leaf = query.locate_leaf(&0)?;
    let former_right = query.with_page(left_leaf, |bytes, _| {
        Ok(SlottedPage::open(bytes, left_leaf, before.next_page_id)?.header().right)
    })?;
    if former_right == 0 {
        return Err(io::Error::other("fixture did not create a right leaf neighbor"));
    }
    drop(query);

    let mut updater = BPlusTreeUpdate::<u32, Vec<u8>>::try_new(&path)?;
    updater.upsert(&1, &value)?;

    let after = database_header(&path)?;
    assert_eq!(after.root_page_id, before.root_page_id);
    assert!(after.next_page_id > before.next_page_id);
    let mut query = BPlusTreeQuery::<u32, Vec<u8>>::try_new(&path)?;
    let new_right = query
        .with_page(left_leaf, |bytes, _| Ok(SlottedPage::open(bytes, left_leaf, after.next_page_id)?.header().right))?;
    assert_ne!(new_right, former_right);
    query.with_page(new_right, |bytes, _| {
        let page = SlottedPage::open(bytes, new_right, after.next_page_id)?;
        assert_eq!(page.header().left, left_leaf);
        assert_eq!(page.header().right, former_right);
        Ok(())
    })?;
    query.with_page(former_right, |bytes, _| {
        let page = SlottedPage::open(bytes, former_right, after.next_page_id)?;
        assert_eq!(page.header().left, new_right);
        Ok(())
    })?;
    let _ = verify_full(&mut query)?;
    Ok(())
}

pub(in crate::v3::tree::tests) fn long_split_key(index: u32, marker: char) -> String {
    format!("{index:04}{marker}{}", "k".repeat(1_880))
}

#[test]
fn leaf_promotion_recursively_splits_a_full_internal_root() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("internal-split.db");
    let value = String::from("value");
    let mut tree = BPlusTree::new();
    for index in 0..6u32 {
        tree.insert(long_split_key(index, '-'), value.clone());
    }
    tree.store(&path)?;
    let before = database_header(&path)?;
    let before_root = fs::read(&path)?;
    let range = page_byte_range(before.root_page_id, before_root.len())?;
    let root_page = SlottedPage::open(
        before_root.get(range).ok_or_else(|| io::Error::other("root page is missing"))?,
        before.root_page_id,
        before.next_page_id,
    )?;
    assert_eq!(root_page.header().page_type, PageType::Internal);
    assert_eq!(root_page.header().cell_count, 2);

    let inserted_key = long_split_key(1, 'z');
    let mut updater = BPlusTreeUpdate::<String, String>::try_new(&path)?;
    updater.upsert(&inserted_key, &value)?;

    let after = database_header(&path)?;
    assert_eq!(after.generation, before.generation + 1);
    assert_ne!(after.root_page_id, before.root_page_id);
    let mut query = BPlusTreeQuery::<String, String>::try_new(&path)?;
    let report = verify_full(&mut query)?;
    assert_eq!(report.live_entries, 7);
    assert_eq!(query.query(&inserted_key).map_err(BPlusTreeError::to_io)?, Some(value));
    Ok(())
}

#[test]
fn freed_overflow_pages_are_reused_before_file_growth() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("free-reuse.db");
    let large = random_value();
    let mut tree = BPlusTree::new();
    tree.insert(1u32, large.clone());
    tree.store(&path)?;
    let before = database_header(&path)?;
    let before_len = fs::metadata(&path)?.len();

    let mut updater = BPlusTreeUpdate::<u32, Vec<u8>>::try_new(&path)?;
    assert!(updater.delete(&1)?);
    assert_ne!(database_header(&path)?.free_page_head, 0);
    updater.upsert(&2, &large)?;

    let after = database_header(&path)?;
    assert_eq!(after.generation, before.generation + 2);
    assert_eq!(fs::metadata(&path)?.len(), before_len);
    let mut query = BPlusTreeQuery::<u32, Vec<u8>>::try_new(&path)?;
    assert_eq!(query.query(&1).map_err(BPlusTreeError::to_io)?, None);
    assert_eq!(query.query(&2).map_err(BPlusTreeError::to_io)?, Some(large));
    assert_eq!(verify_full(&mut query)?.live_entries, 1);
    Ok(())
}

/// Point-query benchmark for the persisted read path.
///
/// `bench_store_throughput` only covers writing. The request hot path is
/// `BPlusTreeQuery::query_zero_copy` against an mmapped database, so the
/// extraction gate needs a read-side number too. Everything here is derived
/// from fixed seeds so two runs on the same host compare like for like: the
/// values come from the same LCG the store benchmark uses, and the lookup
/// order is a fixed-stride walk that defeats sequential prefetching without
/// needing a random number generator.
#[test]
#[ignore = "benchmark; run explicitly with --ignored --nocapture"]
fn bench_point_query_throughput() -> io::Result<()> {
    // Coprime with every entry count below, so the walk visits each key exactly
    // once and never degenerates into a scan.
    const STRIDE: u32 = 7_919;
    const LOOKUPS: u32 = 200_000;

    let noise = |seed: u32, len: usize| -> Vec<u8> {
        let mut state = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                u8::try_from(state >> 24).unwrap_or(0)
            })
            .collect()
    };

    for (label, count, size) in [("klein", 2_000u32, 800usize), ("mittel", 20_000, 2_000), ("gross", 60_000, 2_000)] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("bench.db");
        let mut tree = BPlusTree::<u32, Vec<u8>>::new();
        for key in 0..count {
            tree.insert(key, noise(key, size));
        }
        tree.store(&path)?;

        let mut query = BPlusTreeQuery::<u32, Vec<u8>>::try_new(&path)?;
        // Fault the mapping in first; this benchmark measures lookup, not page-in.
        let mut checksum = 0u64;
        for key in 0..count {
            checksum +=
                query.query_zero_copy(&key).map_err(BPlusTreeError::to_io)?.map_or(0, |value| u64::from(value[0]));
        }

        let start = std::time::Instant::now();
        let mut key = 0u32;
        for _ in 0..LOOKUPS {
            key = (key + STRIDE) % count;
            checksum +=
                query.query_zero_copy(&key).map_err(BPlusTreeError::to_io)?.map_or(0, |value| u64::from(value[0]));
        }
        let elapsed = start.elapsed();

        assert!(checksum > 0, "lookups must not be optimized away");
        println!(
            "BENCH query {label:6} entries={count:6} bytes={size:5} lookups={LOOKUPS} -> {:>8.1} ms  {:>9.0} lookups/s",
            elapsed.as_secs_f64() * 1000.0,
            f64::from(LOOKUPS) / elapsed.as_secs_f64()
        );
    }
    Ok(())
}

pub(in crate::v3::tree::tests) fn run_exclusive_probe_child(database: &Path, expected: &str) -> io::Result<()> {
    let status = Command::new(std::env::current_exe()?)
        .arg("--exact")
        .arg("v3::tree::tests::exclusive_sidecar_probe_child")
        .arg("--nocapture")
        .env("TULIPROX_V3_LOCK_PROBE_PATH", database)
        .env("TULIPROX_V3_LOCK_PROBE_EXPECTED", expected)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("exclusive sidecar child probe failed with {status}")))
    }
}

#[test]
fn owned_iterator_keeps_shared_guard_after_query_is_consumed() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("owned-iterator.db");
    let mut tree = BPlusTree::new();
    tree.insert(1u32, String::from("original"));
    tree.store(&path)?;

    let query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    let iterator = query.disk_iter();
    assert!(!try_exclusive_sidecar(&path)?);
    let (receiver, handle) = spawn_replacement_writer(path.clone())?;
    assert_writer_is_blocked(&receiver)?;
    drop(iterator);
    finish_writer(&receiver, handle)?;
    assert!(wait_for_exclusive_sidecar(&path)?);
    Ok(())
}

#[test]
fn shared_query_blocks_exclusive_writer_in_another_process() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("two-process.db");
    let mut tree = BPlusTree::new();
    tree.insert(1u32, String::from("original"));
    tree.store(&path)?;

    let query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    run_exclusive_probe_child(&path, "blocked")?;
    drop(query);
    run_exclusive_probe_child(&path, "acquired")
}

pub(in crate::v3::tree::tests) fn leaf_cell_of_footprint(footprint: usize) -> io::Result<Vec<u8>> {
    let key_length = footprint.checked_sub(4 + 24).ok_or_else(|| io::Error::other("footprint is too small"))?;
    if footprint <= 2032 {
        let key = vec![b'k'; key_length];
        let mut cell = Vec::new();
        encode_tombstone_leaf_cell(&key, &mut cell)?;
        Ok(cell)
    } else {
        Ok(vec![0; footprint - 4])
    }
}

#[test]
fn compressed_inline_tombstone_and_overflow_descriptors_round_trip() -> io::Result<()> {
    let raw = [0u8; 128];
    let mut compression_scratch = Vec::new();
    let stored = encode_value(&raw, &mut compression_scratch)?;
    assert_eq!(stored.compression(), Compression::Lz4);

    let mut cell = Vec::new();
    encode_inline_leaf_cell(
        b"compressed",
        u32::try_from(raw.len()).map_err(io::Error::other)?,
        stored.compression(),
        stored.as_slice(),
        &mut cell,
    )?;
    let decoded = LeafCellRef::decode(&cell, PAGE_ID, NEXT_PAGE_ID)?;
    let mut value_scratch = Vec::new();
    assert_eq!(read_leaf_value(&[], &decoded.value, NEXT_PAGE_ID, 1024, &mut value_scratch)?, Some(raw.as_slice()));

    encode_tombstone_leaf_cell(b"deleted", &mut cell)?;
    assert!(matches!(LeafCellRef::decode(&cell, PAGE_ID, NEXT_PAGE_ID)?.value, LeafValueRef::Tombstone));

    encode_overflow_leaf_cell(
        b"large",
        5000,
        Compression::None,
        5000,
        2,
        0x1234_5678,
        PAGE_ID,
        NEXT_PAGE_ID,
        &mut cell,
    )?;
    assert!(matches!(LeafCellRef::decode(&cell, PAGE_ID, NEXT_PAGE_ID)?.value, LeafValueRef::Overflow { head: 2, .. }));
    Ok(())
}

#[test]
fn internal_separator_and_locator_have_exact_codecs() -> io::Result<()> {
    let mut cell = Vec::new();
    encode_internal_cell(b"separator", 9, PAGE_ID, NEXT_PAGE_ID, &mut cell)?;
    let decoded = InternalCellRef::decode(&cell, PAGE_ID, NEXT_PAGE_ID)?;
    assert_eq!(decoded.key_bytes, b"separator");
    assert_eq!(decoded.right_child, 9);

    let locator = Locator::for_key(7, 3, b"separator")?;
    let encoded = locator.encode();
    assert_eq!(encoded.len(), 16);
    assert_eq!(Locator::decode(&encoded)?, locator);
    let mut corrupt = encoded;
    corrupt[10] = 1;
    invalid_data(Locator::decode(&corrupt))
}

#[test]
fn locator_rejects_wrong_key_crc_and_wrong_primary_key() -> io::Result<()> {
    let key = binary_serialize(&42u32)?;
    let mut cell = Vec::new();
    encode_inline_leaf_cell(&key, 1, Compression::None, &[7], &mut cell)?;
    let mut bytes = empty_leaf(PAGE_ID, NEXT_PAGE_ID)?;
    let mut page = SlottedPage::open(bytes.as_mut_slice(), PAGE_ID, NEXT_PAGE_ID)?;
    page.rebuild_ordered([cell.as_slice()])?;

    let locator = Locator::for_key(PAGE_ID, 0, &key)?;
    validate_locator(&page, locator, &key)?;
    let bad_crc = Locator { serialized_key_crc32: locator.serialized_key_crc32 ^ 1, ..locator };
    invalid_data(validate_locator(&page, bad_crc, &key))?;
    invalid_data(validate_locator(&page, locator, &binary_serialize(&43u32)?))
}

#[test]
fn adversarial_leaf_splits_reject_old_limit_and_accept_capped_cells() -> io::Result<()> {
    let first_witness = [leaf_cell_of_footprint(2026)?, leaf_cell_of_footprint(2040)?, leaf_cell_of_footprint(2038)?];
    invalid_input(choose_leaf_split(&first_witness))?;

    let second_witness = [leaf_cell_of_footprint(1984)?, leaf_cell_of_footprint(2296)?, leaf_cell_of_footprint(1984)?];
    invalid_input(choose_leaf_split(&second_witness))?;

    let capped = [leaf_cell_of_footprint(2026)?, leaf_cell_of_footprint(2032)?, leaf_cell_of_footprint(2032)?];
    let split = choose_leaf_split(&capped)?;
    assert_eq!(split, 2);
    assert!(used_leaf_bytes(&capped[..split])? <= PAGE_SIZE);
    assert!(used_leaf_bytes(&capped[split..])? <= PAGE_SIZE);
    Ok(())
}

#[test]
fn leaf_split_selection_is_deterministic_for_variable_sizes() -> io::Result<()> {
    let mut seed = 0x5eed_u64;
    for count in 3..36 {
        let mut cells = Vec::with_capacity(count);
        for _ in 0..count {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let footprint = 100 + usize::try_from(seed % 60).map_err(io::Error::other)?;
            let insert_at =
                if cells.is_empty() { 0 } else { usize::try_from(seed).map_err(io::Error::other)? % (cells.len() + 1) };
            cells.insert(insert_at, leaf_cell_of_footprint(footprint)?);
        }
        if used_leaf_bytes(&cells)? > PAGE_SIZE {
            let first = choose_leaf_split(&cells)?;
            let second = choose_leaf_split(&cells)?;
            assert_eq!(first, second);
            assert!(used_leaf_bytes(&cells[..first])? <= PAGE_SIZE);
            assert!(used_leaf_bytes(&cells[first..])? <= PAGE_SIZE);
        }
    }
    Ok(())
}

#[test]
fn internal_split_promotes_exact_separator_and_child() -> io::Result<()> {
    let mut cells = Vec::new();
    for (key_length, child) in [(1000, 2), (1000, 3), (1000, 4), (1000, 5)] {
        let mut cell = Vec::new();
        encode_internal_cell(&vec![b'k'; key_length], child, PAGE_ID, NEXT_PAGE_ID, &mut cell)?;
        cells.push(cell);
    }
    let split = choose_internal_split(&cells, PAGE_ID, NEXT_PAGE_ID)?;
    assert_eq!(split.promoted_index, 1);
    assert_eq!(split.promoted.key_bytes.len(), 1000);
    assert_eq!(split.right_leftmost_child, 3);
    assert_eq!(split.left_cells, &cells[..1]);
    assert_eq!(split.right_cells, &cells[2..]);
    Ok(())
}

#[test]
fn overflow_chain_round_trip_is_bounded_and_checksum_validated() -> io::Result<()> {
    let stored = vec![0x5a; 5000];
    let mut database = vec![0; PAGE_SIZE * 4];
    let first = encode_overflow_page(2, NEXT_PAGE_ID, 3, &stored[..4056])?;
    let second = encode_overflow_page(3, NEXT_PAGE_ID, 0, &stored[4056..])?;
    database[PAGE_SIZE * 2..PAGE_SIZE * 3].copy_from_slice(&first);
    database[PAGE_SIZE * 3..PAGE_SIZE * 4].copy_from_slice(&second);

    let value = LeafValueRef::Overflow {
        compression: Compression::None,
        logical_len: 5000,
        stored_len: 5000,
        head: 2,
        crc32: crc32fast::hash(&stored),
    };
    let mut scratch = Vec::new();
    assert_eq!(read_leaf_value(&database, &value, NEXT_PAGE_ID, 6000, &mut scratch)?, Some(stored.as_slice()));

    let free = encode_free_page(3, NEXT_PAGE_ID, 0)?;
    database[PAGE_SIZE * 3..PAGE_SIZE * 4].copy_from_slice(&free);
    invalid_data(read_leaf_value(&database, &value, NEXT_PAGE_ID, 6000, &mut scratch))
}

#[test]
fn compressed_overflow_chain_reuses_scratch_and_round_trips() -> io::Result<()> {
    let mut raw = Vec::with_capacity(10_000);
    let mut state = 0x51a7_9e2d_4c83_b6f0_u64;
    for _ in 0..5000 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        raw.push(state.to_le_bytes()[0]);
    }
    raw.resize(10_000, 0);
    let mut encoded = Vec::new();
    let stored = encode_value(&raw, &mut encoded)?;
    if stored.compression() != Compression::Lz4 || stored.as_slice().len() <= OVERFLOW_PAYLOAD_LEN {
        return Err(io::Error::other("test value must span compressed overflow pages"));
    }
    let stored = stored.as_slice().to_vec();
    let mut corrupt = stored.clone();
    corrupt
        .get_mut(..4)
        .ok_or_else(|| io::Error::other("missing test LZ4 length"))?
        .copy_from_slice(&1u32.to_le_bytes());
    invalid_data(decompress_value_in_place(
        &mut corrupt,
        u32::try_from(raw.len()).map_err(io::Error::other)?,
        raw.len(),
    ))?;
    let page_count = stored.len().div_ceil(OVERFLOW_PAYLOAD_LEN);
    assert!(page_count > 1);
    let next_page_id = u64::try_from(page_count)
        .map_err(io::Error::other)?
        .checked_add(2)
        .ok_or_else(|| io::Error::other("test page count overflow"))?;
    let mut database = vec![0; usize::try_from(next_page_id).map_err(io::Error::other)? * PAGE_SIZE];
    for (index, payload) in stored.chunks(OVERFLOW_PAYLOAD_LEN).enumerate() {
        let page_id = u64::try_from(index).map_err(io::Error::other)? + 1;
        let next = if index + 1 == page_count { 0 } else { page_id + 1 };
        let page = encode_overflow_page(page_id, next_page_id, next, payload)?;
        let start = usize::try_from(page_id).map_err(io::Error::other)? * PAGE_SIZE;
        database[start..start + PAGE_SIZE].copy_from_slice(&page);
    }
    let value = LeafValueRef::Overflow {
        compression: Compression::Lz4,
        logical_len: u32::try_from(raw.len()).map_err(io::Error::other)?,
        stored_len: u32::try_from(stored.len()).map_err(io::Error::other)?,
        head: 1,
        crc32: crc32fast::hash(&stored),
    };
    let mut scratch = Vec::new();
    assert_eq!(read_leaf_value(&database, &value, next_page_id, raw.len(), &mut scratch)?, Some(raw.as_slice()));
    Ok(())
}

#[test]
fn overflow_chain_rejects_truncation_cycle_oversize_and_bad_crc() -> io::Result<()> {
    let stored = vec![0x5a; 5000];
    let value = LeafValueRef::Overflow {
        compression: Compression::None,
        logical_len: 5000,
        stored_len: 5000,
        head: 2,
        crc32: crc32fast::hash(&stored),
    };
    let mut database = vec![0; PAGE_SIZE * 4];
    let first = encode_overflow_page(2, NEXT_PAGE_ID, 3, &stored[..4056])?;
    let short = encode_overflow_page(3, NEXT_PAGE_ID, 0, &stored[4056..4999])?;
    database[PAGE_SIZE * 2..PAGE_SIZE * 3].copy_from_slice(&first);
    database[PAGE_SIZE * 3..PAGE_SIZE * 4].copy_from_slice(&short);
    let mut scratch = Vec::new();
    invalid_data(read_leaf_value(&database, &value, NEXT_PAGE_ID, 6000, &mut scratch))?;

    let cycle = encode_overflow_page(3, NEXT_PAGE_ID, 2, &stored[4056..])?;
    database[PAGE_SIZE * 3..PAGE_SIZE * 4].copy_from_slice(&cycle);
    invalid_data(read_leaf_value(&database, &value, NEXT_PAGE_ID, 6000, &mut scratch))?;
    invalid_data(read_leaf_value(&database, &value, NEXT_PAGE_ID, 4999, &mut scratch))?;

    let last = encode_overflow_page(3, NEXT_PAGE_ID, 0, &stored[4056..])?;
    database[PAGE_SIZE * 3..PAGE_SIZE * 4].copy_from_slice(&last);
    let LeafValueRef::Overflow { compression, logical_len, stored_len, head, crc32 } = value else {
        return Err(io::Error::other("expected overflow value"));
    };
    let bad_crc = LeafValueRef::Overflow { compression, logical_len, stored_len, head, crc32: crc32 ^ 1 };
    invalid_data(read_leaf_value(&database, &bad_crc, NEXT_PAGE_ID, 6000, &mut scratch))?;

    database[PAGE_SIZE * 2 + 40] ^= 1;
    invalid_data(read_leaf_value(&database, &value, NEXT_PAGE_ID, 6000, &mut scratch))?;

    let empty = encode_overflow_page(2, NEXT_PAGE_ID, 3, &[])?;
    let one_byte = encode_overflow_page(3, NEXT_PAGE_ID, 0, b"x")?;
    database[PAGE_SIZE * 2..PAGE_SIZE * 3].copy_from_slice(&empty);
    database[PAGE_SIZE * 3..PAGE_SIZE * 4].copy_from_slice(&one_byte);
    let empty_chain_value = LeafValueRef::Overflow {
        compression: Compression::None,
        logical_len: 1,
        stored_len: 1,
        head: 2,
        crc32: crc32fast::hash(b"x"),
    };
    invalid_data(read_leaf_value(&database, &empty_chain_value, NEXT_PAGE_ID, 6000, &mut scratch))
}

pub(in crate::v3::tree::tests) fn page_range(page_id: u64) -> io::Result<std::ops::Range<usize>> {
    let start = usize::try_from(page_id)
        .map_err(io::Error::other)?
        .checked_mul(PAGE_SIZE)
        .ok_or_else(|| io::Error::other("test page offset overflow"))?;
    Ok(start..start + PAGE_SIZE)
}

pub(in crate::v3::tree::tests) fn rewrite_page_checksum(database: &mut [u8], page_id: u64) -> io::Result<()> {
    let range = page_range(page_id)?;
    crate::v3::format::write_page_checksum(
        database.get_mut(range).ok_or_else(|| io::Error::other("test page missing"))?,
    )
}

pub(in crate::v3::tree::tests) fn set_page_reference(
    database: &mut [u8],
    page_id: u64,
    offset: usize,
    reference: u64,
) -> io::Result<()> {
    let page = page_range(page_id)?;
    let start = page.start + offset;
    database
        .get_mut(start..start + 8)
        .ok_or_else(|| io::Error::other("test reference missing"))?
        .copy_from_slice(&reference.to_le_bytes());
    rewrite_page_checksum(database, page_id)
}

pub(in crate::v3::tree::tests) fn verify_rejects<K, V>(database: &[u8]) -> io::Result<()>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("corrupt.db");
    fs::write(&path, database)?;
    let mut query = BPlusTreeQuery::<K, V>::try_new(&path)?;
    invalid_data(verify_full(&mut query))
}

pub(in crate::v3::tree::tests) fn stored_tree_fixture() -> io::Result<(Vec<u8>, u64, u64, u64, u64)> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("tree.db");
    let mut tree = BPlusTree::new();
    for key in 1_000..1_700u32 {
        tree.insert(key, "same-value".to_string());
    }
    tree.store(&path)?;
    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    let root = query.header.root_page_id;
    let first = query.leftmost_leaf()?;
    let next_page_id = query.header.next_page_id;
    let second = query
        .with_page(first, |bytes, _| SlottedPage::open(bytes, first, next_page_id).map(|page| page.header().right))?;
    if second == 0 {
        return Err(io::Error::other("test tree did not split"));
    }
    let mut last = second;
    loop {
        let right = query
            .with_page(last, |bytes, _| SlottedPage::open(bytes, last, next_page_id).map(|page| page.header().right))?;
        if right == 0 {
            break;
        }
        last = right;
    }
    drop(query);
    Ok((fs::read(path)?, root, first, second, last))
}

#[test]
fn iterator_yields_second_leaf_corruption_once_then_fuses() -> io::Result<()> {
    let (mut database, _, _, second, _) = stored_tree_fixture()?;
    let byte = page_range(second)?.start + 100;
    *database.get_mut(byte).ok_or_else(|| io::Error::other("test corruption byte missing"))? ^= 1;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("iterator-corrupt.db");
    fs::write(&path, database)?;
    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    let mut iterator = query.iter();
    let mut yielded = 0usize;
    let mut saw_error = false;
    for item in iterator.by_ref() {
        match item {
            Ok(_) => yielded += 1,
            Err(err) => {
                assert_eq!(err.kind(), io::ErrorKind::InvalidData);
                saw_error = true;
                break;
            }
        }
    }
    if !saw_error {
        return Err(io::Error::other("corruption was hidden as end-of-stream"));
    }
    assert!(iterator.next().is_none());
    assert!(yielded > 0);
    Ok(())
}

#[test]
fn iterator_skips_corrupt_value_and_continues_with_next_cell() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("iterator-corrupt-value.db");
    let mut tree = BPlusTree::new();
    for key in 1..=3u32 {
        tree.insert(key, format!("value-{key}"));
    }
    tree.store(&path)?;

    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    let (page_id, range) = query.locate_cell(&2)?.ok_or_else(|| io::Error::other("test key missing"))?;
    let next_page_id = query.header.next_page_id;
    let stored_offset = query.with_page(page_id, |bytes, _| {
        let cell = LeafCellRef::decode(
            bytes.get(range).ok_or_else(|| io::Error::other("test cell range missing"))?,
            page_id,
            next_page_id,
        )?;
        let LeafValueRef::Inline { stored, .. } = cell.value else {
            return Err(io::Error::other("test value is not inline"));
        };
        Ok(stored.as_ptr() as usize - bytes.as_ptr() as usize)
    })?;
    drop(query);

    let mut database = fs::read(&path)?;
    let absolute = page_range(page_id)?.start + stored_offset;
    *database.get_mut(absolute).ok_or_else(|| io::Error::other("test value byte missing"))? = 0xc1;
    rewrite_page_checksum(&mut database, page_id)?;
    fs::write(&path, database)?;

    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    let mut iterator = query.iter();
    assert_eq!(iterator.next().transpose()?, Some((1, String::from("value-1"))));
    assert!(iterator.next().is_some_and(|entry| entry.is_err()));
    assert_eq!(iterator.next().transpose()?, Some((3, String::from("value-3"))));
    assert!(iterator.next().is_none());
    Ok(())
}

#[test]
fn iterator_rejects_leaf_cycle_before_yielding_duplicate_entries() -> io::Result<()> {
    let (mut database, _, first, second, _) = stored_tree_fixture()?;
    set_page_reference(&mut database, second, 16, first)?;
    set_page_reference(&mut database, first, 8, second)?;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("iterator-cycle.db");
    fs::write(&path, database)?;

    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    let mut iterator = query.iter();
    let mut seen = HashSet::new();
    let mut errors = 0usize;
    for item in iterator.by_ref() {
        match item {
            Ok((key, _)) => assert!(seen.insert(key), "iterator yielded key {key} twice"),
            Err(err) => {
                assert_eq!(err.kind(), io::ErrorKind::InvalidData);
                errors += 1;
            }
        }
    }
    assert_eq!(errors, 1);
    assert!(iterator.next().is_none());
    Ok(())
}

#[test]
fn full_verifier_rejects_tree_sibling_free_and_orphan_corruption() -> io::Result<()> {
    let (database, root, first, second, last) = stored_tree_fixture()?;

    let mut child_cycle = database.clone();
    set_page_reference(&mut child_cycle, root, 32, root)?;
    verify_rejects::<u32, String>(&child_cycle)?;

    let mut duplicate_child = database.clone();
    let root_start = page_range(root)?.start;
    let leftmost =
        u64::from_le_bytes(duplicate_child[root_start + 32..root_start + 40].try_into().map_err(io::Error::other)?);
    let first_cell =
        u16::from_le_bytes(duplicate_child[root_start + 40..root_start + 42].try_into().map_err(io::Error::other)?);
    let child_offset = root_start + usize::from(first_cell) + 4;
    duplicate_child[child_offset..child_offset + 8].copy_from_slice(&leftmost.to_le_bytes());
    rewrite_page_checksum(&mut duplicate_child, root)?;
    verify_rejects::<u32, String>(&duplicate_child)?;

    let mut wrong_type = database.clone();
    let header = DatabaseHeader::decode(&wrong_type[..PAGE_SIZE])?;
    let overflow = encode_overflow_page(leftmost, header.next_page_id, 0, b"wrong type")?;
    let range = page_range(leftmost)?;
    wrong_type[range].copy_from_slice(&overflow);
    verify_rejects::<u32, String>(&wrong_type)?;

    let mut asymmetric = database.clone();
    set_page_reference(&mut asymmetric, second, 8, 0)?;
    verify_rejects::<u32, String>(&asymmetric)?;

    let mut sibling_cycle = database.clone();
    set_page_reference(&mut sibling_cycle, last, 16, first)?;
    set_page_reference(&mut sibling_cycle, first, 8, last)?;
    verify_rejects::<u32, String>(&sibling_cycle)?;

    let mut inverted = database.clone();
    let first_start = page_range(first)?.start;
    let second_start = page_range(second)?.start;
    let first_count =
        u16::from_le_bytes(inverted[first_start + 2..first_start + 4].try_into().map_err(io::Error::other)?);
    let last_slot = first_start + PAGE_HEADER_LEN + (usize::from(first_count) - 1) * SLOT_LEN;
    let left_cell =
        usize::from(u16::from_le_bytes(inverted[last_slot..last_slot + 2].try_into().map_err(io::Error::other)?));
    let right_cell = usize::from(u16::from_le_bytes(
        inverted[second_start + PAGE_HEADER_LEN..second_start + PAGE_HEADER_LEN + 2]
            .try_into()
            .map_err(io::Error::other)?,
    ));
    for index in 0..3 {
        inverted.swap(first_start + left_cell + 24 + index, second_start + right_cell + 24 + index);
    }
    rewrite_page_checksum(&mut inverted, first)?;
    rewrite_page_checksum(&mut inverted, second)?;
    verify_rejects::<u32, String>(&inverted)?;

    let mut live_and_free = database.clone();
    let mut header = DatabaseHeader::decode(&live_and_free[..PAGE_SIZE])?;
    header.free_page_head = first;
    live_and_free[..PAGE_SIZE].copy_from_slice(&header.encode()?);
    verify_rejects::<u32, String>(&live_and_free)?;

    let mut orphan = database.clone();
    let mut header = DatabaseHeader::decode(&orphan[..PAGE_SIZE])?;
    let orphan_id = header.next_page_id;
    header.next_page_id += 1;
    orphan[..PAGE_SIZE].copy_from_slice(&header.encode()?);
    orphan.extend_from_slice(&encode_free_page(orphan_id, header.next_page_id, 0)?);
    verify_rejects::<u32, String>(&orphan)?;

    let mut duplicate_free = database;
    let mut header = DatabaseHeader::decode(&duplicate_free[..PAGE_SIZE])?;
    let first_free = header.next_page_id;
    let second_free = first_free + 1;
    header.next_page_id += 2;
    header.free_page_head = first_free;
    duplicate_free[..PAGE_SIZE].copy_from_slice(&header.encode()?);
    duplicate_free.extend_from_slice(&encode_free_page(first_free, header.next_page_id, second_free)?);
    duplicate_free.extend_from_slice(&encode_free_page(second_free, header.next_page_id, first_free)?);
    verify_rejects::<u32, String>(&duplicate_free)
}

pub(in crate::v3::tree::tests) fn overflow_heads(
    query: &mut BPlusTreeQuery<u32, Vec<u8>>,
) -> io::Result<(u64, u64, u64)> {
    let mut result = Vec::new();
    for key in [1, 2] {
        let leaf = query.locate_leaf(&key)?;
        let next_page_id = query.header.next_page_id;
        let (head, cell_offset) = query.with_page(leaf, |bytes, _| {
            let page = SlottedPage::open(bytes, leaf, next_page_id)?;
            let index = search_leaf(&page, &key)?.map_err(|_| io::Error::other("test key missing"))?;
            let cell = LeafCellRef::decode(page.cell(index)?, leaf, next_page_id)?;
            let LeafValueRef::Overflow { head, .. } = cell.value else {
                return Err(io::Error::other("test value is not overflow-backed"));
            };
            let slot = PAGE_HEADER_LEN + index * SLOT_LEN;
            let offset = u16::from_le_bytes(bytes[slot..slot + 2].try_into().map_err(io::Error::other)?);
            Ok((head, u64::from(offset)))
        })?;
        result.push((leaf, head, cell_offset));
    }
    let first = result.first().ok_or_else(|| io::Error::other("first test overflow missing"))?;
    let second = result.get(1).ok_or_else(|| io::Error::other("second test overflow missing"))?;
    Ok((first.1, second.0, second.2))
}

#[test]
fn full_verifier_rejects_overflow_cycle_and_shared_chain() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("overflow.db");
    let value = random_value();
    let mut tree = BPlusTree::new();
    tree.insert(1u32, value.clone());
    tree.insert(2u32, value);
    tree.store(&path)?;
    let mut query = BPlusTreeQuery::<u32, Vec<u8>>::try_new(&path)?;
    let (first_head, second_leaf, second_cell_offset) = overflow_heads(&mut query)?;
    let next_page_id = query.header.next_page_id;
    drop(query);
    let database = fs::read(&path)?;

    let mut cycle = database.clone();
    let mut last = first_head;
    loop {
        let start = page_range(last)?.start;
        let next = u64::from_le_bytes(cycle[start + 16..start + 24].try_into().map_err(io::Error::other)?);
        if next == 0 {
            break;
        }
        last = next;
    }
    set_page_reference(&mut cycle, last, 16, first_head)?;
    verify_rejects::<u32, Vec<u8>>(&cycle)?;

    let mut shared = database;
    let start = page_range(second_leaf)?.start + usize::try_from(second_cell_offset).map_err(io::Error::other)? + 12;
    shared[start..start + 8].copy_from_slice(&first_head.to_le_bytes());
    rewrite_page_checksum(&mut shared, second_leaf)?;
    let _ = next_page_id;
    verify_rejects::<u32, Vec<u8>>(&shared)
}

#[test]
fn full_verifier_rejects_oversized_overflow_descriptor_before_reserve() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("oversized-overflow.db");
    let value = random_value();
    let mut tree = BPlusTree::new();
    tree.insert(1u32, value.clone());
    tree.insert(2u32, value);
    tree.store(&path)?;
    let mut query = BPlusTreeQuery::<u32, Vec<u8>>::try_new(&path)?;
    let (_, second_leaf, second_cell_offset) = overflow_heads(&mut query)?;
    drop(query);

    let mut database = fs::read(&path)?;
    let descriptor = page_range(second_leaf)?.start + usize::try_from(second_cell_offset).map_err(io::Error::other)?;
    database[descriptor + 4..descriptor + 8].copy_from_slice(&u32::MAX.to_le_bytes());
    database[descriptor + 8..descriptor + 12].copy_from_slice(&u32::MAX.to_le_bytes());
    rewrite_page_checksum(&mut database, second_leaf)?;
    fs::write(&path, database)?;

    let mut query = BPlusTreeQuery::<u32, Vec<u8>>::try_new(&path)?;
    let error = verify_full(&mut query).err().ok_or_else(|| io::Error::other("oversized value accepted"))?;
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("overflow value exceeds allocation limit"));
    Ok(())
}

#[test]
fn full_verifier_rejects_uncompressed_overflow_length_mismatch() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("overflow-length.db");
    let mut tree = BPlusTree::new();
    tree.insert(1u32, random_value());
    tree.store(&path)?;
    let mut query = BPlusTreeQuery::<u32, Vec<u8>>::try_new(&path)?;
    let leaf = query.locate_leaf(&1)?;
    let next_page_id = query.header.next_page_id;
    let cell_offset = query.with_page(leaf, |bytes, _| {
        let page = SlottedPage::open(bytes, leaf, next_page_id)?;
        let index = search_leaf(&page, &1)?.map_err(|_| io::Error::other("test key missing"))?;
        let slot = PAGE_HEADER_LEN
            .checked_add(index.checked_mul(SLOT_LEN).ok_or_else(|| io::Error::other("test slot overflow"))?)
            .ok_or_else(|| io::Error::other("test slot overflow"))?;
        Ok(u16::from_le_bytes(bytes[slot..slot + 2].try_into().map_err(io::Error::other)?))
    })?;
    drop(query);

    let mut database = fs::read(&path)?;
    let descriptor = page_range(leaf)?.start + usize::from(cell_offset);
    database[descriptor + 4..descriptor + 8].copy_from_slice(&1u32.to_le_bytes());
    rewrite_page_checksum(&mut database, leaf)?;
    fs::write(&path, database)?;
    let mut query = BPlusTreeQuery::<u32, Vec<u8>>::try_new(&path)?;
    invalid_data(verify_full(&mut query))
}

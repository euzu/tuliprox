use super::{
    database_header, publish_database, random_value, verify_full, wal_path, wal_temporary_path, BPlusTree,
    BPlusTreeQuery, BPlusTreeSerialWriter, BPlusTreeUpdate, FlushPolicy, LeafCellRef, LeafValueRef,
};
use crate::{
    common::BPlusTreeError,
    v3::{
        format::{encode_overflow_leaf_cell, PAGE_HEADER_LEN, PAGE_SIZE},
        page::SlottedPage,
    },
};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[test]
fn smaller_inline_update_compacts_without_appending_pages() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("smaller-inline.db");
    let mut tree = BPlusTree::new();
    tree.insert(7u32, String::from("a much longer inline value"));
    tree.store(&path)?;
    let before = database_header(&path)?;
    let before_len = fs::metadata(&path)?.len();

    let mut updater = BPlusTreeUpdate::<u32, String>::try_new(&path)?;
    updater.update(&7, String::from("x")).map_err(BPlusTreeError::to_io)?;

    let after = database_header(&path)?;
    assert_eq!(after.generation, before.generation + 1);
    assert_eq!(after.root_page_id, before.root_page_id);
    assert_eq!(fs::metadata(&path)?.len(), before_len);
    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    assert_eq!(query.query(&7).map_err(BPlusTreeError::to_io)?, Some(String::from("x")));
    let _ = verify_full(&mut query)?;
    Ok(())
}

#[test]
fn growing_inline_value_uses_page_local_compaction_before_split() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("growing-inline.db");
    let mut tree = BPlusTree::new();
    for key in 0..8u32 {
        tree.insert(key, vec![u8::try_from(key).map_err(io::Error::other)?; 8]);
    }
    tree.store(&path)?;
    let before = database_header(&path)?;
    let before_len = fs::metadata(&path)?.len();
    let grown = vec![0x5a; 200];

    let mut updater = BPlusTreeUpdate::<u32, Vec<u8>>::try_new(&path)?;
    updater.update(&4, grown.clone()).map_err(BPlusTreeError::to_io)?;

    let after = database_header(&path)?;
    assert_eq!(after.generation, before.generation + 1);
    assert_eq!(after.root_page_id, before.root_page_id);
    assert_eq!(fs::metadata(&path)?.len(), before_len);
    let mut query = BPlusTreeQuery::<u32, Vec<u8>>::try_new(&path)?;
    assert_eq!(query.query(&4).map_err(BPlusTreeError::to_io)?, Some(grown));
    let _ = verify_full(&mut query)?;
    Ok(())
}

#[test]
fn medium_stored_value_remains_inline() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("medium-inline.db");
    let value = random_value().get(..300).ok_or_else(|| io::Error::other("random test value is too short"))?.to_vec();
    let mut tree = BPlusTree::new();
    tree.insert(7u32, value.clone());

    let report = tree.store_verified(&path)?;

    assert_eq!(report.overflow_pages, 0);
    let mut query = BPlusTreeQuery::<u32, Vec<u8>>::try_new(&path)?;
    assert_eq!(query.query(&7).map_err(BPlusTreeError::to_io)?, Some(value));
    Ok(())
}

/// The Xtream cluster import commits after every batch so the transaction's
/// dirty-page map stays bounded. That reopens the write transaction mid-import,
/// so every batch must survive and stay readable — including values large enough
/// to spill into overflow chains, which is what dominated the dirty-page map.
#[test]
fn committing_between_batches_keeps_every_entry_readable() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("batched-commits.db");
    BPlusTree::<u32, String>::new().store(&path)?;

    let mut updater = BPlusTreeUpdate::<u32, String>::try_new(&path)?;
    updater.set_flush_policy(FlushPolicy::Batch);
    for batch in 0..4u32 {
        let keys: Vec<u32> = (0..8).map(|index| batch * 8 + index).collect();
        let values: Vec<String> = keys.iter().map(|key| format!("{key}").repeat(600)).collect();
        let items = keys.iter().zip(&values).collect::<Vec<_>>();
        let prepared = BPlusTreeUpdate::<u32, String>::prepare_upsert_batch(&items)?;
        updater.upsert_batch_encoded(prepared)?;
        updater.commit()?;
        assert!(updater.active.is_none(), "commit must release the transaction after batch {batch}");
    }

    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    for key in 0..32u32 {
        assert_eq!(
            query.query(&key).map_err(BPlusTreeError::to_io)?,
            Some(format!("{key}").repeat(600)),
            "key {key} is missing after a mid-import commit"
        );
    }
    assert!(!wal_path(&path).try_exists()?);
    Ok(())
}

/// Throughput benchmark for `BPlusTree::store` across three workload sizes.
#[test]
#[ignore = "benchmark; run explicitly with --ignored --nocapture"]
fn bench_store_throughput() -> io::Result<()> {
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
        let start = std::time::Instant::now();
        tree.store(&path)?;
        let elapsed = start.elapsed();
        println!(
            "BENCH {label:6} entries={count:6} bytes={size:5} -> {:>8.1} ms  file={} MiB",
            elapsed.as_secs_f64() * 1000.0,
            fs::metadata(&path)?.len() / (1024 * 1024)
        );
    }
    Ok(())
}

#[test]
fn streamed_store_writes_every_page_despite_out_of_order_overflow_chains() -> io::Result<()> {
    // The streaming builder hands out a leaf's page id *before* the overflow chain it
    // points at, but writes the leaf *after* those pages — so page ids do not reach the
    // file in ascending order. Worse, this nests: leaf A is reserved, its chains are
    // written, A is written, then leaf B repeats the whole dance at higher ids.
    //
    // The entry count is sized to produce many leaves, not one: overflow leaf cells are
    // tiny (key plus pointers, ~34 bytes), so a few hundred entries would all land in a
    // single leaf and never exercise the nesting. Values are incompressible on purpose —
    // a repeated string would compress back under `MAX_INLINE_STORED_VALUE` and skip
    // overflow entirely. Both properties are asserted below rather than assumed.
    //
    // The file-length check is the real guard: writing pages sequentially instead of
    // positionally would leave the file short or the ids scrambled.
    // Deterministic LCG — compresses badly, so values are forced into overflow chains.
    let noise = |seed: u32, len: usize| -> Vec<u8> {
        let mut state = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                u8::try_from(state >> 24).unwrap_or(0)
            })
            .collect()
    };

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("streamed.db");
    let mut tree = BPlusTree::<u32, Vec<u8>>::new();
    for key in 0..1_200u32 {
        // Spans one, two and three overflow pages (OVERFLOW_PAYLOAD_LEN = 4056).
        tree.insert(key, noise(key, 3000 + (key as usize % 3) * 4000));
    }
    let report = tree.store_verified(&path)?;
    assert!(
        report.overflow_pages > 2_000,
        "test must force multi-page overflow chains, got {} overflow pages",
        report.overflow_pages
    );
    assert!(
        report.tree_pages > 8,
        "test must produce many leaves so the reserve-then-write dance nests, got {} tree pages",
        report.tree_pages
    );

    let header = BPlusTreeQuery::<u32, Vec<u8>>::try_new(&path)?.header;
    assert_eq!(
        fs::metadata(&path)?.len(),
        header.next_page_id * PAGE_SIZE as u64,
        "file length must match the allocated page count exactly"
    );

    let mut query = BPlusTreeQuery::<u32, Vec<u8>>::try_new(&path)?;
    for key in 0..1_200u32 {
        assert_eq!(
            query.query(&key).map_err(BPlusTreeError::to_io)?,
            Some(noise(key, 3000 + (key as usize % 3) * 4000)),
            "key {key} did not survive the streamed store"
        );
    }
    Ok(())
}

/// Compaction is NOT optional cleanup on an insert-built tree: leaf splits leave
/// pages roughly half full, and `build_pages` repacks them to near-capacity.
///
/// Note `free_page_head` stays 0 throughout — inserts never free a page — so it is
/// NOT a usable "nothing to compact" signal. Guarding compaction on an empty free
/// list would skip exactly the case that gains the most.
#[test]
fn compaction_repacks_an_insert_built_tree_despite_an_empty_free_list() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("repack.db");
    BPlusTree::<u32, String>::new().store(&path)?;

    let mut updater = BPlusTreeUpdate::<u32, String>::try_new(&path)?;
    updater.set_flush_policy(FlushPolicy::Batch);
    for batch in 0..8u32 {
        let keys: Vec<u32> = (0..250).map(|index| batch * 250 + index).collect();
        let values: Vec<String> = keys.iter().map(|key| format!("{key:07}").repeat(90)).collect();
        let items = keys.iter().zip(&values).collect::<Vec<_>>();
        updater.upsert_batch_encoded(BPlusTreeUpdate::<u32, String>::prepare_upsert_batch(&items)?)?;
        updater.commit()?;
    }

    let before = BPlusTreeQuery::<u32, String>::try_new(&path)?.header;
    assert_eq!(before.free_page_head, 0, "inserts must not free pages");

    updater.compact()?;

    let after = BPlusTreeQuery::<u32, String>::try_new(&path)?.header;
    assert!(
        after.next_page_id * 3 < before.next_page_id * 2,
        "compaction must reclaim over a third of the pages, got {} -> {}",
        before.next_page_id,
        after.next_page_id
    );

    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    for key in 0..2000u32 {
        assert_eq!(
            query.query(&key).map_err(BPlusTreeError::to_io)?,
            Some(format!("{key:07}").repeat(90)),
            "key {key} did not survive compaction"
        );
    }
    Ok(())
}

#[test]
fn immediate_commit_clears_wal_then_invalidates_sorted_index() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("immediate.db");
    let index_path = crate::common::get_file_path_for_db_index(&path);
    let mut tree = BPlusTree::new();
    tree.insert(1u32, String::from("old"));
    tree.store(&path)?;
    fs::write(&index_path, b"derived")?;

    let mut updater = BPlusTreeUpdate::<u32, String>::try_new(&path)?;
    updater.upsert(&1, &String::from("new"))?;

    assert!(!wal_path(&path).try_exists()?);
    assert!(!wal_temporary_path(&path).try_exists()?);
    assert!(!index_path.try_exists()?);
    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    assert_eq!(query.query(&1).map_err(BPlusTreeError::to_io)?, Some(String::from("new")));
    Ok(())
}

#[test]
fn missing_delete_and_empty_commit_are_true_noops() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("no-op.db");
    let index_path = crate::common::get_file_path_for_db_index(&path);
    let mut tree = BPlusTree::new();
    tree.insert(1u32, String::from("one"));
    tree.store(&path)?;
    fs::write(&index_path, b"still-valid")?;
    let before = fs::read(&path)?;

    let mut updater = BPlusTreeUpdate::<u32, String>::try_new(&path)?;
    assert!(!updater.delete(&2)?);
    updater.commit()?;

    assert_eq!(fs::read(&path)?, before);
    assert!(index_path.try_exists()?);
    assert!(!wal_path(&path).try_exists()?);
    Ok(())
}

#[test]
fn serial_writer_commits_a_batch_and_shuts_down_cleanly() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("serial-writer.db");
    BPlusTree::<u32, String>::new().store(&path)?;

    let writer = BPlusTreeSerialWriter::new(&path, FlushPolicy::Batch)?;
    let one = String::from("one");
    let two = String::from("two");
    assert_ne!(writer.upsert(&[(&1, &one), (&2, &two)])?, 0);
    writer.shutdown()?;

    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    assert_eq!(query.iter().collect::<io::Result<Vec<_>>>()?, vec![(1, one), (2, two)]);
    Ok(())
}

#[test]
fn store_with_index_publishes_an_identity_bound_sorted_snapshot() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("indexed.db");
    let index_path = crate::common::get_file_path_for_db_index(&path);
    let mut tree = BPlusTree::new();
    tree.insert(1u32, String::from("ccc"));
    tree.insert(2u32, String::from("a"));
    tree.insert(3u32, String::from("bb"));

    assert_ne!(tree.store_with_index(&path, String::len)?, 0);

    let query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    let mut sorted = crate::sorted_index::v4::OwnedIterator::<u32, String, usize>::open(query, &index_path)?;
    assert_eq!(
        sorted.by_ref().collect::<io::Result<Vec<_>>>()?,
        vec![(2, String::from("a")), (3, String::from("bb")), (1, String::from("ccc"))]
    );
    assert_eq!(sorted.remaining(), 0);
    assert!(!fs::read_dir(dir.path())?
        .any(|entry| entry.is_ok_and(|entry| entry.file_name().to_string_lossy().ends_with(".v3.tmp"))));
    Ok(())
}

#[test]
fn compact_rebuilds_only_live_data_with_a_fresh_identity() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("compact.db");
    let index_path = crate::common::get_file_path_for_db_index(&path);
    let mut tree = BPlusTree::new();
    for key in 0..100u32 {
        tree.insert(
            key,
            if key % 10 == 0 { random_value() } else { vec![u8::try_from(key).map_err(io::Error::other)?; 32] },
        );
    }
    tree.store(&path)?;
    let original_id = database_header(&path)?.database_id;

    let mut updater = BPlusTreeUpdate::<u32, Vec<u8>>::try_new(&path)?;
    updater.set_flush_policy(FlushPolicy::Batch);
    for key in (0..100u32).step_by(2) {
        assert!(updater.delete(&key)?);
    }
    updater.commit()?;
    let length_before = fs::metadata(&path)?.len();
    fs::write(&index_path, b"stale")?;

    updater.compact()?;

    let header = database_header(&path)?;
    assert_ne!(header.database_id, original_id);
    assert_eq!(header.generation, 1);
    assert!(fs::metadata(&path)?.len() <= length_before);
    assert!(!index_path.try_exists()?);
    let mut query = BPlusTreeQuery::<u32, Vec<u8>>::try_new(&path)?;
    let entries = query.iter().collect::<io::Result<Vec<_>>>()?;
    assert_eq!(entries.len(), 50);
    assert!(entries.iter().all(|(key, _)| key % 2 == 1));
    let report = verify_full(&mut query)?;
    assert_eq!(report.live_entries, 50);
    assert_eq!(report.free_pages, 0);
    Ok(())
}

#[test]
fn compact_read_failure_preserves_database_and_index() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("compact-corrupt.db");
    let index_path = crate::common::get_file_path_for_db_index(&path);
    let mut tree = BPlusTree::new();
    tree.insert(1u32, String::from("one"));
    tree.store(&path)?;
    let updater = BPlusTreeUpdate::<u32, String>::try_new(&path)?;

    let header = database_header(&path)?;
    let mut corrupted = fs::read(&path)?;
    let offset = usize::try_from(header.root_page_id)
        .map_err(io::Error::other)?
        .checked_mul(PAGE_SIZE)
        .and_then(|start| start.checked_add(PAGE_HEADER_LEN))
        .ok_or_else(|| io::Error::other("corruption offset overflow"))?;
    *corrupted.get_mut(offset).ok_or_else(|| io::Error::other("corruption offset outside database"))? ^= 0xff;
    fs::write(&path, &corrupted)?;
    fs::write(&index_path, b"still-valid")?;

    let mut updater = updater;
    assert!(updater.compact().is_err());
    assert_eq!(fs::read(&path)?, corrupted);
    assert_eq!(fs::read(&index_path)?, b"still-valid");
    Ok(())
}

#[test]
fn batch_overlay_is_visible_to_updater_and_commits_once() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("batch.db");
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

    let mut updater = BPlusTreeUpdate::<u32, String>::try_new(&path)?;
    updater.set_flush_policy(FlushPolicy::Batch);
    updater.upsert(&17, &value)?;
    updater.upsert(&18, &value)?;
    assert_eq!(updater.query(&18).map_err(BPlusTreeError::to_io)?, Some(value.clone()));
    assert_eq!(database_header(&path)?.generation, before.generation);

    updater.commit()?;

    let after = database_header(&path)?;
    assert_eq!(after.generation, before.generation + 1);
    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    assert_eq!(query.query(&17).map_err(BPlusTreeError::to_io)?, Some(value.clone()));
    assert_eq!(query.query(&18).map_err(BPlusTreeError::to_io)?, Some(value));
    assert_eq!(verify_full(&mut query)?.live_entries, 19);
    Ok(())
}

#[test]
fn final_overlay_validation_rejects_unordered_leaf_keys_before_wal() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("unordered-overlay.db");
    let mut tree = BPlusTree::new();
    tree.insert(1u32, String::from("one"));
    tree.insert(2u32, String::from("two"));
    tree.store(&path)?;
    let before = fs::read(&path)?;

    let mut updater = BPlusTreeUpdate::<u32, String>::try_new(&path)?;
    updater.set_flush_policy(FlushPolicy::Batch);
    updater.upsert(&1, &String::from("staged"))?;
    let active = updater.active.as_mut().ok_or_else(|| io::Error::other("test transaction is missing"))?;
    let leaf_id = active.transaction.next_header.root_page_id;
    let snapshot = active.transaction.page_copy(active.base.as_slice(), leaf_id)?;
    let page = SlottedPage::open(snapshot.as_slice(), leaf_id, active.transaction.next_header.next_page_id)?;
    let mut cells = page.cells().map(|cell| cell.map(<[u8]>::to_vec)).collect::<io::Result<Vec<_>>>()?;
    let duplicate = cells.first().cloned().ok_or_else(|| io::Error::other("test leaf has no cells"))?;
    *cells.get_mut(1).ok_or_else(|| io::Error::other("test leaf lacks a second cell"))? = duplicate;
    let next_page_id = active.transaction.next_header.next_page_id;
    let dirty = active.transaction.page_mut(active.base.as_slice(), leaf_id)?;
    SlottedPage::open(dirty.as_mut_slice(), leaf_id, next_page_id)?.rebuild_ordered(cells.iter().map(Vec::as_slice))?;

    let error = updater.commit().err().ok_or_else(|| io::Error::other("unordered overlay was committed"))?;
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(fs::read(&path)?, before);
    assert!(!wal_path(&path).try_exists()?);
    Ok(())
}

#[test]
fn final_overlay_validation_rejects_shared_overflow_before_wal() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("shared-overflow-overlay.db");
    let first_value = random_value();
    let mut second_value = first_value.clone();
    second_value.reverse();
    let mut tree = BPlusTree::new();
    tree.insert(1u32, first_value);
    tree.insert(2u32, second_value);
    tree.store(&path)?;
    let before = fs::read(&path)?;

    let mut updater = BPlusTreeUpdate::<u32, Vec<u8>>::try_new(&path)?;
    updater.set_flush_policy(FlushPolicy::Batch);
    updater.ensure_transaction()?;
    let active = updater.active.as_mut().ok_or_else(|| io::Error::other("test transaction is missing"))?;
    let leaf_id = active.transaction.next_header.root_page_id;
    let next_page_id = active.transaction.next_header.next_page_id;
    let snapshot = active.transaction.page_copy(active.base.as_slice(), leaf_id)?;
    let page = SlottedPage::open(snapshot.as_slice(), leaf_id, next_page_id)?;
    let first = LeafCellRef::decode(page.cell(0)?, leaf_id, next_page_id)?;
    let second = LeafCellRef::decode(page.cell(1)?, leaf_id, next_page_id)?;
    let LeafValueRef::Overflow { compression, logical_len, stored_len, head, crc32 } = first.value else {
        return Err(io::Error::other("first test value is not overflow-backed"));
    };
    let mut replacement = Vec::new();
    encode_overflow_leaf_cell(
        second.key_bytes,
        logical_len,
        compression,
        stored_len,
        head,
        crc32,
        leaf_id,
        next_page_id,
        &mut replacement,
    )?;
    let dirty = active.transaction.page_mut(active.base.as_slice(), leaf_id)?;
    SlottedPage::open(dirty.as_mut_slice(), leaf_id, next_page_id)?.replace_same_len(1, &replacement)?;

    let error = updater.commit().err().ok_or_else(|| io::Error::other("shared overflow was committed"))?;
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(fs::read(&path)?, before);
    assert!(!wal_path(&path).try_exists()?);
    Ok(())
}

pub(in crate::v3::tree::tests) fn pending_path(database: &Path, suffix: &str) -> PathBuf {
    let mut name = database.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

#[test]
fn query_removes_abandoned_wal_temp_and_rejects_corrupt_active_wal() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("pending-recovery.db");
    let mut tree = BPlusTree::new();
    tree.insert(1u32, String::from("original"));
    tree.store(&path)?;

    let temporary = pending_path(&path, ".wal.tmp");
    fs::write(&temporary, b"not activated")?;
    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    assert_eq!(query.query(&1).map_err(BPlusTreeError::to_io)?, Some(String::from("original")));
    assert!(!temporary.try_exists()?);
    drop(query);

    let active = pending_path(&path, ".wal");
    fs::write(&active, b"corrupt active WAL")?;
    let active_before = fs::read(&active)?;
    let error = BPlusTreeQuery::<u32, String>::try_new(&path)
        .err()
        .ok_or_else(|| io::Error::other("query accepted corrupt active WAL"))?;
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(fs::read(active)?, active_before);
    Ok(())
}

#[test]
fn replacement_store_removes_wal_temp_and_preserves_corrupt_active_wal() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("pending-store.db");
    let mut original = BPlusTree::new();
    original.insert(1u32, String::from("original"));
    original.store(&path)?;
    let database_before = fs::read(&path)?;

    let temporary = pending_path(&path, ".wal.tmp");
    fs::write(&temporary, b"not activated")?;
    let mut replacement = BPlusTree::new();
    replacement.insert(2u32, String::from("replacement"));
    replacement.store(&path)?;
    assert!(!temporary.try_exists()?);

    let published_before = fs::read(&path)?;
    let active = pending_path(&path, ".wal");
    let active_before = b"corrupt active WAL".to_vec();
    fs::write(&active, &active_before)?;
    let mut rejected = BPlusTree::new();
    rejected.insert(3u32, String::from("rejected"));
    let error = rejected.store(&path).err().ok_or_else(|| io::Error::other("store accepted corrupt WAL"))?;
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_ne!(published_before, database_before);
    assert_eq!(fs::read(&path)?, published_before);
    assert_eq!(fs::read(active)?, active_before);
    Ok(())
}

#[test]
fn clean_loaded_store_recovers_temp_but_rejects_corrupt_active_wal() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("pending-clean-store.db");
    let mut original = BPlusTree::new();
    original.insert(1u32, String::from("original"));
    original.store(&path)?;
    let database_before = fs::read(&path)?;
    let mut loaded = BPlusTree::<u32, String>::load(&path)?;

    let temporary = pending_path(&path, ".wal.tmp");
    fs::write(&temporary, b"not activated")?;
    assert_eq!(loaded.store(&path)?, 0);
    assert!(!temporary.try_exists()?);
    assert_eq!(fs::read(&path)?, database_before);

    let active = pending_path(&path, ".wal");
    let active_before = b"corrupt active WAL".to_vec();
    fs::write(&active, &active_before)?;
    let error = loaded.store(&path).err().ok_or_else(|| io::Error::other("clean store accepted corrupt WAL"))?;
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(fs::read(&path)?, database_before);
    assert_eq!(fs::read(active)?, active_before);
    Ok(())
}

#[test]
fn publish_reports_post_commit_directory_sync_failure() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let destination = dir.path().join("database.db");
    let temporary = dir.path().join("database.tx.v3.tmp");
    let mut old = BPlusTree::new();
    old.insert(1u32, vec![1]);
    old.store(&destination)?;
    let mut new = BPlusTree::new();
    new.insert(2u32, vec![2]);
    new.store(&temporary)?;

    let error =
        publish_database(&temporary, &destination, |_| Err(io::Error::other("injected directory sync failure")))
            .err()
            .ok_or_else(|| io::Error::other("post-commit sync failure was hidden"))?;
    assert!(error.database_was_published());
    let error = io::Error::from(error);
    assert!(error.to_string().contains("database published but directory sync failed; durability unknown"));
    assert!(!temporary.exists());
    let mut query = BPlusTreeQuery::<u32, Vec<u8>>::try_new(&destination)?;
    assert_eq!(query.query(&1).map_err(BPlusTreeError::to_io)?, None);
    assert_eq!(query.query(&2).map_err(BPlusTreeError::to_io)?, Some(vec![2]));
    Ok(())
}

#[test]
fn store_sync_failure_after_rename_invalidates_previous_sorted_index() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let destination = dir.path().join("database.db");
    let index = crate::common::get_file_path_for_db_index(&destination);
    let mut old = BPlusTree::new();
    old.insert(1u32, String::from("old"));
    let _ = old.store_with_index(&destination, String::clone)?;
    assert!(index.is_file());

    let mut replacement = BPlusTree::new();
    replacement.insert(2u32, String::from("new"));
    let error = replacement
        .store_exclusive_with_directory_sync(&destination, |_| Err(io::Error::other("injected directory sync failure")))
        .err()
        .ok_or_else(|| io::Error::other("post-rename sync failure was hidden"))?;

    assert!(error.to_string().contains("publication durability remains unknown"));
    assert!(!index.exists());
    assert!(replacement.dirty);
    let mut query = BPlusTreeQuery::<u32, String>::try_new(&destination)?;
    assert_eq!(query.query(&1).map_err(BPlusTreeError::to_io)?, None);
    assert_eq!(query.query(&2).map_err(BPlusTreeError::to_io)?, Some(String::from("new")));
    Ok(())
}

#[test]
fn store_reports_sync_and_index_invalidation_failures_together() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let destination = dir.path().join("database.db");
    let index = crate::common::get_file_path_for_db_index(&destination);
    let mut old = BPlusTree::new();
    old.insert(1u32, String::from("old"));
    let _ = old.store(&destination)?;
    fs::create_dir(&index)?;

    let mut replacement = BPlusTree::new();
    replacement.insert(2u32, String::from("new"));
    let error = replacement
        .store_exclusive_with_directory_sync(&destination, |_| Err(io::Error::other("injected directory sync failure")))
        .err()
        .ok_or_else(|| io::Error::other("publish and index invalidation failures were hidden"))?;
    let message = error.to_string();

    assert!(message.contains("unknown directory durability"));
    assert!(message.contains("previous sorted index could not be invalidated"));
    assert!(index.is_dir());
    assert!(replacement.dirty);
    let mut query = BPlusTreeQuery::<u32, String>::try_new(&destination)?;
    assert_eq!(query.query(&1).map_err(BPlusTreeError::to_io)?, None);
    assert_eq!(query.query(&2).map_err(BPlusTreeError::to_io)?, Some(String::from("new")));
    Ok(())
}

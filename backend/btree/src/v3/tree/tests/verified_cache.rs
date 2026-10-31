use super::super::{
    is_verified_with_cache, leave_committed_test_wal, mark_verified_with_cache, reset_verify_full_count,
    verify_full_count, BPlusTree, BPlusTreeUpdate, DatabaseHeader, VerifiedIdentity, PAGE_SIZE,
};
use crate::common::{read_exact_at_offset, write_all_at_offset};
use std::{
    fs::{self, OpenOptions},
    io,
};

#[test]
fn second_try_new_on_unchanged_file_skips_verify_full() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("unchanged.db");
    let mut tree = BPlusTree::new();
    tree.insert(1u32, 100u64);
    tree.store(&path)?;

    reset_verify_full_count();
    let updater1 = BPlusTreeUpdate::<u32, u64>::try_new(&path)?;
    assert_eq!(verify_full_count(), 0);
    drop(updater1);

    let updater2 = BPlusTreeUpdate::<u32, u64>::try_new(&path)?;
    assert_eq!(verify_full_count(), 0);
    drop(updater2);
    Ok(())
}

#[test]
fn uncached_file_runs_verify_full_once_and_subsequent_skips() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let seed_path = dir.path().join("seed.db");
    let target_path = dir.path().join("target_uncached.db");

    let mut tree = BPlusTree::new();
    tree.insert(1u32, 100u64);
    tree.store(&seed_path)?;

    // Copying the file produces an uncached database at target_path
    fs::copy(&seed_path, &target_path)?;

    reset_verify_full_count();

    let updater1 = BPlusTreeUpdate::<u32, u64>::try_new(&target_path)?;
    assert_eq!(verify_full_count(), 1);
    drop(updater1);

    let updater2 = BPlusTreeUpdate::<u32, u64>::try_new(&target_path)?;
    assert_eq!(verify_full_count(), 1);
    drop(updater2);
    Ok(())
}

#[test]
fn new_instance_after_commit_skips_verify_full() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("after_commit.db");
    let mut tree = BPlusTree::new();
    tree.insert(1u32, 100u64);
    tree.store(&path)?;

    let mut updater1 = BPlusTreeUpdate::<u32, u64>::try_new(&path)?;
    updater1.upsert(&2, &200)?;
    updater1.commit()?;
    drop(updater1);

    reset_verify_full_count();
    let updater2 = BPlusTreeUpdate::<u32, u64>::try_new(&path)?;
    assert_eq!(verify_full_count(), 0);
    drop(updater2);
    Ok(())
}

#[test]
fn replacement_by_store_runs_verify_full_once() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("replacement_store.db");
    let mut tree = BPlusTree::new();
    tree.insert(1u32, 100u64);
    tree.store(&path)?;

    let updater1 = BPlusTreeUpdate::<u32, u64>::try_new(&path)?;
    drop(updater1);

    reset_verify_full_count();
    let mut replacement = BPlusTree::new();
    replacement.insert(2u32, 200u64);
    replacement.store(&path)?;
    assert_eq!(verify_full_count(), 1);

    let updater2 = BPlusTreeUpdate::<u32, u64>::try_new(&path)?;
    assert_eq!(verify_full_count(), 1);
    drop(updater2);
    Ok(())
}

#[test]
fn external_write_changing_generation_triggers_verify_full() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("external_write.db");
    let mut tree = BPlusTree::new();
    tree.insert(1u32, 100u64);
    tree.store(&path)?;

    let updater1 = BPlusTreeUpdate::<u32, u64>::try_new(&path)?;
    drop(updater1);

    let file = OpenOptions::new().read(true).write(true).open(&path)?;
    let mut header_bytes = [0u8; PAGE_SIZE];
    read_exact_at_offset(&file, &mut header_bytes, 0)?;
    let mut header = DatabaseHeader::decode(&header_bytes)?;
    header.generation = header.generation.checked_add(1).ok_or_else(|| io::Error::other("generation overflow"))?;
    let encoded = header.encode()?;
    write_all_at_offset(&file, &encoded, 0)?;
    drop(file);

    reset_verify_full_count();
    let updater2 = BPlusTreeUpdate::<u32, u64>::try_new(&path)?;
    assert_eq!(verify_full_count(), 1);
    drop(updater2);

    let updater3 = BPlusTreeUpdate::<u32, u64>::try_new(&path)?;
    assert_eq!(verify_full_count(), 1);
    drop(updater3);
    Ok(())
}

#[test]
fn wal_recovery_advancing_generation_triggers_verify_full() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("wal_recovery.db");
    let mut tree = BPlusTree::new();
    tree.insert(1u32, 100u64);
    tree.store(&path)?;

    let updater1 = BPlusTreeUpdate::<u32, u64>::try_new(&path)?;
    drop(updater1);

    leave_committed_test_wal(&path)?;

    reset_verify_full_count();
    let updater2 = BPlusTreeUpdate::<u32, u64>::try_new(&path)?;
    assert_eq!(verify_full_count(), 1);
    drop(updater2);

    let updater3 = BPlusTreeUpdate::<u32, u64>::try_new(&path)?;
    assert_eq!(verify_full_count(), 1);
    drop(updater3);
    Ok(())
}

#[test]
fn corrupt_database_with_unverified_identity_is_rejected() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let seed_path = dir.path().join("seed_corrupt.db");
    let corrupt_path = dir.path().join("target_corrupt.db");

    let mut tree = BPlusTree::new();
    tree.insert(1u32, 100u64);
    tree.insert(2u32, 200u64);
    tree.store(&seed_path)?;

    // Copying creates an uncached database at corrupt_path
    fs::copy(&seed_path, &corrupt_path)?;

    // Corrupt root page bytes (page 1)
    let file = OpenOptions::new().read(true).write(true).open(&corrupt_path)?;
    let garbage = [0xff; 128];
    write_all_at_offset(&file, &garbage, PAGE_SIZE as u64)?;
    drop(file);

    reset_verify_full_count();
    let result = BPlusTreeUpdate::<u32, u64>::try_new(&corrupt_path);
    assert!(result.is_err());
    assert_eq!(verify_full_count(), 1);
    Ok(())
}

#[test]
fn compact_marks_resulting_identity_and_subsequent_try_new_skips() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("compact.db");
    let mut tree = BPlusTree::new();
    tree.insert(1u32, 100u64);
    tree.store(&path)?;

    let mut updater = BPlusTreeUpdate::<u32, u64>::try_new(&path)?;
    updater.upsert(&2, &200)?;
    updater.compact()?;
    drop(updater);

    reset_verify_full_count();
    let updater2 = BPlusTreeUpdate::<u32, u64>::try_new(&path)?;
    assert_eq!(verify_full_count(), 0);
    drop(updater2);
    Ok(())
}

#[test]
fn ensure_transaction_skips_when_committed_by_another_instance() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("mismatch_ensure.db");
    let mut tree = BPlusTree::new();
    tree.insert(1u32, 100u64);
    tree.store(&path)?;

    let mut updater1 = BPlusTreeUpdate::<u32, u64>::try_new(&path)?;
    let mut updater2 = BPlusTreeUpdate::<u32, u64>::try_new(&path)?;

    updater1.upsert(&2, &200)?;
    updater1.commit()?;

    reset_verify_full_count();
    updater2.upsert(&3, &300)?;
    assert_eq!(verify_full_count(), 0);
    updater2.commit()?;
    Ok(())
}

#[test]
fn poisoned_mutex_treats_as_not_verified_and_never_panics() {
    use std::{collections::HashMap, sync::Mutex};

    let cache = Mutex::new(HashMap::new());
    let _ = std::panic::catch_unwind(|| {
        let _guard = cache.lock().unwrap();
        panic!("test panic to poison cache mutex");
    });
    assert!(cache.is_poisoned());

    let identity = VerifiedIdentity::new([0; 16], 1, 1);
    let path = std::path::Path::new("/test");

    // Poisoned mutex must be treated as not verified
    assert!(!is_verified_with_cache(&cache, path, identity));

    // Marking on poisoned mutex must never panic and do nothing
    mark_verified_with_cache(&cache, path, identity);
    assert!(!is_verified_with_cache(&cache, path, identity));
}

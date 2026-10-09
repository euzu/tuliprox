use super::{
    assert_writer_is_blocked, database_header, finish_writer, spawn_replacement_writer, try_exclusive_sidecar,
    verify_full, wait_for_exclusive_sidecar, wal_path, BPlusTree, BPlusTreeQuery, BPlusTreeUpdate,
    ConditionalSerialize, FlushPolicy,
};
use crate::common::BPlusTreeError;
use std::{fs, io};

#[test]
fn dropped_uncommitted_batch_discards_overlay_without_wal() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("dropped-batch.db");
    let mut tree = BPlusTree::new();
    tree.insert(1u32, String::from("old"));
    tree.store(&path)?;
    let before = fs::read(&path)?;

    let mut updater = BPlusTreeUpdate::<u32, String>::try_new(&path)?;
    updater.set_flush_policy(FlushPolicy::Batch);
    updater.upsert(&1, &String::from("uncommitted"))?;
    assert_eq!(updater.query(&1).map_err(BPlusTreeError::to_io)?, Some(String::from("uncommitted")));
    drop(updater);

    assert_eq!(fs::read(&path)?, before);
    assert!(!wal_path(&path).try_exists()?);
    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    assert_eq!(query.query(&1).map_err(BPlusTreeError::to_io)?, Some(String::from("old")));
    Ok(())
}

#[test]
fn direct_serialization_error_aborts_an_existing_batch() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("direct-serialization.db");
    let initial = ConditionalSerialize { value: String::from("initial"), fail: false };
    let mut tree = BPlusTree::new();
    tree.insert(0u32, initial);
    tree.store(&path)?;
    let before = fs::read(&path)?;
    let staged = ConditionalSerialize { value: String::from("staged"), fail: false };
    let failing = ConditionalSerialize { value: String::from("failing"), fail: true };

    let mut updater = BPlusTreeUpdate::<u32, ConditionalSerialize>::try_new(&path)?;
    updater.set_flush_policy(FlushPolicy::Batch);
    updater.upsert(&1, &staged)?;
    assert!(updater.upsert(&2, &failing).is_err());
    updater.commit()?;

    assert_eq!(fs::read(&path)?, before);
    assert!(!wal_path(&path).try_exists()?);
    Ok(())
}

#[test]
fn delete_key_serialization_error_aborts_an_existing_batch() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("delete-serialization.db");
    let initial = ConditionalSerialize { value: String::from("initial"), fail: false };
    let mut tree = BPlusTree::new();
    tree.insert(initial.clone(), String::from("old"));
    tree.store(&path)?;
    let before = fs::read(&path)?;
    let staged = ConditionalSerialize { value: String::from("staged"), fail: false };
    let failing = ConditionalSerialize { value: String::from("failing"), fail: true };

    let mut updater = BPlusTreeUpdate::<ConditionalSerialize, String>::try_new(&path)?;
    updater.set_flush_policy(FlushPolicy::Batch);
    updater.upsert(&staged, &String::from("new"))?;
    assert!(updater.delete(&failing).is_err());
    updater.commit()?;

    assert_eq!(fs::read(&path)?, before);
    assert!(!wal_path(&path).try_exists()?);
    Ok(())
}

#[test]
fn idle_updater_refreshes_after_full_replacement() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("replacement-refresh.db");
    let mut original = BPlusTree::new();
    original.insert(1u32, String::from("original"));
    original.store(&path)?;
    let mut updater = BPlusTreeUpdate::<u32, String>::try_new(&path)?;
    let original_id = updater.database_id;

    let mut replacement = BPlusTree::new();
    replacement.insert(2u32, String::from("replacement"));
    replacement.store(&path)?;
    let replacement_id = database_header(&path)?.database_id;
    assert_ne!(replacement_id, original_id);

    updater.upsert(&3, &String::from("updated"))?;

    assert_eq!(updater.database_id, replacement_id);
    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    assert_eq!(query.query(&1).map_err(BPlusTreeError::to_io)?, None);
    assert_eq!(query.query(&2).map_err(BPlusTreeError::to_io)?, Some(String::from("replacement")));
    assert_eq!(query.query(&3).map_err(BPlusTreeError::to_io)?, Some(String::from("updated")));
    let _ = verify_full(&mut query)?;
    Ok(())
}

#[test]
fn shared_queries_coexist_and_block_replacement_until_both_drop() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("shared-queries.db");
    let mut tree = BPlusTree::new();
    tree.insert(1u32, String::from("original"));
    tree.store(&path)?;

    let first = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    let second = first.try_clone()?;
    assert!(!try_exclusive_sidecar(&path)?);
    drop(first);
    assert!(!try_exclusive_sidecar(&path)?);
    let (receiver, handle) = spawn_replacement_writer(path.clone())?;
    assert_writer_is_blocked(&receiver)?;
    drop(second);
    finish_writer(&receiver, handle)?;
    assert!(wait_for_exclusive_sidecar(&path)?);
    Ok(())
}

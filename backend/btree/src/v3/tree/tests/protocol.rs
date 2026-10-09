use super::{database_header, BPlusTree, BPlusTreeMetadata, BPlusTreeQuery, BPlusTreeUpdate};
use crate::common::BPlusTreeError;
use std::{fs, io, ops::Bound};

#[test]
fn metadata_update_is_a_single_header_transaction() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("metadata.db");
    let mut tree = BPlusTree::<u32, String>::new();
    tree.insert(1, String::from("one"));
    tree.store(&path)?;
    let before = database_header(&path)?;
    let before_len = fs::metadata(&path)?.len();

    let mut updater = BPlusTreeUpdate::<u32, String>::try_new(&path)?;
    updater.set_metadata(&BPlusTreeMetadata::TargetIdMapping(42))?;

    let after = database_header(&path)?;
    assert_eq!(after.generation, before.generation + 1);
    assert_eq!(after.metadata, BPlusTreeMetadata::TargetIdMapping(42));
    assert_eq!(fs::metadata(&path)?.len(), before_len);
    assert_eq!(updater.get_metadata()?, BPlusTreeMetadata::TargetIdMapping(42));
    Ok(())
}

#[test]
fn range_page_does_not_preallocate_the_requested_limit() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("range-limit.db");
    let mut tree = BPlusTree::new();
    tree.insert(1u32, String::from("one"));
    tree.insert(2u32, String::from("two"));
    tree.store(&path)?;

    let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
    let (entries, has_more) =
        query.range_page(Bound::Unbounded, Bound::Unbounded, 0, usize::MAX).map_err(BPlusTreeError::to_io)?;
    assert_eq!(entries, vec![(1, String::from("one")), (2, String::from("two"))]);
    assert!(!has_more);
    Ok(())
}

mod format;
pub mod migration;
mod page;
mod publish;
mod tree;
mod wal;

pub use format::{Locator, MAGIC, STORAGE_VERSION_V3 as STORAGE_VERSION};
pub use publish::{publish_staged_database, BPlusTreeStagingArtifacts};
#[allow(unused_imports)]
pub use tree::{
    BPlusTree, BPlusTreeDiskIterator, BPlusTreeDiskIteratorOwned, BPlusTreeQuery, BPlusTreeRangeIterator,
    BPlusTreeSerialWriter, BPlusTreeUpdate, FlushPolicy,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BPlusTreeMetadata {
    Empty,
    TargetIdMapping(u32),
    /// Binds a database to the recovery history that produced it.
    ///
    /// The engine never interprets these fields; it only stores them so a
    /// recovery wrapper can tell whether the database is behind, level with or
    /// ahead of its own journal.
    Recovery(RecoveryIdentity),
}

/// Application-neutral recovery bookkeeping carried in the database header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecoveryIdentity {
    pub database_id: [u8; 16],
    pub schema_fingerprint: [u8; 32],
    pub schema_version: u32,
    pub applied_revision: u64,
}

#[cfg(test)]
mod contract_tests {
    use super::{tree::verify_full, BPlusTree, BPlusTreeMetadata, BPlusTreeQuery};
    use crate::v2;
    use std::{io, ops::Bound};

    fn incompressible_value() -> Vec<u8> {
        let mut state = 0x1234_5678_9abc_def0u64;
        (0..12_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state.to_le_bytes()[0]
            })
            .collect()
    }

    macro_rules! storage_contract {
        ($name:ident, $tree:ty, $query:ty, $metadata:expr) => {
            #[test]
            fn $name() -> io::Result<()> {
                type Tree = $tree;
                type Query = $query;

                let dir = tempfile::tempdir()?;
                let empty_path = dir.path().join("empty.db");
                let mut empty = Tree::new();
                empty.store(&empty_path)?;
                let mut empty_query = Query::try_new(&empty_path)?;
                assert!(empty_query.is_empty().map_err(|error| error.to_io())?);
                assert!(empty_query.iter().collect::<io::Result<Vec<_>>>()?.is_empty());

                let path = dir.path().join("contract.db");
                let expected = vec![(10, vec![10]), (20, vec![20, 20]), (30, incompressible_value())];
                let mut tree = Tree::new();
                tree.set_metadata($metadata);
                for (key, value) in &expected {
                    tree.insert(*key, value.clone());
                }
                assert_eq!(tree.query(&20), Some(&vec![20, 20]));
                assert_eq!(tree.find_le(&25).map(|(key, _)| *key), Some(20));
                tree.store(&path)?;

                let loaded = Tree::load(&path)?;
                assert_eq!(loaded.get_metadata(), &$metadata);
                assert_eq!(loaded.iter().map(|(key, value)| (*key, value.clone())).collect::<Vec<_>>(), expected);

                let mut query = Query::try_new(&path)?;
                assert_eq!(query.query_zero_copy(&20).map_err(|error| error.to_io())?, Some(vec![20, 20]));
                assert_eq!(query.query_le(&25).map_err(|error| error.to_io())?, Some(vec![20, 20]));
                assert_eq!(query.iter().collect::<io::Result<Vec<_>>>()?, expected);

                let mut query = Query::try_new(&path)?;
                assert_eq!(
                    query.range_iter(Bound::Included(&20), Bound::Included(&30)).collect::<io::Result<Vec<_>>>()?,
                    expected[1..].to_vec()
                );
                Ok(())
            }
        };
    }

    storage_contract!(
        v3_storage_contract,
        BPlusTree<u32, Vec<u8>>,
        BPlusTreeQuery<u32, Vec<u8>>,
        BPlusTreeMetadata::TargetIdMapping(42)
    );

    #[test]
    fn v2_migration_reader_contract() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("v2.db");
        let expected = vec![(10, vec![10]), (20, vec![20, 20]), (30, incompressible_value())];
        let mut tree = v2::BPlusTree::new();
        tree.set_metadata(v2::BPlusTreeMetadata::TargetIdMapping(42));
        for (key, value) in &expected {
            tree.insert(*key, value.clone());
        }
        tree.store(&path)?;

        let mut query = v2::BPlusTreeQuery::<u32, Vec<u8>>::try_new(&path)?;
        let actual = query
            .range_iter(Bound::Unbounded, Bound::Unbounded)
            .collect::<Result<Vec<_>, _>>()
            .map_err(v2::BPlusTreeError::to_io)?;
        assert_eq!(actual, expected);
        Ok(())
    }

    #[test]
    fn failed_v3_store_preserves_published_database() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("preserved.db");
        let mut tree = BPlusTree::<String, u32>::new();
        tree.insert("published".into(), 7);
        tree.store(&path)?;
        tree.insert("x".repeat(2_100), 8);
        assert!(tree.store(&path).is_err());

        let mut query = BPlusTreeQuery::<String, u32>::try_new(&path)?;
        assert_eq!(query.query(&"published".into()).map_err(v2::BPlusTreeError::to_io)?, Some(7));
        assert_eq!(query.query(&"x".repeat(2_100)).map_err(v2::BPlusTreeError::to_io)?, None);
        Ok(())
    }

    #[test]
    fn v3_full_verification_contract() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("verified.db");
        let mut tree = BPlusTree::new();
        tree.insert(1u32, incompressible_value());
        tree.store(&path)?;
        let mut query = BPlusTreeQuery::<u32, Vec<u8>>::try_new(&path)?;
        assert_eq!(verify_full(&mut query)?.live_entries, 1);
        Ok(())
    }
}

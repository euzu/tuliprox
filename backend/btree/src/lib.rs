//! Persistent B+Tree storage engine.
//!
//! Everything below this module is engine-internal: page and cell layout, the
//! write-ahead log, sidecar locking, the sorted-index sidecar and the v2
//! compatibility reader. The list of `pub` items in this file is the engine's
//! entire contract with the rest of the backend, and is the surface that becomes
//! the `tuliprox-btree` crate's API.
//!
//! Nothing in here may name application types, `AppState`, `api`, backend
//! `model`, backend `utils` or a Tuliprox-specific error.

pub(crate) mod codec;
mod common;
pub(crate) mod sorted_index;
#[cfg(test)]
mod stress;
// The v2 line is read-only in production: only the migration reader uses it.
// Its writer is a fixture builder, published under `test-support`.
#[cfg(any(test, feature = "test-support"))]
pub mod v2;
#[cfg(not(any(test, feature = "test-support")))]
pub(crate) mod v2;
pub(crate) mod v3;

// --- Trees, queries and iterators -----------------------------------------
// --- Errors ----------------------------------------------------------------
pub use common::BPlusTreeError;
// --- On-disk layout --------------------------------------------------------
// A published database has two companion files whose names the engine derives:
// the sorted-index sidecar and the sidecar lock. Callers that stage, publish or
// clean up databases need both, and they must agree with the engine or a
// staging database can end up sharing a lock domain with a published one.
pub use common::{ensure_distinct_sidecar_lock_domains, get_file_path_for_db_index, sidecar_lock_path};
// --- Sorted-index sidecar --------------------------------------------------
/// Iterates a database in sorted-index order, falling back to the caller's own
/// handling when the sidecar is unusable.
pub use sorted_index::v4::OwnedIterator as SortedIndexIterator;
// --- Generic typed migration ----------------------------------------------
// Only the mechanics live here. Which databases exist, which record schema each
// one holds, and what to do when a migration fails are application decisions and
// live in the application that links this crate.
pub use v3::migration as typed_migration;
// --- Staged publication ----------------------------------------------------
pub use v3::{publish_staged_database, BPlusTreeStagingArtifacts};
pub use v3::{
    BPlusTree, BPlusTreeDiskIterator, BPlusTreeDiskIteratorOwned, BPlusTreeMetadata, BPlusTreeQuery,
    BPlusTreeRangeIterator, BPlusTreeSerialWriter, BPlusTreeUpdate, FlushPolicy, MAGIC, STORAGE_VERSION,
};

// --- Test support ----------------------------------------------------------
/// Fixture builders for the tests of crates that depend on this one.
///
/// Nothing here is part of the production API. A dependent's tests have to be
/// able to construct the inputs this engine reads - a legacy v2 database, a
/// sorted-index sidecar with a deliberately corrupt entry - and those builders
/// necessarily reach below the public surface. Publishing them behind a feature
/// keeps them out of the production API instead of widening it to accommodate
/// tests: enable `test-support` from `[dev-dependencies]` and it stays off for
/// a normal build.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    pub use crate::{sorted_index::v4, v3::Locator};
    use std::io;

    /// Every entry of a database together with its physical locator, for
    /// building a sorted-index sidecar by hand.
    pub fn collect_with_locators<K, V>(query: &mut crate::BPlusTreeQuery<K, V>) -> io::Result<Vec<(K, V, Locator)>>
    where
        K: Ord + serde::Serialize + for<'de> serde::Deserialize<'de> + Clone,
        V: serde::Serialize + for<'de> serde::Deserialize<'de> + Clone,
    {
        query.collect_with_locators()
    }

    /// The identity a sorted-index sidecar has to carry to be accepted for a
    /// given database snapshot.
    pub fn snapshot_identity<K, V>(query: &crate::BPlusTreeQuery<K, V>) -> ([u8; 16], u64) {
        query.snapshot_identity()
    }
}

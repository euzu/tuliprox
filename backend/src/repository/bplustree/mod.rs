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
pub(crate) mod v2;
pub(crate) mod v3;

// --- Trees, queries and iterators -----------------------------------------
pub use v3::{
    BPlusTree, BPlusTreeDiskIterator, BPlusTreeDiskIteratorOwned, BPlusTreeMetadata, BPlusTreeQuery,
    BPlusTreeRangeIterator, BPlusTreeSerialWriter, BPlusTreeUpdate, FlushPolicy,
};

// --- Errors ----------------------------------------------------------------
pub use common::BPlusTreeError;

// --- Staged publication ----------------------------------------------------
pub use v3::{publish_staged_database, BPlusTreeStagingArtifacts};

// --- On-disk layout --------------------------------------------------------
// A published database has two companion files whose names the engine derives:
// the sorted-index sidecar and the sidecar lock. Callers that stage, publish or
// clean up databases need both, and they must agree with the engine or a
// staging database can end up sharing a lock domain with a published one.
pub use common::{ensure_distinct_sidecar_lock_domains, get_file_path_for_db_index, sidecar_lock_path};
pub use v3::{Locator, MAGIC, STORAGE_VERSION};

// --- Sorted-index sidecar --------------------------------------------------
/// Iterates a database in sorted-index order, falling back to the caller's own
/// handling when the sidecar is unusable.
pub use sorted_index::v4::OwnedIterator as SortedIndexIterator;

// --- Generic typed migration ----------------------------------------------
// Only the mechanics live here. Which databases exist, which record schema each
// one holds, and what to do when a migration fails are application decisions and
// live above the engine in `repository::startup_migration`.
pub use v3::migration as typed_migration;

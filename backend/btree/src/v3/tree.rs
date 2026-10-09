use super::{
    format::{
        decompress_value_in_place, decompress_value_into, encode_inline_leaf_cell, encode_internal_cell,
        encode_overflow_leaf_cell, encode_tombstone_leaf_cell, encode_value, stored_value_checksum, Compression,
        DatabaseHeader, InternalCellRef, InternalPreamble, LeafCellRef, LeafValueRef, Locator, PageHeader, PageType,
        Slot, MAX_CELL_FOOTPRINT, MAX_INLINE_STORED_VALUE, OVERFLOW_PAYLOAD_LEN, PAGE_HEADER_LEN, PAGE_SIZE, SLOT_LEN,
    },
    page::{encode_free_page, encode_overflow_page, overflow_payload, PageValidation, SlottedPage},
    wal::{
        commit_ordered_page_refs_under_existing_lock, invalidate_sorted_index, recover_pending,
        recover_pending_under_existing_lock, recovery_required, sync_parent_directory, wal_path, wal_temporary_path,
        with_exclusive_sidecar, ExclusiveSidecarGuard, SharedSidecarGuard, WalOperationError, WalOutcome,
    },
    BPlusTreeMetadata,
};
use parking_lot::Mutex;
use std::{
    collections::BTreeMap,
    io::{self},
    marker::PhantomData,
    ops::{Bound, Range},
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc},
    thread::JoinHandle,
};

#[derive(Debug)]
pub(crate) struct InternalSplit<'a, T> {
    #[cfg(test)]
    pub(crate) promoted_index: usize,
    pub(crate) promoted: InternalCellRef<'a>,
    pub(crate) right_leftmost_child: u64,
    pub(crate) left_cells: &'a [T],
    pub(crate) right_cells: &'a [T],
}

#[cfg(test)]
thread_local! {
    static INTERNAL_KEY_DECODE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Clone, Debug)]
pub struct BPlusTree<K, V> {
    entries: BTreeMap<K, V>,
    metadata: BPlusTreeMetadata,
    dirty: bool,
}

const PAGE_SIZE_U64: u64 = PAGE_SIZE as u64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlushPolicy {
    Immediate,
    Batch,
}

pub struct BPlusTreeUpdate<K, V> {
    filepath: PathBuf,
    database_id: [u8; 16],
    verified_generation: u64,
    verified_next_page_id: u64,
    flush_policy: FlushPolicy,
    active: Option<ActiveBatch>,
    scratch: WriteScratch,
    _types: PhantomData<(K, V)>,
}

pub struct BPlusTreeSerialWriter<K, V> {
    updater: Arc<Mutex<BPlusTreeUpdate<K, V>>>,
    flush_policy: FlushPolicy,
    dirty: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    background_error: Arc<Mutex<Option<io::Error>>>,
    background_handle: Mutex<Option<JoinHandle<()>>>,
}

pub struct BPlusTreeQuery<K, V> {
    snapshot: Arc<QuerySnapshot<K>>,
    header: DatabaseHeader,
    file_len: usize,
    page_buffer: Vec<u8>,
    value_scratch: Vec<u8>,
    locator_page_id: u64,
    locator_cell_ranges: Vec<Range<usize>>,
    _value: PhantomData<V>,
}

pub struct BPlusTreeDiskIterator<'a, K, V> {
    query: &'a mut BPlusTreeQuery<K, V>,
    state: CursorState<K, V>,
}

pub struct BPlusTreeDiskIteratorOwned<K, V> {
    query: BPlusTreeQuery<K, V>,
    state: CursorState<K, V>,
}

pub struct BPlusTreeRangeIterator<'a, K, V> {
    iterator: BPlusTreeDiskIterator<'a, K, V>,
    start: Bound<K>,
    end: Bound<K>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct VerificationReport {
    pub(crate) live_entries: u64,
    pub(crate) tree_pages: u64,
    pub(crate) overflow_pages: u64,
    pub(crate) free_pages: u64,
}

#[cfg(test)]
mod tests;

mod bulk_store;
mod error;
mod iterators;
mod memory;
mod mutation;
mod query;
mod serial_writer;
mod transaction;
mod update;
mod verify;

#[cfg(test)]
use self::mutation::database_page;
#[allow(unused_imports)]
pub(crate) use self::mutation::{choose_internal_split, choose_leaf_split, used_leaf_bytes};
#[cfg(test)]
use self::query::{internal_key_decode_count, reset_internal_key_decode_count};
#[cfg(test)]
pub(crate) use self::query::{read_leaf_value, validate_locator};
#[cfg(test)]
use self::transaction::page_byte_range;
use self::{
    bulk_store::{
        build_pages, internal_page, leaf_page, temporary_path, verify_internal_page, verify_leaf_page, PageSink,
        StoredDatabase,
    },
    error::{invalid_data, invalid_input},
    iterators::{record_page_visit, CursorState},
    mutation::{
        checked_page_usage, locate_transaction_leaf, minimum_tree_key, overflow_chain_pages, push_child_visits,
        stage_delete, stage_upsert, validate_leaf_backlink, validated_overflow_chain_pages,
    },
    query::{decode_key, QuerySnapshot},
    transaction::{query_transaction, DatabaseImage, WriteScratch, WriteTransaction},
    update::ActiveBatch,
    verify::{validate_transaction_links, VerifyLeaf, VerifyValue, Visit},
};
pub(super) use self::{
    bulk_store::{publish_database, publish_database_and_invalidate_sorted_index},
    error::PublishDatabaseError,
};
pub(crate) use self::{query::search_leaf, verify::verify_full};

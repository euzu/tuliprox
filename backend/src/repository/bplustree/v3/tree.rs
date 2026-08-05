use super::{
    format::{
        decompress_value_in_place, decompress_value_into, encode_inline_leaf_cell, encode_internal_cell,
        encode_overflow_leaf_cell, encode_tombstone_leaf_cell, encode_value, stored_value_checksum, Compression,
        DatabaseHeader, InternalCellRef, InternalPreamble, LeafCellRef, LeafValueRef, Locator, PageHeader, PageType, Slot,
        MAX_CELL_FOOTPRINT, MAX_INLINE_STORED_VALUE, OVERFLOW_PAYLOAD_LEN, PAGE_HEADER_LEN, PAGE_SIZE, SLOT_LEN,
    },
    page::{encode_free_page, encode_overflow_page, overflow_payload, PageValidation, SlottedPage},
    wal::{
        commit_ordered_page_refs_under_existing_lock, invalidate_sorted_index, recover_pending,
        recover_pending_under_existing_lock, recovery_required, sync_parent_directory, wal_path, wal_temporary_path,
        with_exclusive_sidecar, ExclusiveSidecarGuard, SharedSidecarGuard, WalOperationError, WalOutcome,
    },
    BPlusTreeMetadata,
};
use crate::{
    repository::bplustree::common::{mmap_with_advice, read_exact_at_offset, write_all_at_offset, Advice, BPlusTreeError},
    utils::{binary_deserialize, binary_serialize, binary_serialize_into},
};
use memmap2::Mmap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{File, OpenOptions},
    io::{self, Read},
    marker::PhantomData,
    ops::{Bound, Range},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
    },
    thread::JoinHandle,
    time::Duration,
};

fn invalid_data(message: &'static str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message) }

fn invalid_input(message: &'static str) -> io::Error { io::Error::new(io::ErrorKind::InvalidInput, message) }

fn checked_page_usage<'a, I>(base: usize, cells: I, maximum_cell_footprint: usize) -> io::Result<usize>
where
    I: IntoIterator<Item = &'a [u8]>,
{
    cells.into_iter().try_fold(base, |used, cell| {
        let footprint = SLOT_LEN
            .checked_add(cell.len())
            .ok_or_else(|| invalid_input("cell footprint overflow"))?;
        if footprint > maximum_cell_footprint {
            return Err(invalid_input("cell footprint exceeds format limit"));
        }
        used.checked_add(footprint).ok_or_else(|| invalid_input("page usage overflow"))
    })
}

pub(crate) fn used_leaf_bytes<T: AsRef<[u8]>>(cells: &[T]) -> io::Result<usize> {
    checked_page_usage(PAGE_HEADER_LEN, cells.iter().map(AsRef::as_ref), MAX_CELL_FOOTPRINT)
}

pub(crate) fn choose_leaf_split<T: AsRef<[u8]>>(cells: &[T]) -> io::Result<usize> {
    if cells.len() < 2 {
        return Err(invalid_input("leaf split requires two non-empty outputs"));
    }
    let mut total = 0usize;
    for cell in cells {
        let footprint = SLOT_LEN
            .checked_add(cell.as_ref().len())
            .ok_or_else(|| invalid_input("leaf cell footprint overflow"))?;
        if footprint > MAX_CELL_FOOTPRINT {
            return Err(invalid_input("leaf cell footprint exceeds format limit"));
        }
        total = total.checked_add(footprint).ok_or_else(|| invalid_input("leaf split usage overflow"))?;
    }

    let mut left_payload = 0usize;
    let mut best = None;
    for boundary in 1..cells.len() {
        let cell = cells
            .get(boundary - 1)
            .ok_or_else(|| invalid_input("leaf split boundary is outside cells"))?;
        let footprint = SLOT_LEN
            .checked_add(cell.as_ref().len())
            .ok_or_else(|| invalid_input("leaf cell footprint overflow"))?;
        left_payload = left_payload
            .checked_add(footprint)
            .ok_or_else(|| invalid_input("leaf split usage overflow"))?;
        let right_payload = total
            .checked_sub(left_payload)
            .ok_or_else(|| invalid_input("leaf split usage underflow"))?;
        let left_used = PAGE_HEADER_LEN
            .checked_add(left_payload)
            .ok_or_else(|| invalid_input("leaf split usage overflow"))?;
        let right_used = PAGE_HEADER_LEN
            .checked_add(right_payload)
            .ok_or_else(|| invalid_input("leaf split usage overflow"))?;
        if left_used <= PAGE_SIZE && right_used <= PAGE_SIZE {
            let imbalance = left_used.abs_diff(right_used);
            if best.is_none_or(|(_, best_imbalance)| imbalance < best_imbalance) {
                best = Some((boundary, imbalance));
            }
        }
    }
    best.map(|(boundary, _)| boundary)
        .ok_or_else(|| invalid_input("leaf split has no valid boundary"))
}

fn used_internal_bytes<'a, I>(cells: I) -> io::Result<usize>
where
    I: IntoIterator<Item = &'a [u8]>,
{
    checked_page_usage(PAGE_HEADER_LEN + 8, cells, SLOT_LEN + 12 + 2004)
}

#[derive(Debug)]
pub(crate) struct InternalSplit<'a, T> {
    #[cfg(test)]
    pub(crate) promoted_index: usize,
    pub(crate) promoted: InternalCellRef<'a>,
    pub(crate) right_leftmost_child: u64,
    pub(crate) left_cells: &'a [T],
    pub(crate) right_cells: &'a [T],
}

pub(crate) fn choose_internal_split<T: AsRef<[u8]>>(
    cells: &[T],
    page_id: u64,
    next_page_id: u64,
) -> io::Result<InternalSplit<'_, T>> {
    if cells.len() < 3 {
        return Err(invalid_input("internal split requires two non-empty outputs and a promoted separator"));
    }
    for cell in cells {
        InternalCellRef::decode(cell.as_ref(), page_id, next_page_id)?;
    }

    let mut best = None;
    for promoted_index in 1..cells.len() - 1 {
        let left_used = used_internal_bytes(cells[..promoted_index].iter().map(AsRef::as_ref))?;
        let right_used = used_internal_bytes(cells[promoted_index + 1..].iter().map(AsRef::as_ref))?;
        if left_used <= PAGE_SIZE && right_used <= PAGE_SIZE {
            let imbalance = left_used.abs_diff(right_used);
            if best.is_none_or(|(_, best_imbalance)| imbalance < best_imbalance) {
                best = Some((promoted_index, imbalance));
            }
        }
    }
    let promoted_index = best
        .map(|(index, _)| index)
        .ok_or_else(|| invalid_input("internal split has no valid boundary"))?;
    let promoted = InternalCellRef::decode(
        cells
            .get(promoted_index)
            .ok_or_else(|| invalid_input("promoted separator is outside cells"))?
            .as_ref(),
        page_id,
        next_page_id,
    )?;
    Ok(InternalSplit {
        #[cfg(test)]
        promoted_index,
        right_leftmost_child: promoted.right_child,
        promoted,
        left_cells: cells
            .get(..promoted_index)
            .ok_or_else(|| invalid_input("left split range is outside cells"))?,
        right_cells: cells
            .get(promoted_index + 1..)
            .ok_or_else(|| invalid_input("right split range is outside cells"))?,
    })
}

fn decode_key<K>(encoded: &[u8]) -> io::Result<K>
where
    K: for<'de> Deserialize<'de>,
{
    rmp_serde::from_slice(encoded)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, format!("invalid serialized key: {err}")))
}

#[cfg(test)]
thread_local! {
    static INTERNAL_KEY_DECODE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_internal_key_decode_count() { INTERNAL_KEY_DECODE_COUNT.set(0); }

#[cfg(test)]
fn internal_key_decode_count() -> usize { INTERNAL_KEY_DECODE_COUNT.get() }

fn decode_internal_key<K>(encoded: &[u8]) -> io::Result<K>
where
    K: for<'de> Deserialize<'de>,
{
    #[cfg(test)]
    INTERNAL_KEY_DECODE_COUNT.with(|count| count.set(count.get().saturating_add(1)));
    decode_key(encoded)
}

pub(crate) fn search_leaf<K, B>(page: &SlottedPage<B>, target: &K) -> io::Result<Result<usize, usize>>
where
    K: Ord + for<'de> Deserialize<'de>,
    B: AsRef<[u8]>,
{
    if page.header().page_type != PageType::Leaf {
        return Err(invalid_data("typed leaf search requires a leaf page"));
    }
    let mut left = 0usize;
    let mut right = usize::from(page.header().cell_count);
    while left < right {
        let middle = left + (right - left) / 2;
        let cell = LeafCellRef::decode(page.cell(middle)?, page.page_id(), page.next_page_id())?;
        match decode_key::<K>(cell.key_bytes)?.cmp(target) {
            std::cmp::Ordering::Less => left = middle + 1,
            std::cmp::Ordering::Greater => right = middle,
            std::cmp::Ordering::Equal => return Ok(Ok(middle)),
        }
    }
    Ok(Err(left))
}

#[cfg(test)]
pub(crate) fn validate_locator<B: AsRef<[u8]>>(
    page: &SlottedPage<B>,
    locator: Locator,
    serialized_primary_key: &[u8],
) -> io::Result<()> {
    if page.header().page_type != PageType::Leaf || page.page_id() != locator.leaf_page_id {
        return Err(invalid_data("locator does not reference this leaf page"));
    }
    let cell = LeafCellRef::decode(
        page.cell(usize::from(locator.slot_index))?,
        page.page_id(),
        page.next_page_id(),
    )?;
    let cell_crc = crc32fast::hash(cell.key_bytes);
    if cell_crc != locator.serialized_key_crc32
        || crc32fast::hash(serialized_primary_key) != locator.serialized_key_crc32
        || cell.key_bytes != serialized_primary_key
    {
        return Err(invalid_data("locator serialized key mismatch"));
    }
    Ok(())
}

#[cfg(test)]
fn database_page(database: &[u8], page_id: u64, next_page_id: u64) -> io::Result<&[u8]> {
    if page_id == 0 || page_id >= next_page_id {
        return Err(invalid_data("overflow page id is outside database"));
    }
    let page_id = usize::try_from(page_id).map_err(|_| invalid_data("overflow page id exceeds usize"))?;
    let offset = page_id
        .checked_mul(PAGE_SIZE)
        .ok_or_else(|| invalid_data("overflow page offset overflow"))?;
    let end = offset
        .checked_add(PAGE_SIZE)
        .ok_or_else(|| invalid_data("overflow page end overflow"))?;
    database.get(offset..end).ok_or_else(|| invalid_data("truncated overflow page"))
}

#[cfg(test)]
pub(crate) fn read_leaf_value<'a>(
    database: &'a [u8],
    value: &LeafValueRef<'a>,
    next_page_id: u64,
    maximum_length: usize,
    scratch: &'a mut Vec<u8>,
) -> io::Result<Option<&'a [u8]>> {
    match *value {
        LeafValueRef::Tombstone => Ok(None),
        LeafValueRef::Inline { compression, logical_len, stored, .. } => {
            let logical_len = usize::try_from(logical_len).map_err(|_| invalid_data("logical length exceeds usize"))?;
            if logical_len > maximum_length {
                return Err(invalid_data("logical length exceeds allocation limit"));
            }
            match compression {
                Compression::None => Ok(Some(stored)),
                Compression::Lz4 => decompress_value_into(
                    stored,
                    u32::try_from(logical_len).map_err(|_| invalid_data("logical length exceeds u32"))?,
                    maximum_length,
                    scratch,
                )
                .map(Some),
            }
        }
        LeafValueRef::Overflow { compression, logical_len, stored_len, head, crc32 } => {
            let logical_length = usize::try_from(logical_len).map_err(|_| invalid_data("logical length exceeds usize"))?;
            let stored_length = usize::try_from(stored_len).map_err(|_| invalid_data("stored length exceeds usize"))?;
            if logical_length > maximum_length || stored_length > maximum_length {
                return Err(invalid_data("overflow value exceeds allocation limit"));
            }
            scratch.clear();
            scratch
                .try_reserve(stored_length)
                .map_err(|err| io::Error::new(io::ErrorKind::OutOfMemory, err))?;
            let mut page_id = head;
            let mut visited = HashSet::new();
            while page_id != 0 {
                if u64::try_from(visited.len()).map_err(|_| invalid_data("overflow chain length exceeds u64"))?
                    >= next_page_id
                {
                    return Err(invalid_data("overflow chain contains a cycle"));
                }
                let page = SlottedPage::open(database_page(database, page_id, next_page_id)?, page_id, next_page_id)?;
                if page.header().page_type != PageType::Overflow {
                    return Err(invalid_data("overflow chain references a non-overflow page"));
                }
                let payload = overflow_payload(&page)?;
                if payload.is_empty() {
                    return Err(invalid_data("overflow chain contains an empty payload"));
                }
                visited
                    .try_reserve(1)
                    .map_err(|err| io::Error::new(io::ErrorKind::OutOfMemory, err))?;
                if !visited.insert(page_id) {
                    return Err(invalid_data("overflow chain contains a cycle"));
                }
                let new_length = scratch
                    .len()
                    .checked_add(payload.len())
                    .ok_or_else(|| invalid_data("overflow chain length overflow"))?;
                if new_length > stored_length {
                    return Err(invalid_data("overflow chain exceeds stored length"));
                }
                scratch.extend_from_slice(payload);
                page_id = page.header().right;
            }
            if scratch.len() != stored_length {
                return Err(invalid_data("overflow chain stored length mismatch"));
            }
            if crc32fast::hash(scratch) != crc32 {
                return Err(invalid_data("stored value checksum mismatch"));
            }
            if compression == Compression::Lz4 {
                decompress_value_in_place(scratch, logical_len, maximum_length)?;
            } else if scratch.len() != logical_length {
                return Err(invalid_data("uncompressed overflow length mismatch"));
            }
            Ok(Some(scratch.as_slice()))
        }
    }
}

#[derive(Clone, Debug)]
pub struct BPlusTree<K, V> {
    entries: BTreeMap<K, V>,
    metadata: BPlusTreeMetadata,
    dirty: bool,
}

impl<K: Ord, V> Default for BPlusTree<K, V> {
    fn default() -> Self { Self::new() }
}

impl<K: Ord, V> BPlusTree<K, V> {
    pub const fn new() -> Self {
        Self { entries: BTreeMap::new(), metadata: BPlusTreeMetadata::Empty, dirty: true }
    }

    pub fn get_metadata(&self) -> &BPlusTreeMetadata { &self.metadata }

    pub fn set_metadata(&mut self, metadata: BPlusTreeMetadata) {
        self.metadata = metadata;
        self.dirty = true;
    }

    pub fn is_empty(&self) -> bool { self.entries.is_empty() }

    pub fn len(&self) -> usize { self.entries.len() }

    pub fn insert(&mut self, key: K, value: V) {
        let _ = self.entries.insert(key, value);
        self.dirty = true;
    }

    pub fn query(&self, key: &K) -> Option<&V> { self.entries.get(key) }

    pub fn find_le(&self, key: &K) -> Option<(&K, &V)> { self.entries.range(..=key).next_back() }

    pub fn iter(&self) -> std::collections::btree_map::Iter<'_, K, V> { self.entries.iter() }
}

impl<'a, K: Ord, V> IntoIterator for &'a BPlusTree<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = std::collections::btree_map::Iter<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter { self.entries.iter() }
}

#[derive(Clone)]
struct NodeInfo {
    page_id: u64,
    minimum_key: Vec<u8>,
}

/// Sink for the bulk tree builder: hands out page ids and writes each finished page
/// straight to the destination file instead of buffering the whole database in memory.
///
/// Pages are **not** emitted in ascending id order — a leaf reserves its id before the
/// overflow chain it points at, but is encoded only once all of its cells are known, so
/// it is written after them. Writes must therefore be positional; a sequential writer
/// would silently scramble the file. Every id handed out is written exactly once, so the
/// finished file has no holes, and `verify_full` re-reads it before it is published.
struct PageSink {
    file: File,
    next_page_id: u64,
}

const PAGE_SIZE_U64: u64 = PAGE_SIZE as u64;

impl PageSink {
    const fn new(file: File) -> Self { Self { file, next_page_id: 0 } }

    fn allocate(&mut self) -> io::Result<u64> {
        let page_id = self.next_page_id;
        self.next_page_id = page_id.checked_add(1).ok_or_else(|| invalid_input("page count exceeds u64"))?;
        Ok(page_id)
    }

    fn allocate_many(&mut self, count: usize) -> io::Result<u64> {
        let head = self.next_page_id;
        let count = u64::try_from(count).map_err(|_| invalid_input("page count exceeds u64"))?;
        self.next_page_id = head.checked_add(count).ok_or_else(|| invalid_input("page count exceeds u64"))?;
        Ok(head)
    }

    fn write(&self, page_id: u64, page: &[u8; PAGE_SIZE]) -> io::Result<()> {
        let offset = page_id.checked_mul(PAGE_SIZE_U64).ok_or_else(|| invalid_input("page offset overflow"))?;
        write_all_at_offset(&self.file, page, offset)
    }
}

fn leaf_page<T: AsRef<[u8]>>(page_id: u64, left: u64, right: u64, cells: &[T]) -> io::Result<[u8; PAGE_SIZE]> {
    let mut bytes = [0; PAGE_SIZE];
    PageHeader {
        page_type: PageType::Leaf,
        cell_count: 0,
        free_start: u16::try_from(PAGE_HEADER_LEN).map_err(|_| invalid_input("leaf header exceeds u16"))?,
        free_end: u16::try_from(PAGE_SIZE).map_err(|_| invalid_input("page size exceeds u16"))?,
        left,
        right,
    }
    .encode_into(&mut bytes, page_id, u64::MAX)?;
    SlottedPage::open(bytes.as_mut_slice(), page_id, u64::MAX)?
        .rebuild_ordered(cells.iter().map(AsRef::as_ref))?;
    Ok(bytes)
}

fn internal_page<T: AsRef<[u8]>>(
    page_id: u64,
    leftmost_child: u64,
    cells: &[T],
) -> io::Result<[u8; PAGE_SIZE]> {
    let first = cells
        .first()
        .ok_or_else(|| invalid_input("internal page must contain a separator"))?
        .as_ref();
    let first_offset = PAGE_SIZE
        .checked_sub(first.len())
        .ok_or_else(|| invalid_input("internal cell exceeds page"))?;
    let mut bytes = [0; PAGE_SIZE];
    InternalPreamble { leftmost_child }.encode_into(&mut bytes, page_id, u64::MAX)?;
    bytes
        .get_mut(40..44)
        .ok_or_else(|| invalid_input("internal slot is outside page"))?
        .copy_from_slice(
            &Slot {
                offset: u16::try_from(first_offset).map_err(|_| invalid_input("internal cell offset exceeds u16"))?,
                length: u16::try_from(first.len()).map_err(|_| invalid_input("internal cell length exceeds u16"))?,
            }
            .encode(),
        );
    bytes
        .get_mut(first_offset..)
        .ok_or_else(|| invalid_input("internal cell is outside page"))?
        .copy_from_slice(first);
    PageHeader {
        page_type: PageType::Internal,
        cell_count: 1,
        free_start: 44,
        free_end: u16::try_from(first_offset).map_err(|_| invalid_input("internal free_end exceeds u16"))?,
        left: 0,
        right: 0,
    }
    .encode_into(&mut bytes, page_id, u64::MAX)?;
    SlottedPage::open(bytes.as_mut_slice(), page_id, u64::MAX)?
        .rebuild_ordered(cells.iter().map(AsRef::as_ref))?;
    Ok(bytes)
}

fn allocate_overflow_chain(sink: &mut PageSink, stored: &[u8]) -> io::Result<u64> {
    let count = stored.len().div_ceil(OVERFLOW_PAYLOAD_LEN);
    if count == 0 {
        return Err(invalid_input("overflow value must not be empty"));
    }
    let head = sink.allocate_many(count)?;
    for (index, payload) in stored.chunks(OVERFLOW_PAYLOAD_LEN).enumerate() {
        let page_id = head
            .checked_add(u64::try_from(index).map_err(|_| invalid_input("overflow index exceeds u64"))?)
            .ok_or_else(|| invalid_input("overflow page id overflow"))?;
        let next = if index + 1 == count {
            0
        } else {
            page_id.checked_add(1).ok_or_else(|| invalid_input("overflow page id overflow"))?
        };
        let page = encode_overflow_page(page_id, u64::MAX, next, payload)?;
        sink.write(page_id, &page)?;
    }
    Ok(head)
}

fn finish_leaf(sink: &PageSink, page_id: u64, left: u64, right: u64, cells: &[Vec<u8>]) -> io::Result<()> {
    let page = leaf_page(page_id, left, right, cells)?;
    sink.write(page_id, &page)
}

fn build_leaf_level<K, V>(entries: &BTreeMap<K, V>, sink: &mut PageSink) -> io::Result<Vec<NodeInfo>>
where
    K: Ord + Serialize,
    V: Serialize,
{
    let mut leaf_id = sink.allocate()?;
    let mut left_leaf = 0;
    let mut cells = Vec::<Vec<u8>>::new();
    let mut leaves = Vec::<NodeInfo>::new();
    let mut leaf_minimum = None;
    let mut compression_scratch = Vec::new();
    let mut cell = Vec::new();

    for (key, value) in entries {
        let key_bytes = binary_serialize(key)?;
        let raw_value = binary_serialize(value)?;
        let logical_len = u32::try_from(raw_value.len()).map_err(|_| invalid_input("serialized value exceeds u32"))?;
        let stored = encode_value(&raw_value, &mut compression_scratch)?;
        let stored_bytes = stored.as_slice();
        let inline_footprint = SLOT_LEN
            .checked_add(24)
            .and_then(|size| size.checked_add(key_bytes.len()))
            .and_then(|size| size.checked_add(stored_bytes.len()))
            .ok_or_else(|| invalid_input("leaf cell footprint overflow"))?;
        if stored_bytes.len() <= MAX_INLINE_STORED_VALUE && inline_footprint <= MAX_CELL_FOOTPRINT {
            encode_inline_leaf_cell(&key_bytes, logical_len, stored.compression(), stored_bytes, &mut cell)?;
        } else {
            let head = allocate_overflow_chain(sink, stored_bytes)?;
            encode_overflow_leaf_cell(
                &key_bytes,
                logical_len,
                stored.compression(),
                u32::try_from(stored_bytes.len()).map_err(|_| invalid_input("stored value exceeds u32"))?,
                head,
                stored_value_checksum(stored_bytes),
                leaf_id,
                u64::MAX,
                &mut cell,
            )?;
        }

        let next_usage = checked_page_usage(
            PAGE_HEADER_LEN,
            cells.iter().map(Vec::as_slice).chain(std::iter::once(cell.as_slice())),
            MAX_CELL_FOOTPRINT,
        )?;
        if next_usage > PAGE_SIZE {
            let next_leaf = sink.allocate()?;
            finish_leaf(sink, leaf_id, left_leaf, next_leaf, &cells)?;
            leaves.push(NodeInfo {
                page_id: leaf_id,
                minimum_key: leaf_minimum.take().ok_or_else(|| invalid_input("non-empty leaf has no minimum key"))?,
            });
            left_leaf = leaf_id;
            leaf_id = next_leaf;
            cells.clear();
        }
        if cells.is_empty() {
            leaf_minimum = Some(key_bytes);
        }
        cells.try_reserve(1).map_err(|err| io::Error::new(io::ErrorKind::OutOfMemory, err))?;
        cells.push(std::mem::take(&mut cell));
    }

    finish_leaf(sink, leaf_id, left_leaf, 0, &cells)?;
    leaves.push(NodeInfo { page_id: leaf_id, minimum_key: leaf_minimum.unwrap_or_default() });
    Ok(leaves)
}

fn group_internal_children(level: &[NodeInfo]) -> io::Result<Vec<(usize, usize)>> {
    let mut groups = Vec::<(usize, usize)>::new();
    let mut start = 0usize;
    while start < level.len() {
        let mut end = start + 1;
        let mut used = PAGE_HEADER_LEN + 8;
        while end < level.len() {
            let child = level.get(end).ok_or_else(|| invalid_input("internal child index is invalid"))?;
            let cell_len = 12usize
                .checked_add(child.minimum_key.len())
                .ok_or_else(|| invalid_input("internal cell length overflow"))?;
            let next = used
                .checked_add(SLOT_LEN)
                .and_then(|size| size.checked_add(cell_len))
                .ok_or_else(|| invalid_input("internal page usage overflow"))?;
            if next > PAGE_SIZE {
                break;
            }
            used = next;
            end += 1;
        }
        groups.push((start, end));
        start = end;
    }
    if groups.len() > 1 && groups.last().is_some_and(|(from, to)| to - from == 1) {
        let last = groups.len() - 1;
        let previous = groups
            .get(last - 1)
            .copied()
            .ok_or_else(|| invalid_input("internal grouping is missing its previous group"))?;
        let moved = previous.1.checked_sub(1).ok_or_else(|| invalid_input("internal grouping underflow"))?;
        if moved <= previous.0 {
            return Err(invalid_input("internal page cannot retain two children"));
        }
        groups
            .get_mut(last - 1)
            .ok_or_else(|| invalid_input("internal grouping is missing its previous group"))?
            .1 = moved;
        groups
            .get_mut(last)
            .ok_or_else(|| invalid_input("internal grouping is missing its last group"))?
            .0 = moved;
    }
    Ok(groups)
}

fn build_parent_level(level: &[NodeInfo], sink: &mut PageSink) -> io::Result<Vec<NodeInfo>> {
    let groups = group_internal_children(level)?;
    let mut parents = Vec::with_capacity(groups.len());
    for (start, end) in groups {
        let children = level.get(start..end).ok_or_else(|| invalid_input("internal child range is invalid"))?;
        if children.len() < 2 {
            return Err(invalid_input("internal page requires two children"));
        }
        let page_id = sink.allocate()?;
        let (first_child, remaining_children) =
            children.split_first().ok_or_else(|| invalid_input("internal page has no children"))?;
        let mut encoded_cells = Vec::with_capacity(remaining_children.len());
        for child in remaining_children {
            let mut encoded = Vec::new();
            encode_internal_cell(&child.minimum_key, child.page_id, page_id, u64::MAX, &mut encoded)?;
            encoded_cells.push(encoded);
        }
        let encoded = internal_page(page_id, first_child.page_id, &encoded_cells)?;
        sink.write(page_id, &encoded)?;
        parents.push(NodeInfo { page_id, minimum_key: first_child.minimum_key.clone() });
    }
    Ok(parents)
}

/// Streams the whole tree into `sink` and returns the root page id. Page 0 is reserved
/// for the database header, which the caller writes last once `sink.next_page_id` is final.
fn build_pages<K, V>(entries: &BTreeMap<K, V>, sink: &mut PageSink) -> io::Result<u64>
where
    K: Ord + Serialize,
    V: Serialize,
{
    let header_page_id = sink.allocate()?;
    if header_page_id != 0 {
        return Err(invalid_input("database header must be the first allocated page"));
    }
    let mut level = build_leaf_level(entries, sink)?;
    if entries.is_empty() {
        return Ok(level.first().ok_or_else(|| invalid_input("tree has no root page"))?.page_id);
    }
    while level.len() > 1 {
        level = build_parent_level(&level, sink)?;
    }
    Ok(level.first().ok_or_else(|| invalid_input("tree has no root page"))?.page_id)
}

fn temporary_path(filepath: &Path) -> io::Result<PathBuf> {
    let name = filepath
        .file_name()
        .ok_or_else(|| invalid_input("database path has no file name"))?
        .to_string_lossy();
    Ok(filepath.with_file_name(format!("{name}.{}.v3.tmp", uuid::Uuid::new_v4())))
}

fn publish_database(
    temporary: &Path,
    destination: &Path,
    sync_directory: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let temporary = match tempfile::TempPath::try_from_path(temporary) {
        Ok(path) => path,
        Err(error) => {
            let _ = std::fs::remove_file(temporary);
            return Err(error);
        }
    };
    temporary.persist(destination).map_err(io::Error::from)?;
    sync_directory(destination).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("database published but directory sync failed; durability unknown: {error}"),
        )
    })
}

impl<K, V> BPlusTree<K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de>,
{
    pub fn store(&mut self, filepath: &Path) -> io::Result<u64> {
        with_exclusive_sidecar(filepath, || {
            recover_pending_under_existing_lock(filepath)?;
            if self.dirty {
                self.store_exclusive(filepath).map(|stored| stored.root_page_id)
            } else {
                Ok(0)
            }
        })
    }

    pub(crate) fn store_verified(&mut self, filepath: &Path) -> io::Result<VerificationReport> {
        with_exclusive_sidecar(filepath, || {
            recover_pending_under_existing_lock(filepath)?;
            if !self.dirty {
                return Err(invalid_input("verified store requires a dirty tree"));
            }
            self.store_exclusive(filepath).map(|stored| stored.verification)
        })
    }

    fn store_exclusive(&mut self, filepath: &Path) -> io::Result<StoredDatabase> {
        let temp_path = temporary_path(filepath)?;
        // The builder streams into the temp file, so it exists from here on — every error
        // path below has to remove it again.
        let prepared = (|| {
            let mut sink = PageSink::new(OpenOptions::new().write(true).create_new(true).open(&temp_path)?);
            let root_page_id = build_pages(&self.entries, &mut sink)?;
            let header = DatabaseHeader {
                root_page_id,
                next_page_id: sink.next_page_id,
                free_page_head: 0,
                generation: 1,
                database_id: *uuid::Uuid::new_v4().as_bytes(),
                metadata: self.metadata.clone(),
            }
            .encode()?;
            sink.write(0, &header)?;
            sink.file.sync_all()?;
            drop(sink);
            let mut query = BPlusTreeQuery::<K, V>::from_file_unlocked(File::open(&temp_path)?)?;
            let verification = verify_full(&mut query)?;
            drop(query);
            Ok((root_page_id, verification))
        })();
        let (root_page_id, verification) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = std::fs::remove_file(&temp_path);
                return Err(error);
            }
        };
        publish_database(&temp_path, filepath, sync_parent_directory)?;
        invalidate_sorted_index(filepath)?;
        self.dirty = false;
        Ok(StoredDatabase { root_page_id, verification })
    }

    pub fn store_with_index<SortKey, F>(&mut self, filepath: &Path, sort_key_extractor: F) -> io::Result<u64>
    where
        SortKey: Ord + Serialize,
        F: Fn(&V) -> SortKey,
    {
        self.store_with_index_result(filepath, sort_key_extractor)
            .map(|stored| stored.map_or(0, |stored| stored.root_page_id))
    }

    pub(crate) fn store_with_index_verified<SortKey, F>(
        &mut self,
        filepath: &Path,
        sort_key_extractor: F,
    ) -> io::Result<VerificationReport>
    where
        SortKey: Ord + Serialize,
        F: Fn(&V) -> SortKey,
    {
        self.store_with_index_result(filepath, sort_key_extractor)?
            .map(|stored| stored.verification)
            .ok_or_else(|| invalid_input("verified indexed store requires a dirty tree"))
    }

    fn store_with_index_result<SortKey, F>(
        &mut self,
        filepath: &Path,
        sort_key_extractor: F,
    ) -> io::Result<Option<StoredDatabase>>
    where
        SortKey: Ord + Serialize,
        F: Fn(&V) -> SortKey,
    {
        with_exclusive_sidecar(filepath, || {
            recover_pending_under_existing_lock(filepath)?;
            if !self.dirty {
                return Ok(None);
            }
            let stored = self.store_exclusive(filepath)?;
            Self::store_index_exclusive(filepath, sort_key_extractor, stored.verification.live_entries)?;
            Ok(Some(stored))
        })
    }

    fn store_index_exclusive<SortKey, F>(
        filepath: &Path,
        sort_key_extractor: F,
        expected_entries: u64,
    ) -> io::Result<()>
    where
        SortKey: Ord + Serialize,
        F: Fn(&V) -> SortKey,
    {
        let mut query = BPlusTreeQuery::<K, V>::from_file_unlocked(File::open(filepath)?)?;
        let (database_id, generation) = query.snapshot_identity();
        let mut entries = query
            .collect_with_locators()?
            .into_iter()
            .map(|(key, value, locator)| (sort_key_extractor(&value), key, locator))
            .collect::<Vec<_>>();
        if u64::try_from(entries.len()).map_err(|_| invalid_data("sorted-index entry count exceeds u64"))?
            != expected_entries
        {
            return Err(invalid_data("sorted-index source entry count differs from verified database"));
        }
        entries.sort_unstable_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
        drop(query);

        let index_path = crate::repository::storage::get_file_path_for_db_index(filepath);
        let temporary = temporary_path(&index_path)?;
        let prepared = (|| {
            let mut writer = crate::repository::bplustree::sorted_index::v4::Writer::new(&temporary, database_id, generation)?;
            for (sort_key, primary_key, locator) in &entries {
                writer.push(sort_key, primary_key, *locator)?;
            }
            let _ = writer.finish()?;
            Ok(())
        })();
        if let Err(error) = prepared {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
        publish_database(&temporary, &index_path, sync_parent_directory)
    }

}

struct StoredDatabase {
    root_page_id: u64,
    verification: VerificationReport,
}

impl<K, V> BPlusTree<K, V>
where
    K: Ord + for<'de> Deserialize<'de>,
    V: for<'de> Deserialize<'de>,
{
    pub fn load(filepath: &Path) -> io::Result<Self> {
        let mut query = BPlusTreeQuery::<K, V>::try_new(filepath)?;
        let metadata = query.header.metadata.clone();
        let mut entries = BTreeMap::new();
        for entry in query.iter() {
            let (key, value) = entry?;
            let _ = entries.insert(key, value);
        }
        Ok(Self { entries, metadata, dirty: false })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlushPolicy {
    Immediate,
    Batch,
}

struct DatabaseImage {
    mmap: Option<Mmap>,
    fallback: Vec<u8>,
}

impl DatabaseImage {
    fn open(path: &Path) -> io::Result<(Self, DatabaseHeader)> {
        let mut file = File::open(path)?;
        let file_len = usize::try_from(file.metadata()?.len())
            .map_err(|_| invalid_data("database length exceeds usize"))?;
        let mmap = mmap_with_advice(&file, Advice::Normal, "v3 B+Tree update");
        let mut fallback = Vec::new();
        if mmap.is_none() {
            fallback
                .try_reserve_exact(file_len)
                .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
            file.read_to_end(&mut fallback)?;
        }
        let image = Self { mmap, fallback };
        let bytes = image.as_slice();
        let header = DatabaseHeader::decode(
            bytes.get(..PAGE_SIZE).ok_or_else(|| invalid_data("database header is truncated"))?,
        )?;
        let expected = usize::try_from(header.next_page_id)
            .map_err(|_| invalid_data("next page id exceeds usize"))?
            .checked_mul(PAGE_SIZE)
            .ok_or_else(|| invalid_data("database length overflow"))?;
        if bytes.len() != expected {
            return Err(invalid_data("database file length does not match header"));
        }
        Ok((image, header))
    }

    fn as_slice(&self) -> &[u8] { self.mmap.as_deref().unwrap_or(&self.fallback) }
}

struct WriteTransaction {
    original_header: DatabaseHeader,
    next_header: DatabaseHeader,
    original_file_len: u64,
    dirty_pages: BTreeMap<u64, Box<[u8; PAGE_SIZE]>>,
    allocated_pages: HashSet<u64>,
    freed_pages: HashSet<u64>,
}

impl WriteTransaction {
    fn new(header: DatabaseHeader, original_file_len: usize) -> io::Result<Self> {
        Ok(Self {
            original_header: header.clone(),
            next_header: header,
            original_file_len: u64::try_from(original_file_len)
                .map_err(|_| invalid_data("database length exceeds u64"))?,
            dirty_pages: BTreeMap::new(),
            allocated_pages: HashSet::new(),
            freed_pages: HashSet::new(),
        })
    }

    fn page<'a>(&'a self, base: &'a [u8], page_id: u64) -> io::Result<&'a [u8]> {
        if page_id == 0 || page_id >= self.next_header.next_page_id {
            return Err(invalid_data("transaction page id is outside database"));
        }
        if let Some(page) = self.dirty_pages.get(&page_id) {
            return Ok(page.as_slice());
        }
        if page_id >= self.original_header.next_page_id {
            return Err(invalid_data("transaction appended page is missing"));
        }
        let range = page_byte_range(page_id, base.len())?;
        base.get(range).ok_or_else(|| invalid_data("transaction base page is truncated"))
    }

    fn page_copy(&self, base: &[u8], page_id: u64) -> io::Result<[u8; PAGE_SIZE]> {
        let mut copied = [0; PAGE_SIZE];
        copied.copy_from_slice(self.page(base, page_id)?);
        Ok(copied)
    }

    fn page_mut<'a>(&'a mut self, base: &[u8], page_id: u64) -> io::Result<&'a mut [u8; PAGE_SIZE]> {
        if page_id == 0 || page_id >= self.next_header.next_page_id {
            return Err(invalid_data("transaction page id is outside database"));
        }
        if self.dirty_pages.contains_key(&page_id) {
            return self
                .dirty_pages
                .get_mut(&page_id)
                .map(Box::as_mut)
                .ok_or_else(|| invalid_data("transaction page copy is missing"));
        }
        if page_id >= self.original_header.next_page_id {
            return Err(invalid_data("appended transaction page is missing"));
        }
        let range = page_byte_range(page_id, base.len())?;
        let source = base.get(range).ok_or_else(|| invalid_data("transaction base page is truncated"))?;
        let mut copied = Box::new([0; PAGE_SIZE]);
        copied.copy_from_slice(source);
        let _ = self.dirty_pages.insert(page_id, copied);
        self.dirty_pages
            .get_mut(&page_id)
            .map(Box::as_mut)
            .ok_or_else(|| invalid_data("transaction page copy is missing"))
    }

    #[allow(clippy::large_types_passed_by_value)]
    fn write_page(&mut self, page_id: u64, page: [u8; PAGE_SIZE]) -> io::Result<()> {
        if page_id == 0 || page_id >= self.next_header.next_page_id {
            return Err(invalid_input("written page id is outside transaction bounds"));
        }
        let _ = self.dirty_pages.insert(page_id, Box::new(page));
        Ok(())
    }

    fn allocate_page(&mut self, base: &[u8]) -> io::Result<u64> {
        if self.next_header.free_page_head != 0 {
            let page_id = self.next_header.free_page_head;
            let page = SlottedPage::open(self.page(base, page_id)?, page_id, self.next_header.next_page_id)?;
            if page.header().page_type != PageType::Free {
                return Err(invalid_data("free list references a non-free page"));
            }
            self.next_header.free_page_head = page.header().right;
            if !self.allocated_pages.insert(page_id) {
                return Err(invalid_data("free page was allocated twice"));
            }
            let _ = self.freed_pages.remove(&page_id);
            return Ok(page_id);
        }
        let page_id = self.next_header.next_page_id;
        self.next_header.next_page_id = page_id
            .checked_add(1)
            .ok_or_else(|| invalid_input("next page id overflow"))?;
        if !self.allocated_pages.insert(page_id) {
            return Err(invalid_data("appended page was allocated twice"));
        }
        Ok(page_id)
    }

    fn free_page(&mut self, page_id: u64) -> io::Result<()> {
        if page_id == 0 || page_id == self.next_header.root_page_id || page_id >= self.next_header.next_page_id {
            return Err(invalid_input("page cannot be added to the free list"));
        }
        if !self.freed_pages.insert(page_id) {
            return Err(invalid_data("page was freed twice in one transaction"));
        }
        let _ = self.allocated_pages.remove(&page_id);
        let page = encode_free_page(page_id, self.next_header.next_page_id, self.next_header.free_page_head)?;
        self.next_header.free_page_head = page_id;
        self.write_page(page_id, page)
    }

    fn has_changes(&self) -> bool {
        !self.dirty_pages.is_empty() || self.next_header.metadata != self.original_header.metadata
    }

    fn prepared_pages(&mut self) -> io::Result<Vec<(u64, &[u8; PAGE_SIZE])>> {
        if !self.has_changes() {
            return Ok(Vec::new());
        }
        self.next_header.generation = self
            .original_header
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid_input("database generation overflow"))?;
        let _ = self.dirty_pages.insert(0, Box::new(self.next_header.encode()?));
        let mut prepared = Vec::new();
        prepared
            .try_reserve_exact(self.dirty_pages.len())
            .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
        for (page_id, page) in &self.dirty_pages {
            if *page_id == 0 {
                DatabaseHeader::decode(page.as_slice())?;
            } else {
                SlottedPage::open(page.as_slice(), *page_id, self.next_header.next_page_id)?;
            }
            prepared.push((*page_id, page.as_ref()));
        }
        Ok(prepared)
    }
}

fn page_byte_range(page_id: u64, database_len: usize) -> io::Result<std::ops::Range<usize>> {
    let start = usize::try_from(page_id)
        .map_err(|_| invalid_data("page id exceeds usize"))?
        .checked_mul(PAGE_SIZE)
        .ok_or_else(|| invalid_data("page offset overflow"))?;
    let end = start.checked_add(PAGE_SIZE).ok_or_else(|| invalid_data("page end overflow"))?;
    if end > database_len {
        return Err(invalid_data("page is outside database"));
    }
    Ok(start..end)
}

struct ActiveBatch {
    base: DatabaseImage,
    transaction: WriteTransaction,
    _guard: ExclusiveSidecarGuard,
}

#[derive(Default)]
struct WriteScratch {
    key: Vec<u8>,
    value: Vec<u8>,
    compression: Vec<u8>,
    cell: Vec<u8>,
    read_value: Vec<u8>,
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

impl<K, V> BPlusTreeUpdate<K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de>,
{
    pub fn try_new(filepath: &Path) -> io::Result<Self> {
        let mut query = BPlusTreeQuery::<K, V>::try_new(filepath)?;
        let _ = verify_full(&mut query)?;
        let database_id = query.header.database_id;
        let verified_generation = query.header.generation;
        let verified_next_page_id = query.header.next_page_id;
        drop(query);
        Ok(Self {
            filepath: filepath.to_path_buf(),
            database_id,
            verified_generation,
            verified_next_page_id,
            flush_policy: FlushPolicy::Immediate,
            active: None,
            scratch: WriteScratch::default(),
            _types: PhantomData,
        })
    }

    pub fn try_new_with_backoff(filepath: &Path) -> io::Result<Self> { Self::try_new(filepath) }

    pub fn try_new_with_backoff_stats(filepath: &Path) -> io::Result<(Self, u64)> {
        Self::try_new(filepath).map(|updater| (updater, 0))
    }

    pub fn set_flush_policy(&mut self, policy: FlushPolicy) { self.flush_policy = policy; }

    fn ensure_transaction(&mut self) -> io::Result<()> {
        if self.active.is_some() {
            return Ok(());
        }
        let guard = ExclusiveSidecarGuard::acquire(&self.filepath)?;
        recover_pending_under_existing_lock(&self.filepath)?;
        let (base, header) = DatabaseImage::open(&self.filepath)?;
        if header.database_id != self.database_id
            || header.generation != self.verified_generation
            || header.next_page_id != self.verified_next_page_id
        {
            let mut query = BPlusTreeQuery::<K, V>::from_file_unlocked(File::open(&self.filepath)?)?;
            let _ = verify_full(&mut query)?;
            self.database_id = query.header.database_id;
            self.verified_generation = query.header.generation;
            self.verified_next_page_id = query.header.next_page_id;
        }
        let transaction = WriteTransaction::new(header, base.as_slice().len())?;
        self.active = Some(ActiveBatch { base, transaction, _guard: guard });
        Ok(())
    }

    pub fn update(&mut self, key: &K, value: V) -> Result<u64, BPlusTreeError> {
        self.upsert(key, &value).map_err(Into::into)
    }

    pub fn update_batch(&mut self, items: &[(&K, &V)]) -> Result<u64, BPlusTreeError> {
        self.upsert_batch(items).map_err(Into::into)
    }

    pub fn prepare_upsert_batch(items: &[(&K, &V)]) -> io::Result<Vec<(K, Vec<u8>)>> {
        let mut prepared = Vec::new();
        prepared
            .try_reserve_exact(items.len())
            .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
        for (key, value) in items {
            prepared.push(((*key).clone(), binary_serialize(value)?));
        }
        prepared.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(prepared)
    }

    pub fn upsert_batch_prepared_with_backoff(filepath: &Path, items: &[(&K, &V)]) -> io::Result<u64> {
        let prepared = Self::prepare_upsert_batch(items)?;
        let mut updater = Self::try_new_with_backoff(filepath)?;
        updater.upsert_batch_encoded(prepared)
    }

    pub fn upsert_batch(&mut self, items: &[(&K, &V)]) -> io::Result<u64> {
        let prepared = match Self::prepare_upsert_batch(items) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.active = None;
                return Err(error);
            }
        };
        self.upsert_batch_encoded(prepared)
    }

    pub fn upsert_batch_encoded(&mut self, items: Vec<(K, Vec<u8>)>) -> io::Result<u64> {
        if items.is_empty() {
            return self.upsert_batch(&[]);
        }
        let policy = self.flush_policy;
        self.flush_policy = FlushPolicy::Batch;
        let mut root = 0;
        for (key, value) in items {
            match self.upsert_serialized(&key, &value) {
                Ok(next_root) => root = next_root,
                Err(error) => {
                    self.active = None;
                    self.flush_policy = policy;
                    return Err(error);
                }
            }
        }
        self.flush_policy = policy;
        if policy == FlushPolicy::Immediate {
            self.commit()?;
        }
        Ok(root)
    }

    pub fn upsert(&mut self, key: &K, value: &V) -> io::Result<u64> {
        let result = self.upsert_inner(key, value);
        if result.is_err() {
            self.active = None;
        }
        result
    }

    fn upsert_inner(&mut self, key: &K, value: &V) -> io::Result<u64> {
        self.scratch.key.clear();
        binary_serialize_into(&mut self.scratch.key, key)?;
        self.scratch.value.clear();
        binary_serialize_into(&mut self.scratch.value, value)?;
        self.ensure_transaction()?;
        let stored = match encode_value(&self.scratch.value, &mut self.scratch.compression) {
            Ok(stored) => stored,
            Err(error) => {
                self.active = None;
                return Err(error);
            }
        };
        let active = self.active.as_mut().ok_or_else(|| invalid_data("write transaction is missing"))?;
        let staged = stage_upsert::<K>(
            &mut active.transaction,
            active.base.as_slice(),
            key,
            &self.scratch.key,
            u32::try_from(self.scratch.value.len()).map_err(|_| invalid_input("serialized value exceeds u32"))?,
            stored.compression(),
            stored.as_slice(),
            &mut self.scratch.cell,
        );
        if let Err(error) = staged {
            self.active = None;
            return Err(error);
        }
        let root = self
            .active
            .as_ref()
            .ok_or_else(|| invalid_data("write transaction is missing"))?
            .transaction
            .next_header
            .root_page_id;
        if self.flush_policy == FlushPolicy::Immediate {
            self.commit()?;
        }
        Ok(root)
    }

    fn upsert_serialized(&mut self, key: &K, raw_value: &[u8]) -> io::Result<u64> {
        let result = self.upsert_serialized_inner(key, raw_value);
        if result.is_err() {
            self.active = None;
        }
        result
    }

    fn upsert_serialized_inner(&mut self, key: &K, raw_value: &[u8]) -> io::Result<u64> {
        self.scratch.key.clear();
        binary_serialize_into(&mut self.scratch.key, key)?;
        self.ensure_transaction()?;
        let stored = match encode_value(raw_value, &mut self.scratch.compression) {
            Ok(stored) => stored,
            Err(error) => {
                self.active = None;
                return Err(error);
            }
        };
        let active = self.active.as_mut().ok_or_else(|| invalid_data("write transaction is missing"))?;
        let staged = stage_upsert::<K>(
            &mut active.transaction,
            active.base.as_slice(),
            key,
            &self.scratch.key,
            u32::try_from(raw_value.len()).map_err(|_| invalid_input("serialized value exceeds u32"))?,
            stored.compression(),
            stored.as_slice(),
            &mut self.scratch.cell,
        );
        if let Err(error) = staged {
            self.active = None;
            return Err(error);
        }
        let root = self
            .active
            .as_ref()
            .ok_or_else(|| invalid_data("write transaction is missing"))?
            .transaction
            .next_header
            .root_page_id;
        if self.flush_policy == FlushPolicy::Immediate {
            self.commit()?;
        }
        Ok(root)
    }

    pub fn delete(&mut self, key: &K) -> io::Result<bool> {
        let result = self.delete_inner(key);
        if result.is_err() {
            self.active = None;
        }
        result
    }

    fn delete_inner(&mut self, key: &K) -> io::Result<bool> {
        let started = self.active.is_none();
        self.scratch.key.clear();
        binary_serialize_into(&mut self.scratch.key, key)?;
        self.ensure_transaction()?;
        let active = self.active.as_mut().ok_or_else(|| invalid_data("write transaction is missing"))?;
        let deleted = stage_delete::<K>(
            &mut active.transaction,
            active.base.as_slice(),
            key,
            &self.scratch.key,
            &mut self.scratch.cell,
        );
        let deleted = match deleted {
            Ok(deleted) => deleted,
            Err(error) => {
                self.active = None;
                return Err(error);
            }
        };
        if !deleted && started {
            self.active = None;
        } else if deleted && self.flush_policy == FlushPolicy::Immediate {
            self.commit()?;
        }
        Ok(deleted)
    }

    pub fn delete_batch(&mut self, keys: &[&K]) -> io::Result<usize> {
        if keys.is_empty() {
            return Ok(0);
        }
        let policy = self.flush_policy;
        self.flush_policy = FlushPolicy::Batch;
        let mut deleted = 0usize;
        let mut ordered = Vec::new();
        ordered
            .try_reserve_exact(keys.len())
            .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
        ordered.extend_from_slice(keys);
        ordered.sort();
        for key in ordered {
            match self.delete(key) {
                Ok(true) => deleted = deleted.checked_add(1).ok_or_else(|| invalid_input("delete count overflow"))?,
                Ok(false) => {}
                Err(error) => {
                    self.active = None;
                    self.flush_policy = policy;
                    return Err(error);
                }
            }
        }
        self.flush_policy = policy;
        if policy == FlushPolicy::Immediate {
            self.commit()?;
        }
        Ok(deleted)
    }

    pub fn get_metadata(&self) -> io::Result<BPlusTreeMetadata> {
        if let Some(active) = &self.active {
            return Ok(active.transaction.next_header.metadata.clone());
        }
        BPlusTreeQuery::<K, V>::try_new(&self.filepath).map(|query| query.header.metadata)
    }

    pub fn set_metadata(&mut self, metadata: &BPlusTreeMetadata) -> io::Result<()> {
        let started = self.active.is_none();
        self.ensure_transaction()?;
        let active = self.active.as_mut().ok_or_else(|| invalid_data("write transaction is missing"))?;
        if active.transaction.next_header.metadata == *metadata {
            if started {
                self.active = None;
            }
            return Ok(());
        }
        active.transaction.next_header.metadata = metadata.clone();
        if self.flush_policy == FlushPolicy::Immediate {
            self.commit()?;
        }
        Ok(())
    }

    pub fn query(&mut self, key: &K) -> Result<Option<V>, BPlusTreeError> {
        if let Some(active) = &mut self.active {
            return query_transaction::<K, V>(
                &active.transaction,
                active.base.as_slice(),
                key,
                &mut self.scratch.read_value,
            )
            .map_err(Into::into);
        }
        BPlusTreeQuery::<K, V>::try_new(&self.filepath)
            .and_then(|mut query| query.query_io(key))
            .map_err(Into::into)
    }

    pub fn commit(&mut self) -> io::Result<()> {
        let Some(mut active) = self.active.take() else { return Ok(()) };
        validate_transaction_links::<K>(&active.transaction, active.base.as_slice())?;
        let prepared = active.transaction.prepared_pages()?;
        if prepared.is_empty() {
            return Ok(());
        }
        let committed_header = DatabaseHeader::decode(prepared[0].1)?;
        let committed_generation = committed_header.generation;
        let committed_next_page_id = committed_header.next_page_id;
        let result = commit_ordered_page_refs_under_existing_lock(&self.filepath, &prepared);
        match result {
            Ok(()) => {
                self.verified_generation = committed_generation;
                self.verified_next_page_id = committed_next_page_id;
                Ok(())
            }
            Err(error) => {
                let outcome = error
                    .get_ref()
                    .and_then(|source| source.downcast_ref::<WalOperationError>())
                    .map(WalOperationError::outcome);
                if outcome == Some(WalOutcome::CommittedCleanupPending) {
                    self.verified_generation = committed_generation;
                    self.verified_next_page_id = committed_next_page_id;
                }
                if let Err(recovery_error) = recover_pending_under_existing_lock(&self.filepath) {
                    log::error!(
                        "B+Tree commit failed and recovery remains pending for {}: {recovery_error}",
                        self.filepath.display()
                    );
                }
                Err(error)
            }
        }
    }

    pub fn compact(&mut self) -> io::Result<()> {
        self.commit()?;
        let filepath = self.filepath.clone();
        let header = with_exclusive_sidecar(&filepath, || {
            recover_pending_under_existing_lock(&filepath)?;
            let mut query = BPlusTreeQuery::<K, V>::from_file_unlocked(File::open(&filepath)?)?;
            let _ = verify_full(&mut query)?;
            let metadata = query.header.metadata.clone();
            let mut entries = BTreeMap::new();
            for entry in query.iter() {
                let (key, value) = entry?;
                let _ = entries.insert(key, value);
            }
            drop(query);

            let mut replacement = BPlusTree { entries, metadata, dirty: true };
            let _ = replacement.store_exclusive(&filepath)?;
            let file = File::open(&filepath)?;
            let mut page = [0; PAGE_SIZE];
            read_exact_at_offset(&file, &mut page, 0)?;
            DatabaseHeader::decode(&page)
        })?;
        self.database_id = header.database_id;
        self.verified_generation = header.generation;
        self.verified_next_page_id = header.next_page_id;
        Ok(())
    }
}

pub struct BPlusTreeSerialWriter<K, V> {
    updater: Arc<Mutex<BPlusTreeUpdate<K, V>>>,
    flush_policy: FlushPolicy,
    dirty: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    background_error: Arc<Mutex<Option<io::Error>>>,
    background_handle: Mutex<Option<JoinHandle<()>>>,
}

impl<K, V> BPlusTreeSerialWriter<K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone + Send + 'static,
    V: Serialize + for<'de> Deserialize<'de> + Send + 'static,
{
    pub fn new(filepath: &Path, flush_policy: FlushPolicy) -> io::Result<Self> {
        let mut updater = BPlusTreeUpdate::try_new_with_backoff(filepath)?;
        updater.set_flush_policy(flush_policy);
        Ok(Self {
            updater: Arc::new(Mutex::new(updater)),
            flush_policy,
            dirty: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(AtomicBool::new(false)),
            background_error: Arc::new(Mutex::new(None)),
            background_handle: Mutex::new(None),
        })
    }

    pub fn upsert_prepared(&self, items: Vec<(K, Vec<u8>)>) -> io::Result<u64> {
        let result = self.updater.lock().upsert_batch_encoded(items);
        if result.is_ok() {
            self.dirty.store(self.flush_policy == FlushPolicy::Batch, Ordering::Release);
        }
        result
    }

    pub fn upsert(&self, items: &[(&K, &V)]) -> io::Result<u64> {
        self.upsert_prepared(BPlusTreeUpdate::<K, V>::prepare_upsert_batch(items)?)
    }

    pub fn start_background_commit(&self, interval: Duration) -> io::Result<()> {
        if self.flush_policy != FlushPolicy::Batch {
            return Err(invalid_input("background commit requires batch flush policy"));
        }
        if interval.is_zero() {
            return Err(invalid_input("background commit interval must be greater than zero"));
        }
        let mut slot = self.background_handle.lock();
        if slot.is_some() {
            return Ok(());
        }
        self.shutdown.store(false, Ordering::Release);
        let updater = Arc::clone(&self.updater);
        let dirty = Arc::clone(&self.dirty);
        let shutdown = Arc::clone(&self.shutdown);
        let background_error = Arc::clone(&self.background_error);
        *slot = Some(
            std::thread::Builder::new()
                .name(String::from("bplustree-commit"))
                .spawn(move || {
                    while !shutdown.load(Ordering::Acquire) {
                        std::thread::park_timeout(interval);
                        if shutdown.load(Ordering::Acquire) {
                            break;
                        }
                        commit_if_dirty(&updater, &dirty, &background_error);
                    }
                    commit_if_dirty(&updater, &dirty, &background_error);
                })
                .map_err(io::Error::other)?,
        );
        Ok(())
    }

    pub fn stop_background_commit(&self) -> io::Result<()> {
        self.shutdown.store(true, Ordering::Release);
        if let Some(handle) = self.background_handle.lock().take() {
            handle.thread().unpark();
            handle.join().map_err(|_| io::Error::other("background B+Tree commit thread panicked"))?;
        }
        if let Some(error) = self.background_error.lock().take() {
            return Err(error);
        }
        Ok(())
    }

    pub fn flush_now(&self) -> io::Result<()> { self.commit() }

    pub fn commit(&self) -> io::Result<()> {
        self.updater.lock().commit()?;
        self.dirty.store(false, Ordering::Release);
        Ok(())
    }

    pub fn shutdown(&self) -> io::Result<()> {
        self.stop_background_commit()?;
        self.commit()
    }
}

fn commit_if_dirty<K, V>(
    updater: &Mutex<BPlusTreeUpdate<K, V>>,
    dirty: &AtomicBool,
    background_error: &Mutex<Option<io::Error>>,
) where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de>,
{
    if !dirty.swap(false, Ordering::AcqRel) {
        return;
    }
    if let Err(error) = updater.lock().commit() {
        dirty.store(true, Ordering::Release);
        log::error!("Background B+Tree commit failed: {error}");
        *background_error.lock() = Some(error);
    }
}

impl<K, V> Drop for BPlusTreeSerialWriter<K, V> {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if self.dirty.load(Ordering::Acquire) {
            log::warn!("Dropping dirty B+Tree serial writer without an explicit shutdown");
        }
        if let Some(handle) = self.background_handle.lock().take() {
            handle.thread().unpark();
            drop(handle);
        }
    }
}

fn minimum_tree_key(
    transaction: &WriteTransaction,
    base: &[u8],
    mut page_id: u64,
) -> io::Result<Option<Vec<u8>>> {
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(page_id) {
            return Err(invalid_data("minimum-key descent contains a cycle"));
        }
        let page = SlottedPage::open(transaction.page(base, page_id)?, page_id, transaction.next_header.next_page_id)?;
        match page.header().page_type {
            PageType::Leaf => {
                let Some(cell) = page.cells().next() else { return Ok(None) };
                return LeafCellRef::decode(cell?, page_id, transaction.next_header.next_page_id)
                    .map(|cell| Some(cell.key_bytes.to_vec()));
            }
            PageType::Internal => {
                page_id = InternalPreamble::decode(page.as_bytes(), page_id, transaction.next_header.next_page_id)?
                    .leftmost_child;
            }
            PageType::Overflow | PageType::Free => return Err(invalid_data("tree references a non-tree page")),
        }
    }
}

fn validate_leaf_backlink(
    transaction: &WriteTransaction,
    base: &[u8],
    page_id: u64,
    sibling_id: u64,
    sibling_points_left: bool,
) -> io::Result<()> {
    if sibling_id == 0 {
        return Ok(());
    }
    let sibling = SlottedPage::open(
        transaction.page(base, sibling_id)?,
        sibling_id,
        transaction.next_header.next_page_id,
    )?;
    let backlink = if sibling_points_left { sibling.header().left } else { sibling.header().right };
    if sibling.header().page_type != PageType::Leaf || backlink != page_id {
        return Err(invalid_data("asymmetric leaf sibling link"));
    }
    Ok(())
}

fn validate_transaction_free_list(transaction: &WriteTransaction, base: &[u8]) -> io::Result<()> {
    let mut free = HashSet::new();
    let mut page_id = transaction.next_header.free_page_head;
    while page_id != 0 {
        if !free.insert(page_id) {
            return Err(invalid_data("free list contains a cycle"));
        }
        if transaction.allocated_pages.contains(&page_id) {
            return Err(invalid_data("allocated page remains linked from free list"));
        }
        let page = SlottedPage::open(transaction.page(base, page_id)?, page_id, transaction.next_header.next_page_id)?;
        if page.header().page_type != PageType::Free {
            return Err(invalid_data("free list references a non-free page"));
        }
        page_id = page.header().right;
    }
    if !transaction.freed_pages.iter().all(|page| free.contains(page)) {
        return Err(invalid_data("freed page is missing from free list"));
    }
    for page_id in &transaction.allocated_pages {
        let page = transaction.page(base, *page_id)?;
        if SlottedPage::open(page, *page_id, transaction.next_header.next_page_id)?.header().page_type == PageType::Free {
            return Err(invalid_data("allocated page still has free-page type"));
        }
    }
    Ok(())
}

fn validate_transaction_links<K>(transaction: &WriteTransaction, base: &[u8]) -> io::Result<()>
where
    K: Ord + for<'de> Deserialize<'de>,
{
    let original_length = transaction
        .original_header
        .next_page_id
        .checked_mul(u64::try_from(PAGE_SIZE).map_err(|_| invalid_data("page size exceeds u64"))?)
        .ok_or_else(|| invalid_data("original database length overflow"))?;
    if original_length != transaction.original_file_len {
        return Err(invalid_data("transaction original length does not match header"));
    }

    let mut owned_overflow_pages = HashSet::new();
    for (page_id, bytes) in &transaction.dirty_pages {
        if *page_id == 0 {
            continue;
        }
        let page = SlottedPage::open(bytes.as_slice(), *page_id, transaction.next_header.next_page_id)?;
        match page.header().page_type {
            PageType::Leaf => {
                validate_leaf_backlink(transaction, base, *page_id, page.header().left, false)?;
                validate_leaf_backlink(transaction, base, *page_id, page.header().right, true)?;
                let mut previous_key = None;
                for cell in page.cells() {
                    let cell = LeafCellRef::decode(cell?, *page_id, transaction.next_header.next_page_id)?;
                    let key = decode_key::<K>(cell.key_bytes)?;
                    if previous_key.as_ref().is_some_and(|previous| previous >= &key) {
                        return Err(invalid_data("leaf keys are not strictly increasing"));
                    }
                    previous_key = Some(key);
                    if let LeafValueRef::Overflow { stored_len, head, crc32, .. } = cell.value {
                        for overflow_page in
                            validated_overflow_chain_pages(transaction, base, head, stored_len, crc32)?
                        {
                            if !owned_overflow_pages.insert(overflow_page) {
                                return Err(invalid_data("overflow page is owned by multiple values"));
                            }
                        }
                    }
                }
            }
            PageType::Internal => {
                let preamble = InternalPreamble::decode(bytes.as_slice(), *page_id, transaction.next_header.next_page_id)?;
                let child = SlottedPage::open(
                    transaction.page(base, preamble.leftmost_child)?,
                    preamble.leftmost_child,
                    transaction.next_header.next_page_id,
                )?;
                if !matches!(child.header().page_type, PageType::Leaf | PageType::Internal) {
                    return Err(invalid_data("internal page references a non-tree child"));
                }
                let mut previous_key = None;
                for cell in page.cells() {
                    let cell = InternalCellRef::decode(cell?, *page_id, transaction.next_header.next_page_id)?;
                    let key = decode_key::<K>(cell.key_bytes)?;
                    if previous_key.as_ref().is_some_and(|previous| previous >= &key) {
                        return Err(invalid_data("internal separator keys are not strictly increasing"));
                    }
                    previous_key = Some(key);
                    let minimum = minimum_tree_key(transaction, base, cell.right_child)?
                        .ok_or_else(|| invalid_data("internal separator references an empty subtree"))?;
                    if minimum != cell.key_bytes {
                        return Err(invalid_data("internal separator differs from right subtree minimum"));
                    }
                }
            }
            PageType::Overflow => {
                if page.header().right != 0 {
                    let next = SlottedPage::open(
                        transaction.page(base, page.header().right)?,
                        page.header().right,
                        transaction.next_header.next_page_id,
                    )?;
                    if next.header().page_type != PageType::Overflow {
                        return Err(invalid_data("overflow page references a non-overflow page"));
                    }
                }
            }
            PageType::Free => {}
        }
    }

    validate_transaction_free_list(transaction, base)
}

#[derive(Debug)]
struct Promotion {
    key: Vec<u8>,
    right_child: u64,
}

fn internal_child_position<K: Ord + for<'de> Deserialize<'de>, B: AsRef<[u8]>>(
    page: &SlottedPage<B>,
    key: &K,
) -> io::Result<(usize, u64)> {
    let mut left = 0usize;
    let mut right = usize::from(page.header().cell_count);
    while left < right {
        let middle = left + (right - left) / 2;
        let cell = InternalCellRef::decode(page.cell(middle)?, page.page_id(), page.next_page_id())?;
        if decode_key::<K>(cell.key_bytes)? <= *key {
            left = middle + 1;
        } else {
            right = middle;
        }
    }
    let child = if left == 0 {
        InternalPreamble::decode(page.as_bytes(), page.page_id(), page.next_page_id())?.leftmost_child
    } else {
        InternalCellRef::decode(page.cell(left - 1)?, page.page_id(), page.next_page_id())?.right_child
    };
    Ok((left, child))
}

fn locate_transaction_leaf<K: Ord + for<'de> Deserialize<'de>>(
    transaction: &WriteTransaction,
    base: &[u8],
    key: &K,
) -> io::Result<(u64, Vec<(u64, usize)>)> {
    let mut page_id = transaction.next_header.root_page_id;
    let mut path = Vec::new();
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(page_id) {
            return Err(invalid_data("tree descent contains a cycle"));
        }
        let page = SlottedPage::open(transaction.page(base, page_id)?, page_id, transaction.next_header.next_page_id)?;
        match page.header().page_type {
            PageType::Leaf => return Ok((page_id, path)),
            PageType::Internal => {
                let (position, child) = internal_child_position(&page, key)?;
                path.try_reserve(1)
                    .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
                path.push((page_id, position));
                page_id = child;
            }
            PageType::Overflow | PageType::Free => return Err(invalid_data("tree references a non-tree page")),
        }
    }
}

fn overflow_chain_pages(
    transaction: &WriteTransaction,
    base: &[u8],
    mut page_id: u64,
) -> io::Result<Vec<u64>> {
    let mut pages = Vec::new();
    let mut visited = HashSet::new();
    while page_id != 0 {
        if !visited.insert(page_id) {
            return Err(invalid_data("overflow chain contains a cycle"));
        }
        let page = SlottedPage::open(transaction.page(base, page_id)?, page_id, transaction.next_header.next_page_id)?;
        if page.header().page_type != PageType::Overflow || overflow_payload(&page)?.is_empty() {
            return Err(invalid_data("invalid overflow chain page"));
        }
        pages
            .try_reserve(1)
            .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
        pages.push(page_id);
        page_id = page.header().right;
    }
    if pages.is_empty() {
        return Err(invalid_data("overflow chain is empty"));
    }
    Ok(pages)
}

fn validated_overflow_chain_pages(
    transaction: &WriteTransaction,
    base: &[u8],
    head: u64,
    stored_len: u32,
    crc32: u32,
) -> io::Result<Vec<u64>> {
    let pages = overflow_chain_pages(transaction, base, head)?;
    let expected = usize::try_from(stored_len).map_err(|_| invalid_data("stored length exceeds usize"))?;
    let mut actual = 0usize;
    let mut hasher = crc32fast::Hasher::new();
    for page_id in &pages {
        let page = SlottedPage::open(
            transaction.page(base, *page_id)?,
            *page_id,
            transaction.next_header.next_page_id,
        )?;
        let payload = overflow_payload(&page)?;
        actual = actual
            .checked_add(payload.len())
            .ok_or_else(|| invalid_data("overflow chain length overflow"))?;
        if actual > expected {
            return Err(invalid_data("overflow chain exceeds declared length"));
        }
        hasher.update(payload);
    }
    if actual != expected || hasher.finalize() != crc32 {
        return Err(invalid_data("overflow value checksum or length mismatch"));
    }
    Ok(pages)
}

fn free_pages(transaction: &mut WriteTransaction, pages: &[u64]) -> io::Result<()> {
    for page_id in pages.iter().rev() {
        transaction.free_page(*page_id)?;
    }
    Ok(())
}

fn write_overflow_chain(
    transaction: &mut WriteTransaction,
    base: &[u8],
    stored: &[u8],
    old_pages: &[u64],
) -> io::Result<u64> {
    let required = stored.len().div_ceil(OVERFLOW_PAYLOAD_LEN);
    if required == 0 {
        return Err(invalid_input("overflow value must not be empty"));
    }
    let mut pages = Vec::new();
    pages
        .try_reserve_exact(required)
        .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
    if old_pages.len() >= required {
        pages.extend_from_slice(
            old_pages
                .get(..required)
                .ok_or_else(|| invalid_data("overflow reuse range is invalid"))?,
        );
        free_pages(
            transaction,
            old_pages
                .get(required..)
                .ok_or_else(|| invalid_data("overflow tail range is invalid"))?,
        )?;
    } else {
        for _ in 0..required {
            pages.push(transaction.allocate_page(base)?);
        }
        free_pages(transaction, old_pages)?;
    }
    for (index, payload) in stored.chunks(OVERFLOW_PAYLOAD_LEN).enumerate() {
        let page_id = *pages.get(index).ok_or_else(|| invalid_data("overflow page allocation is missing"))?;
        let next = pages.get(index + 1).copied().unwrap_or(0);
        transaction.write_page(
            page_id,
            encode_overflow_page(page_id, transaction.next_header.next_page_id, next, payload)?,
        )?;
    }
    pages.first().copied().ok_or_else(|| invalid_data("overflow head is missing"))
}

fn repair_right_leaf_backlink(
    transaction: &mut WriteTransaction,
    base: &[u8],
    right_page_id: u64,
    left_page_id: u64,
) -> io::Result<()> {
    if right_page_id == 0 {
        return Ok(());
    }
    let next_page_id = transaction.next_header.next_page_id;
    let page = transaction.page_mut(base, right_page_id)?;
    let mut header = SlottedPage::open(page.as_slice(), right_page_id, next_page_id)?.header();
    if header.page_type != PageType::Leaf {
        return Err(invalid_data("leaf sibling references a non-leaf page"));
    }
    header.left = left_page_id;
    header.encode_into(page, right_page_id, next_page_id)
}

fn mutate_leaf(
    transaction: &mut WriteTransaction,
    base: &[u8],
    leaf_id: u64,
    index: usize,
    replace: bool,
    cell: &[u8],
) -> io::Result<Option<Promotion>> {
    let snapshot = transaction.page_copy(base, leaf_id)?;
    let page = SlottedPage::open(snapshot.as_slice(), leaf_id, transaction.next_header.next_page_id)?;
    if page.header().page_type != PageType::Leaf {
        return Err(invalid_data("mutation target is not a leaf page"));
    }
    let count = usize::from(page.header().cell_count);
    if (replace && index >= count) || (!replace && index > count) {
        return Err(invalid_input("leaf mutation index is outside page"));
    }
    if replace && page.cell(index)?.len() == cell.len() {
        let next_page_id = transaction.next_header.next_page_id;
        let dirty = transaction.page_mut(base, leaf_id)?;
        return SlottedPage::open(dirty.as_mut_slice(), leaf_id, next_page_id)?
            .replace_same_len(index, cell)
            .map(|()| None);
    }
    let mut cells = Vec::<&[u8]>::new();
    cells
        .try_reserve_exact(count + usize::from(!replace))
        .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
    for current in 0..count {
        if current == index {
            cells.push(cell);
            if replace {
                continue;
            }
        }
        cells.push(page.cell(current)?);
    }
    if index == count {
        cells.push(cell);
    }
    if used_leaf_bytes(&cells)? <= PAGE_SIZE {
        let next_page_id = transaction.next_header.next_page_id;
        let dirty = transaction.page_mut(base, leaf_id)?;
        SlottedPage::open(dirty.as_mut_slice(), leaf_id, next_page_id)?
            .rebuild_ordered(cells.iter().copied())?;
        return Ok(None);
    }

    let new_leaf_id = transaction.allocate_page(base)?;
    let boundary = choose_leaf_split(&cells)?;
    let (left_cells, right_cells) = cells.split_at(boundary);
    let right = page.header().right;
    let left_page = leaf_page(leaf_id, page.header().left, new_leaf_id, left_cells)?;
    let right_page = leaf_page(new_leaf_id, leaf_id, right, right_cells)?;
    let separator = LeafCellRef::decode(
        right_cells.first().ok_or_else(|| invalid_data("split right leaf is empty"))?,
        new_leaf_id,
        transaction.next_header.next_page_id,
    )?
    .key_bytes
    .to_vec();
    transaction.write_page(leaf_id, left_page)?;
    transaction.write_page(new_leaf_id, right_page)?;
    repair_right_leaf_backlink(transaction, base, right, new_leaf_id)?;
    Ok(Some(Promotion { key: separator, right_child: new_leaf_id }))
}

fn insert_internal_promotion(
    transaction: &mut WriteTransaction,
    base: &[u8],
    page_id: u64,
    position: usize,
    promotion: &Promotion,
    cell_scratch: &mut Vec<u8>,
) -> io::Result<Option<Promotion>> {
    let snapshot = transaction.page_copy(base, page_id)?;
    let page = SlottedPage::open(snapshot.as_slice(), page_id, transaction.next_header.next_page_id)?;
    if page.header().page_type != PageType::Internal {
        return Err(invalid_data("promotion target is not an internal page"));
    }
    let count = usize::from(page.header().cell_count);
    if position > count {
        return Err(invalid_input("internal insertion position is outside page"));
    }
    encode_internal_cell(
        &promotion.key,
        promotion.right_child,
        page_id,
        transaction.next_header.next_page_id,
        cell_scratch,
    )?;
    let mut cells = Vec::<&[u8]>::new();
    cells
        .try_reserve_exact(count + 1)
        .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
    for current in 0..count {
        if current == position {
            cells.push(cell_scratch);
        }
        cells.push(page.cell(current)?);
    }
    if position == count {
        cells.push(cell_scratch);
    }
    if used_internal_bytes(cells.iter().copied())? <= PAGE_SIZE {
        let next_page_id = transaction.next_header.next_page_id;
        let dirty = transaction.page_mut(base, page_id)?;
        SlottedPage::open(dirty.as_mut_slice(), page_id, next_page_id)?
            .rebuild_ordered(cells.iter().copied())?;
        return Ok(None);
    }

    let right_page_id = transaction.allocate_page(base)?;
    let split = choose_internal_split(&cells, page_id, transaction.next_header.next_page_id)?;
    let leftmost = InternalPreamble::decode(snapshot.as_slice(), page_id, transaction.next_header.next_page_id)?
        .leftmost_child;
    let promoted = Promotion { key: split.promoted.key_bytes.to_vec(), right_child: right_page_id };
    let left_page = internal_page(page_id, leftmost, split.left_cells)?;
    let right_page = internal_page(right_page_id, split.right_leftmost_child, split.right_cells)?;
    transaction.write_page(page_id, left_page)?;
    transaction.write_page(right_page_id, right_page)?;
    Ok(Some(promoted))
}

fn propagate_promotion(
    transaction: &mut WriteTransaction,
    base: &[u8],
    mut path: Vec<(u64, usize)>,
    mut promotion: Promotion,
    cell_scratch: &mut Vec<u8>,
) -> io::Result<()> {
    while let Some((parent, position)) = path.pop() {
        let Some(next) = insert_internal_promotion(transaction, base, parent, position, &promotion, cell_scratch)? else {
            return Ok(());
        };
        promotion = next;
    }
    let old_root = transaction.next_header.root_page_id;
    let new_root = transaction.allocate_page(base)?;
    encode_internal_cell(
        &promotion.key,
        promotion.right_child,
        new_root,
        transaction.next_header.next_page_id,
        cell_scratch,
    )?;
    transaction.write_page(new_root, internal_page(new_root, old_root, &[cell_scratch.as_slice()])?)?;
    transaction.next_header.root_page_id = new_root;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn stage_upsert<K: Ord + for<'de> Deserialize<'de>>(
    transaction: &mut WriteTransaction,
    base: &[u8],
    key: &K,
    encoded_key: &[u8],
    logical_len: u32,
    compression: Compression,
    stored: &[u8],
    cell_scratch: &mut Vec<u8>,
) -> io::Result<()> {
    let (leaf_id, path) = locate_transaction_leaf(transaction, base, key)?;
    let page = SlottedPage::open(transaction.page(base, leaf_id)?, leaf_id, transaction.next_header.next_page_id)?;
    let search = search_leaf(&page, key)?;
    let (replace, index, old_overflow) = match search {
        Ok(index) => {
            let old = LeafCellRef::decode(page.cell(index)?, leaf_id, transaction.next_header.next_page_id)?;
            let overflow = match old.value {
                LeafValueRef::Overflow { stored_len, head, crc32, .. } => {
                    validated_overflow_chain_pages(transaction, base, head, stored_len, crc32)?
                }
                LeafValueRef::Inline { .. } | LeafValueRef::Tombstone => Vec::new(),
            };
            (true, index, overflow)
        }
        Err(index) => (false, index, Vec::new()),
    };
    let inline_footprint = SLOT_LEN
        .checked_add(24)
        .and_then(|size| size.checked_add(encoded_key.len()))
        .and_then(|size| size.checked_add(stored.len()))
        .ok_or_else(|| invalid_input("leaf cell footprint overflow"))?;
    if stored.len() <= MAX_INLINE_STORED_VALUE && inline_footprint <= MAX_CELL_FOOTPRINT {
        free_pages(transaction, &old_overflow)?;
        encode_inline_leaf_cell(encoded_key, logical_len, compression, stored, cell_scratch)?;
    } else {
        let head = write_overflow_chain(transaction, base, stored, &old_overflow)?;
        encode_overflow_leaf_cell(
            encoded_key,
            logical_len,
            compression,
            u32::try_from(stored.len()).map_err(|_| invalid_input("stored value exceeds u32"))?,
            head,
            stored_value_checksum(stored),
            leaf_id,
            transaction.next_header.next_page_id,
            cell_scratch,
        )?;
    }
    if let Some(promotion) = mutate_leaf(transaction, base, leaf_id, index, replace, cell_scratch)? {
        propagate_promotion(transaction, base, path, promotion, cell_scratch)?;
    }
    Ok(())
}

fn stage_delete<K: Ord + for<'de> Deserialize<'de>>(
    transaction: &mut WriteTransaction,
    base: &[u8],
    key: &K,
    encoded_key: &[u8],
    cell_scratch: &mut Vec<u8>,
) -> io::Result<bool> {
    let (leaf_id, _) = locate_transaction_leaf(transaction, base, key)?;
    let page = SlottedPage::open(transaction.page(base, leaf_id)?, leaf_id, transaction.next_header.next_page_id)?;
    let Ok(index) = search_leaf(&page, key)? else { return Ok(false) };
    let old = LeafCellRef::decode(page.cell(index)?, leaf_id, transaction.next_header.next_page_id)?;
    match old.value {
        LeafValueRef::Tombstone => return Ok(false),
        LeafValueRef::Overflow { stored_len, head, crc32, .. } => {
            let pages = validated_overflow_chain_pages(transaction, base, head, stored_len, crc32)?;
            free_pages(transaction, &pages)?;
        }
        LeafValueRef::Inline { .. } => {}
    }
    encode_tombstone_leaf_cell(encoded_key, cell_scratch)?;
    let _ = mutate_leaf(transaction, base, leaf_id, index, true, cell_scratch)?;
    Ok(true)
}

fn query_transaction<K, V>(
    transaction: &WriteTransaction,
    base: &[u8],
    key: &K,
    scratch: &mut Vec<u8>,
) -> io::Result<Option<V>>
where
    K: Ord + for<'de> Deserialize<'de>,
    V: for<'de> Deserialize<'de>,
{
    let (leaf_id, _) = locate_transaction_leaf(transaction, base, key)?;
    let page = SlottedPage::open(transaction.page(base, leaf_id)?, leaf_id, transaction.next_header.next_page_id)?;
    let Ok(index) = search_leaf(&page, key)? else { return Ok(None) };
    let cell = LeafCellRef::decode(page.cell(index)?, leaf_id, transaction.next_header.next_page_id)?;
    let bytes = match cell.value {
        LeafValueRef::Tombstone => return Ok(None),
        LeafValueRef::Inline { compression: Compression::None, stored, .. } => stored,
        LeafValueRef::Inline { compression: Compression::Lz4, logical_len, stored, .. } => {
            decompress_value_into(stored, logical_len, transaction_value_limit(transaction)?, scratch)?
        }
        LeafValueRef::Overflow { compression, logical_len, stored_len, head, crc32 } => {
            read_transaction_overflow(
                transaction,
                base,
                compression,
                logical_len,
                stored_len,
                head,
                crc32,
                scratch,
            )?
        }
    };
    binary_deserialize(bytes).map(Some)
}

fn transaction_value_limit(transaction: &WriteTransaction) -> io::Result<usize> {
    usize::try_from(transaction.next_header.next_page_id)
        .map_err(|_| invalid_data("next page id exceeds usize"))?
        .checked_mul(PAGE_SIZE)
        .and_then(|size| size.checked_mul(256))
        .map(|size| size.min(usize::try_from(u32::MAX).unwrap_or(usize::MAX)))
        .ok_or_else(|| invalid_data("value allocation limit overflow"))
}

#[allow(clippy::too_many_arguments)]
fn read_transaction_overflow<'a>(
    transaction: &WriteTransaction,
    base: &[u8],
    compression: Compression,
    logical_len: u32,
    stored_len: u32,
    head: u64,
    crc32: u32,
    scratch: &'a mut Vec<u8>,
) -> io::Result<&'a [u8]> {
    let stored_len = usize::try_from(stored_len).map_err(|_| invalid_data("stored length exceeds usize"))?;
    if stored_len > transaction_value_limit(transaction)? {
        return Err(invalid_data("overflow value exceeds allocation limit"));
    }
    scratch.clear();
    scratch
        .try_reserve(stored_len)
        .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
    for page_id in overflow_chain_pages(transaction, base, head)? {
        let page = SlottedPage::open(transaction.page(base, page_id)?, page_id, transaction.next_header.next_page_id)?;
        let payload = overflow_payload(&page)?;
        if scratch.len().saturating_add(payload.len()) > stored_len {
            return Err(invalid_data("overflow chain exceeds declared length"));
        }
        scratch.extend_from_slice(payload);
    }
    if scratch.len() != stored_len || crc32fast::hash(scratch) != crc32 {
        return Err(invalid_data("overflow value checksum or length mismatch"));
    }
    if compression == Compression::Lz4 {
        decompress_value_in_place(scratch, logical_len, transaction_value_limit(transaction)?)?;
    } else if scratch.len() != usize::try_from(logical_len).map_err(|_| invalid_data("logical length exceeds usize"))? {
        return Err(invalid_data("uncompressed overflow length mismatch"));
    }
    Ok(scratch)
}

enum DecodedEntry<K, V> {
    Ready(K, V),
    Overflow(K, Compression, u32, u32, u64, u32),
    Tombstone,
}

enum DecodedValue<V> {
    Ready(V),
    Overflow(Compression, u32, u32, u64, u32),
    Tombstone,
}

struct InternalRoute<K> {
    leftmost_child: u64,
    separators: Vec<(K, u64)>,
}

impl<K: Ord> InternalRoute<K> {
    fn child_for(&self, target: &K) -> u64 {
        let index = self.separators.partition_point(|(key, _)| key <= target);
        index.checked_sub(1).map_or(self.leftmost_child, |previous| self.separators[previous].1)
    }
}

fn decode_internal_route<K, B>(page: &SlottedPage<B>) -> io::Result<InternalRoute<K>>
where
    K: Ord + for<'de> Deserialize<'de>,
    B: AsRef<[u8]>,
{
    let preamble = InternalPreamble::decode(page.as_bytes(), page.page_id(), page.next_page_id())?;
    let count = usize::from(page.header().cell_count);
    let mut separators = Vec::new();
    separators
        .try_reserve_exact(count)
        .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
    for index in 0..count {
        let cell = InternalCellRef::decode(page.cell(index)?, page.page_id(), page.next_page_id())?;
        separators.push((decode_internal_key(cell.key_bytes)?, cell.right_child));
    }
    Ok(InternalRoute { leftmost_child: preamble.leftmost_child, separators })
}

enum LocateCell<K> {
    Internal(InternalRoute<K>),
    Leaf(Option<Range<usize>>),
}

enum LocateLeaf<K> {
    Internal(InternalRoute<K>),
    Leaf,
}

struct QuerySnapshot<K> {
    file: Option<File>,
    mmap: Option<Mmap>,
    filepath: PathBuf,
    page_validations: Vec<OnceLock<PageValidation>>,
    internal_routes: Vec<OnceLock<InternalRoute<K>>>,
    sidecar_guard: Option<SharedSidecarGuard>,
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

impl<K, V> BPlusTreeQuery<K, V> {
    fn from_file_unlocked(file: File) -> io::Result<Self> {
        let file_len = usize::try_from(file.metadata()?.len()).map_err(|_| invalid_data("database length exceeds usize"))?;
        if file_len < PAGE_SIZE {
            return Err(invalid_data("database is shorter than its header page"));
        }
        let mut header_page = [0; PAGE_SIZE];
        read_exact_at_offset(&file, &mut header_page, 0)?;
        let header = DatabaseHeader::decode(&header_page)?;
        let expected = usize::try_from(header.next_page_id)
            .map_err(|_| invalid_data("next page id exceeds usize"))?
            .checked_mul(PAGE_SIZE)
            .ok_or_else(|| invalid_data("database length overflow"))?;
        if file_len != expected {
            return Err(invalid_data("database file length does not match header"));
        }
        let mmap = mmap_with_advice(&file, Advice::Normal, "v3 B+Tree query");
        let page_count = usize::try_from(header.next_page_id).map_err(|_| invalid_data("next page id exceeds usize"))?;
        let mut page_validations = Vec::new();
        page_validations
            .try_reserve_exact(page_count)
            .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
        let mut internal_routes = Vec::new();
        internal_routes
            .try_reserve_exact(page_count)
            .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
        for _ in 0..page_count {
            page_validations.push(OnceLock::new());
            internal_routes.push(OnceLock::new());
        }
        let file = mmap.is_none().then_some(file);
        let mut page_buffer = Vec::new();
        if mmap.is_none() {
            page_buffer
                .try_reserve_exact(PAGE_SIZE)
                .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
            page_buffer.resize(PAGE_SIZE, 0);
        }
        Ok(Self {
            snapshot: Arc::new(QuerySnapshot {
                file,
                mmap,
                filepath: PathBuf::new(),
                page_validations,
                internal_routes,
                sidecar_guard: None,
            }),
            header,
            file_len,
            page_buffer,
            value_scratch: Vec::new(),
            locator_page_id: 0,
            locator_cell_ranges: Vec::new(),
            _value: PhantomData,
        })
    }

    pub fn try_new(filepath: &Path) -> io::Result<Self> {
        loop {
            let sidecar_guard = SharedSidecarGuard::acquire(filepath)?;
            let pending = wal_path(filepath).try_exists()? || wal_temporary_path(filepath).try_exists()?;
            if !pending {
                let mut query = Self::from_file_unlocked(File::open(filepath)?)?;
                let snapshot = Arc::get_mut(&mut query.snapshot)
                    .ok_or_else(|| invalid_data("new query snapshot is unexpectedly shared"))?;
                snapshot.filepath = filepath.to_path_buf();
                snapshot.sidecar_guard = Some(sidecar_guard);
                return Ok(query);
            }
            drop(sidecar_guard);
            match recover_pending(filepath) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
                    ) =>
                {
                    return Err(recovery_required(filepath, error));
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        if self.snapshot.filepath.as_os_str().is_empty() {
            return Err(invalid_input("mapped query without a path cannot be cloned"));
        }
        let mut page_buffer = Vec::new();
        if self.snapshot.mmap.is_none() {
            page_buffer
                .try_reserve_exact(PAGE_SIZE)
                .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
            page_buffer.resize(PAGE_SIZE, 0);
        }
        Ok(Self {
            snapshot: Arc::clone(&self.snapshot),
            header: self.header.clone(),
            file_len: self.file_len,
            page_buffer,
            value_scratch: Vec::new(),
            locator_page_id: 0,
            locator_cell_ranges: Vec::new(),
            _value: PhantomData,
        })
    }

    #[cfg(test)]
    pub(crate) fn clone_error_fixture() -> Self {
        Self {
            snapshot: Arc::new(QuerySnapshot {
                file: None,
                mmap: None,
                filepath: PathBuf::new(),
                page_validations: Vec::new(),
                internal_routes: Vec::new(),
                sidecar_guard: None,
            }),
            header: DatabaseHeader {
                root_page_id: 1,
                next_page_id: 2,
                free_page_head: 0,
                generation: 1,
                database_id: [0; 16],
                metadata: BPlusTreeMetadata::Empty,
            },
            file_len: 0,
            page_buffer: Vec::new(),
            value_scratch: Vec::new(),
            locator_page_id: 0,
            locator_cell_ranges: Vec::new(),
            _value: PhantomData,
        }
    }

    pub fn filepath(&self) -> &Path { &self.snapshot.filepath }

    pub(crate) fn snapshot_identity(&self) -> ([u8; 16], u64) {
        (self.header.database_id, self.header.generation)
    }

    pub(crate) fn snapshot_metadata(&self) -> &BPlusTreeMetadata { &self.header.metadata }

    fn value_allocation_limit(&self) -> usize {
        self.file_len
            .saturating_mul(256)
            .min(usize::try_from(u32::MAX).unwrap_or(usize::MAX))
    }

    fn assemble_overflow_chain(
        &mut self,
        compression: Compression,
        logical_len: u32,
        stored_len: u32,
        mut page_id: u64,
        crc32: u32,
        mut owned_pages: Option<&mut HashSet<u64>>,
    ) -> io::Result<()> {
        let stored_len = usize::try_from(stored_len).map_err(|_| invalid_data("stored length exceeds usize"))?;
        let logical_len_usize =
            usize::try_from(logical_len).map_err(|_| invalid_data("logical length exceeds usize"))?;
        if stored_len > self.file_len || logical_len_usize > self.value_allocation_limit() {
            return Err(invalid_data("overflow value exceeds allocation limit"));
        }
        self.value_scratch.clear();
        self.value_scratch
            .try_reserve(stored_len)
            .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
        let mut chain = HashSet::new();
        while page_id != 0 {
            record_page_visit(&mut chain, page_id, "overflow chain contains a cycle")?;
            if let Some(pages) = owned_pages.as_deref_mut() {
                record_page_visit(pages, page_id, "overflow page is owned by multiple values")?;
            }
            page_id = self.with_slotted_page(page_id, |page, scratch| {
                if page.header().page_type != PageType::Overflow {
                    return Err(invalid_data("overflow chain references a non-overflow page"));
                }
                let payload = overflow_payload(page)?;
                if payload.is_empty() {
                    return Err(invalid_data("overflow chain contains an empty payload"));
                }
                let next_len = scratch
                    .len()
                    .checked_add(payload.len())
                    .ok_or_else(|| invalid_data("overflow value length overflow"))?;
                if next_len > stored_len {
                    return Err(invalid_data("overflow chain exceeds declared length"));
                }
                scratch.extend_from_slice(payload);
                Ok(page.header().right)
            })?;
        }
        if self.value_scratch.len() != stored_len || crc32fast::hash(&self.value_scratch) != crc32 {
            return Err(invalid_data("overflow value checksum or length mismatch"));
        }
        if compression == Compression::Lz4 {
            let allocation_limit = self.value_allocation_limit();
            decompress_value_in_place(&mut self.value_scratch, logical_len, allocation_limit)?;
        } else if self.value_scratch.len() != logical_len_usize {
            return Err(invalid_data("uncompressed overflow length mismatch"));
        }
        Ok(())
    }

    fn with_page<R>(&mut self, page_id: u64, read: impl FnOnce(&[u8], &mut Vec<u8>) -> io::Result<R>) -> io::Result<R> {
        if page_id == 0 || page_id >= self.header.next_page_id {
            return Err(invalid_data("page id is outside database"));
        }
        let offset = usize::try_from(page_id)
            .map_err(|_| invalid_data("page id exceeds usize"))?
            .checked_mul(PAGE_SIZE)
            .ok_or_else(|| invalid_data("page offset overflow"))?;
        let end = offset.checked_add(PAGE_SIZE).ok_or_else(|| invalid_data("page end overflow"))?;
        if let Some(mmap) = &self.snapshot.mmap {
            let page = mmap.get(offset..end).ok_or_else(|| invalid_data("page is truncated"))?;
            return read(page, &mut self.value_scratch);
        }
        let file = self.snapshot.file.as_ref().ok_or_else(|| invalid_data("query has no data source"))?;
        if self.page_buffer.len() != PAGE_SIZE {
            return Err(invalid_data("query page buffer has invalid length"));
        }
        read_exact_at_offset(
            file,
            &mut self.page_buffer,
            u64::try_from(offset).map_err(|_| invalid_data("page offset exceeds u64"))?,
        )?;
        read(&self.page_buffer, &mut self.value_scratch)
    }

    fn with_slotted_page<R>(
        &mut self,
        page_id: u64,
        read: impl FnOnce(&SlottedPage<&[u8]>, &mut Vec<u8>) -> io::Result<R>,
    ) -> io::Result<R> {
        let index = usize::try_from(page_id).map_err(|_| invalid_data("page id exceeds usize"))?;
        let cached = self
            .snapshot
            .page_validations
            .get(index)
            .ok_or_else(|| invalid_data("page id is outside cache"))?
            .get()
            .copied();
        let next_page_id = self.header.next_page_id;
        let (result, validation) = self.with_page(page_id, |bytes, scratch| {
            let page = match cached {
                Some(validation) => SlottedPage::from_immutable_snapshot(bytes, validation)?,
                None => SlottedPage::open(bytes, page_id, next_page_id)?,
            };
            let validation = cached.is_none().then(|| page.validation());
            read(&page, scratch).map(|result| (result, validation))
        })?;
        if let Some(validation) = validation {
            let slot = self
                .snapshot
                .page_validations
                .get(index)
                .ok_or_else(|| invalid_data("page id is outside cache"))?;
            let _ = slot.set(validation);
        }
        Ok(result)
    }

    fn cached_internal_child(&self, page_id: u64, key: &K) -> io::Result<Option<u64>>
    where
        K: Ord,
    {
        let index = usize::try_from(page_id).map_err(|_| invalid_data("page id exceeds usize"))?;
        Ok(self
            .snapshot
            .internal_routes
            .get(index)
            .ok_or_else(|| invalid_data("page id is outside route cache"))?
            .get()
            .map(|route| route.child_for(key)))
    }

    fn cache_internal_route(&self, page_id: u64, route: InternalRoute<K>) -> io::Result<()> {
        let index = usize::try_from(page_id).map_err(|_| invalid_data("page id exceeds usize"))?;
        let slot = self
            .snapshot
            .internal_routes
            .get(index)
            .ok_or_else(|| invalid_data("page id is outside route cache"))?;
        let _ = slot.set(route);
        Ok(())
    }

    fn locate_leaf(&mut self, key: &K) -> io::Result<u64>
    where
        K: Ord + for<'de> Deserialize<'de>,
    {
        let mut page_id = self.header.root_page_id;
        let mut depth = 0u64;
        loop {
            if depth >= self.header.next_page_id {
                return Err(invalid_data("tree descent contains a cycle"));
            }
            if let Some(child) = self.cached_internal_child(page_id, key)? {
                page_id = child;
                depth += 1;
                continue;
            }
            let step = self.with_slotted_page(page_id, |page, _| {
                match page.header().page_type {
                    PageType::Leaf => Ok(LocateLeaf::Leaf),
                    PageType::Internal => decode_internal_route(page).map(LocateLeaf::Internal),
                    PageType::Overflow | PageType::Free => Err(invalid_data("tree references a non-tree page")),
                }
            })?;
            match step {
                LocateLeaf::Internal(route) => {
                    let child = route.child_for(key);
                    self.cache_internal_route(page_id, route)?;
                    page_id = child;
                    depth += 1;
                }
                LocateLeaf::Leaf => return Ok(page_id),
            }
        }
    }

    fn locate_cell(&mut self, key: &K) -> io::Result<Option<(u64, Range<usize>)>>
    where
        K: Ord + for<'de> Deserialize<'de>,
    {
        let mut page_id = self.header.root_page_id;
        let mut depth = 0u64;
        loop {
            if depth >= self.header.next_page_id {
                return Err(invalid_data("tree descent contains a cycle"));
            }
            if let Some(child) = self.cached_internal_child(page_id, key)? {
                page_id = child;
                depth += 1;
                continue;
            }
            let step = self.with_slotted_page(page_id, |page, _| {
                match page.header().page_type {
                    PageType::Internal => decode_internal_route(page).map(LocateCell::Internal),
                    PageType::Leaf => search_leaf(page, key)?
                        .ok()
                        .map(|index| page.cell_range(index))
                        .transpose()
                        .map(LocateCell::Leaf),
                    PageType::Overflow | PageType::Free => Err(invalid_data("tree references a non-tree page")),
                }
            })?;
            match step {
                LocateCell::Internal(route) => {
                    let child = route.child_for(key);
                    self.cache_internal_route(page_id, route)?;
                    page_id = child;
                    depth += 1;
                }
                LocateCell::Leaf(range) => return Ok(range.map(|range| (page_id, range))),
            }
        }
    }

    fn leftmost_leaf(&mut self) -> io::Result<u64>
    where
        K: for<'de> Deserialize<'de>,
    {
        let mut page_id = self.header.root_page_id;
        let mut depth = 0u64;
        loop {
            if depth >= self.header.next_page_id {
                return Err(invalid_data("tree descent contains a cycle"));
            }
            let result = self.with_slotted_page(page_id, |page, _| {
                match page.header().page_type {
                    PageType::Leaf => Ok(None),
                    PageType::Internal => InternalPreamble::decode(page.as_bytes(), page_id, page.next_page_id())
                        .map(|preamble| Some(preamble.leftmost_child)),
                    PageType::Overflow | PageType::Free => Err(invalid_data("tree references a non-tree page")),
                }
            })?;
            let Some(child) = result else { return Ok(page_id) };
            page_id = child;
            depth += 1;
        }
    }

    fn decode_entry(&mut self, leaf_page_id: u64, slot_index: usize) -> io::Result<Option<(K, V)>>
    where
        K: for<'de> Deserialize<'de>,
        V: for<'de> Deserialize<'de>,
    {
        let range = self.with_slotted_page(leaf_page_id, |page, _| {
            if page.header().page_type != PageType::Leaf {
                return Err(invalid_data("iterator expected a leaf page"));
            }
            page.cell_range(slot_index)
        })?;
        self.decode_entry_range(leaf_page_id, range)
    }

    fn decode_entry_range(&mut self, leaf_page_id: u64, range: Range<usize>) -> io::Result<Option<(K, V)>>
    where
        K: for<'de> Deserialize<'de>,
        V: for<'de> Deserialize<'de>,
    {
        let next_page_id = self.header.next_page_id;
        let allocation_limit = self.value_allocation_limit();
        let decoded = self.with_page(leaf_page_id, |bytes, scratch| {
            let cell_bytes = bytes.get(range).ok_or_else(|| invalid_data("leaf cell is outside page"))?;
            let cell = LeafCellRef::decode(cell_bytes, leaf_page_id, next_page_id)?;
            let key = binary_deserialize(cell.key_bytes)?;
            match cell.value {
                LeafValueRef::Inline { compression: Compression::None, stored, .. } => {
                    binary_deserialize(stored).map(|value| DecodedEntry::Ready(key, value))
                }
                LeafValueRef::Inline { compression: Compression::Lz4, logical_len, stored, .. } => {
                    let decompressed = decompress_value_into(stored, logical_len, allocation_limit, scratch)?;
                    binary_deserialize(decompressed).map(|value| DecodedEntry::Ready(key, value))
                }
                LeafValueRef::Overflow { compression, logical_len, stored_len, head, crc32 } => {
                    Ok(DecodedEntry::Overflow(key, compression, logical_len, stored_len, head, crc32))
                }
                LeafValueRef::Tombstone => Ok(DecodedEntry::Tombstone),
            }
        })?;
        match decoded {
            DecodedEntry::Ready(key, value) => Ok(Some((key, value))),
            DecodedEntry::Tombstone => Ok(None),
            DecodedEntry::Overflow(key, compression, logical_len, stored_len, page_id, crc32) => {
                self.assemble_overflow_chain(compression, logical_len, stored_len, page_id, crc32, None)?;
                let value = binary_deserialize(&self.value_scratch)?;
                Ok(Some((key, value)))
            }
        }
    }
}

impl<K, V> BPlusTreeQuery<K, V>
where
    K: Ord + for<'de> Deserialize<'de>,
    V: for<'de> Deserialize<'de>,
{
    fn query_io(&mut self, key: &K) -> io::Result<Option<V>> {
        let Some((leaf_id, range)) = self.locate_cell(key)? else { return Ok(None) };
        self.decode_entry_range(leaf_id, range).map(|entry| entry.map(|(_, value)| value))
    }

    pub fn query(&mut self, key: &K) -> Result<Option<V>, BPlusTreeError> { self.query_io(key).map_err(Into::into) }

    pub fn query_zero_copy(&mut self, key: &K) -> Result<Option<V>, BPlusTreeError> { self.query(key) }

    fn locator_cell_range(&mut self, locator: Locator) -> io::Result<Range<usize>> {
        if self.locator_page_id != locator.leaf_page_id {
            let page_id = locator.leaf_page_id;
            let mut ranges = std::mem::take(&mut self.locator_cell_ranges);
            self.with_slotted_page(page_id, |page, _| {
                if page.header().page_type != PageType::Leaf {
                    return Err(invalid_data("locator does not reference a leaf page"));
                }
                let count = usize::from(page.header().cell_count);
                ranges.clear();
                ranges
                    .try_reserve(count)
                    .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
                for index in 0..count {
                    ranges.push(page.cell_range(index)?);
                }
                Ok(())
            })?;
            self.locator_page_id = page_id;
            self.locator_cell_ranges = ranges;
        }
        self.locator_cell_ranges
            .get(usize::from(locator.slot_index))
            .cloned()
            .ok_or_else(|| invalid_data("locator slot index is outside leaf page"))
    }

    pub(crate) fn read_locator_value(&mut self, locator: Locator, primary_key: &[u8]) -> io::Result<V> {
        let page_id = locator.leaf_page_id;
        let next_page_id = self.header.next_page_id;
        let range = self.locator_cell_range(locator)?;
        let allocation_limit = self.value_allocation_limit();
        let decoded = self.with_page(page_id, |bytes, scratch| {
            let cell = LeafCellRef::decode(
                bytes.get(range).ok_or_else(|| invalid_data("locator cell is outside page"))?,
                page_id,
                next_page_id,
            )?;
            if crc32fast::hash(cell.key_bytes) != locator.serialized_key_crc32
                || crc32fast::hash(primary_key) != locator.serialized_key_crc32
                || cell.key_bytes != primary_key
            {
                return Err(invalid_data("locator serialized key mismatch"));
            }
            match cell.value {
                LeafValueRef::Inline { compression: Compression::None, stored, .. } => {
                    binary_deserialize(stored).map(DecodedValue::Ready)
                }
                LeafValueRef::Inline { compression: Compression::Lz4, logical_len, stored, .. } => {
                    let decompressed = decompress_value_into(stored, logical_len, allocation_limit, scratch)?;
                    binary_deserialize(decompressed).map(DecodedValue::Ready)
                }
                LeafValueRef::Overflow { compression, logical_len, stored_len, head, crc32 } => {
                    Ok(DecodedValue::Overflow(compression, logical_len, stored_len, head, crc32))
                }
                LeafValueRef::Tombstone => Ok(DecodedValue::Tombstone),
            }
        })?;
        match decoded {
            DecodedValue::Ready(value) => Ok(value),
            DecodedValue::Overflow(compression, logical_len, stored_len, head, crc32) => {
                self.assemble_overflow_chain(compression, logical_len, stored_len, head, crc32, None)?;
                binary_deserialize(&self.value_scratch)
            }
            DecodedValue::Tombstone => Err(invalid_data("locator references a tombstone")),
        }
    }

    pub(crate) fn collect_with_locators(&mut self) -> io::Result<Vec<(K, V, Locator)>> {
        let mut result = Vec::new();
        let mut page_id = self.header.root_page_id;
        let mut visited = HashSet::new();
        let mut descending = true;
        let mut descent_depth = 0u64;
        loop {
            let next_page_id = self.header.next_page_id;
            let (child, right, locators) = self.with_page(page_id, |bytes, _| {
                let page = SlottedPage::open(bytes, page_id, next_page_id)?;
                if descending && page.header().page_type == PageType::Internal {
                    let leftmost = InternalPreamble::decode(bytes, page_id, next_page_id)?.leftmost_child;
                    return Ok((Some(leftmost), 0, Vec::new()));
                }
                if page.header().page_type != PageType::Leaf {
                    return Err(invalid_data("locator scan expected a tree page"));
                }
                let mut locators = Vec::new();
                for index in 0..usize::from(page.header().cell_count) {
                    let cell = LeafCellRef::decode(page.cell(index)?, page_id, next_page_id)?;
                    if !matches!(cell.value, LeafValueRef::Tombstone) {
                        locators.push((
                            Locator::for_key(
                                page_id,
                                u16::try_from(index).map_err(|_| invalid_data("slot index exceeds u16"))?,
                                cell.key_bytes,
                            )?,
                            page.cell_range(index)?,
                        ));
                    }
                }
                Ok((None, page.header().right, locators))
            })?;
            if let Some(child) = child {
                if descent_depth >= self.header.next_page_id {
                    return Err(invalid_data("locator descent contains a cycle"));
                }
                page_id = child;
                descent_depth += 1;
                continue;
            }
            descending = false;
            record_page_visit(&mut visited, page_id, "right sibling chain contains a cycle")?;
            result
                .try_reserve(locators.len())
                .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
            for (locator, range) in locators {
                let entry = self
                    .decode_entry_range(page_id, range)?
                    .ok_or_else(|| invalid_data("live locator became a tombstone"))?;
                result.push((entry.0, entry.1, locator));
            }
            if right == 0 {
                return Ok(result);
            }
            page_id = right;
        }
    }

    pub fn contains_live_key(&mut self, key: &K) -> Result<bool, BPlusTreeError> {
        let result: io::Result<bool> = (|| {
            let leaf_id = self.locate_leaf(key)?;
            let next_page_id = self.header.next_page_id;
            let index = self.with_slotted_page(leaf_id, |page, _| {
                search_leaf(page, key)
            })?;
            let Ok(index) = index else { return Ok(false) };
            self.with_slotted_page(leaf_id, |page, _| {
                let cell = LeafCellRef::decode(page.cell(index)?, leaf_id, next_page_id)?;
                Ok(!matches!(cell.value, LeafValueRef::Tombstone))
            })
        })();
        result.map_err(Into::into)
    }

    pub fn query_le(&mut self, key: &K) -> Result<Option<V>, BPlusTreeError> {
        self.query_le_io(key).map_err(Into::into)
    }

    fn query_le_io(&mut self, key: &K) -> io::Result<Option<V>> {
        let mut leaf_id = self.locate_leaf(key)?;
        let mut first = true;
        let mut visited = HashSet::new();
        record_page_visit(&mut visited, leaf_id, "left sibling chain contains a cycle")?;
        loop {
            let (left, count, start) = self.with_slotted_page(leaf_id, |page, _| {
                if page.header().page_type != PageType::Leaf {
                    return Err(invalid_data("left sibling is not a leaf"));
                }
                let count = usize::from(page.header().cell_count);
                let start = if first {
                    match search_leaf(page, key)? {
                        Ok(index) => Some(index),
                        Err(0) => None,
                        Err(index) => index.checked_sub(1),
                    }
                } else {
                    count.checked_sub(1)
                };
                Ok((page.header().left, count, start))
            })?;
            if let Some(start) = start {
                for index in (0..=start.min(count.saturating_sub(1))).rev() {
                    if let Some((_, value)) = self.decode_entry(leaf_id, index)? {
                        return Ok(Some(value));
                    }
                }
            }
            if left == 0 {
                return Ok(None);
            }
            let current = leaf_id;
            record_page_visit(&mut visited, left, "left sibling chain contains a cycle")?;
            leaf_id = left;
            self.with_slotted_page(leaf_id, |page, _| {
                if page.header().page_type != PageType::Leaf || page.header().right != current {
                    return Err(invalid_data("asymmetric leaf sibling link"));
                }
                Ok(())
            })?;
            first = false;
        }
    }

    pub fn len(&mut self) -> Result<usize, BPlusTreeError> {
        let result: io::Result<usize> = (|| {
            let mut page_id = self.leftmost_leaf()?;
            let mut visited = HashSet::new();
            record_page_visit(&mut visited, page_id, "right sibling chain contains a cycle")?;
            let mut total = 0usize;
            loop {
                let next_page_id = self.header.next_page_id;
                let (right, live) = self.with_page(page_id, |bytes, _| {
                    let page = SlottedPage::open(bytes, page_id, next_page_id)?;
                    if page.header().page_type != PageType::Leaf {
                        return Err(invalid_data("length scan expected a leaf page"));
                    }
                    let mut live = 0usize;
                    for index in 0..usize::from(page.header().cell_count) {
                        let cell = LeafCellRef::decode(page.cell(index)?, page_id, next_page_id)?;
                        if !matches!(cell.value, LeafValueRef::Tombstone) {
                            live = live.checked_add(1).ok_or_else(|| invalid_data("entry count overflow"))?;
                        }
                    }
                    Ok((page.header().right, live))
                })?;
                total = total.checked_add(live).ok_or_else(|| invalid_data("entry count overflow"))?;
                if right == 0 {
                    return Ok(total);
                }
                record_page_visit(&mut visited, right, "right sibling chain contains a cycle")?;
                self.with_page(right, |bytes, _| {
                    let page = SlottedPage::open(bytes, right, next_page_id)?;
                    if page.header().page_type != PageType::Leaf || page.header().left != page_id {
                        return Err(invalid_data("asymmetric leaf sibling link"));
                    }
                    Ok(())
                })?;
                page_id = right;
            }
        })();
        result.map_err(Into::into)
    }

    pub fn is_empty(&mut self) -> Result<bool, BPlusTreeError> {
        let mut iterator = self.iter();
        match iterator.next() {
            None => Ok(true),
            Some(Ok(_)) => Ok(false),
            Some(Err(err)) => Err(BPlusTreeError::Io(err)),
        }
    }

    pub fn iter(&mut self) -> BPlusTreeDiskIterator<'_, K, V> { BPlusTreeDiskIterator::new(self) }

    pub fn disk_iter(self) -> BPlusTreeDiskIteratorOwned<K, V> { BPlusTreeDiskIteratorOwned::new(self) }

    pub fn range_iter(
        &mut self,
        start: Bound<&K>,
        end: Bound<&K>,
    ) -> BPlusTreeRangeIterator<'_, K, V>
    where
        K: Clone,
    {
        let start = start.map(Clone::clone);
        let end = end.map(Clone::clone);
        BPlusTreeRangeIterator { iterator: BPlusTreeDiskIterator::from_bound(self, start.clone()), start, end }
    }

    pub fn range_page(
        &mut self,
        start: Bound<&K>,
        end: Bound<&K>,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<(K, V)>, bool), BPlusTreeError>
    where
        K: Clone,
    {
        let mut iterator = self.range_iter(start, end);
        for _ in 0..offset {
            if let Some(entry) = iterator.next() {
                let _ = entry.map_err(BPlusTreeError::Io)?;
            } else {
                return Ok((Vec::new(), false));
            }
        }
        let mut result = Vec::new();
        while result.len() < limit {
            match iterator.next() {
                Some(Ok(entry)) => {
                    result.try_reserve(1).map_err(|error| {
                        BPlusTreeError::Io(io::Error::new(io::ErrorKind::OutOfMemory, error))
                    })?;
                    result.push(entry);
                }
                Some(Err(err)) => return Err(BPlusTreeError::Io(err)),
                None => return Ok((result, false)),
            }
        }
        let has_more = match iterator.next() {
            Some(Ok(_)) => true,
            Some(Err(err)) => return Err(BPlusTreeError::Io(err)),
            None => false,
        };
        Ok((result, has_more))
    }
}

struct CursorState<K, V> {
    leaf_page_id: u64,
    slot_index: usize,
    cell_ranges: Vec<Range<usize>>,
    right_sibling: u64,
    expected_left_sibling: Option<u64>,
    page_loaded: bool,
    start: Bound<K>,
    initialized: bool,
    finished: bool,
    visited_leaves: HashSet<u64>,
    pending: Option<(K, V)>,
}

impl<K, V> CursorState<K, V> {
    fn new(start: Bound<K>) -> Self {
        Self {
            leaf_page_id: 0,
            slot_index: 0,
            cell_ranges: Vec::new(),
            right_sibling: 0,
            expected_left_sibling: None,
            page_loaded: false,
            start,
            initialized: false,
            finished: false,
            visited_leaves: HashSet::new(),
            pending: None,
        }
    }
}

fn load_cursor_page<K, V>(query: &mut BPlusTreeQuery<K, V>, state: &mut CursorState<K, V>) -> io::Result<()> {
    let page_id = state.leaf_page_id;
    let next_page_id = query.header.next_page_id;
    let expected_left = state.expected_left_sibling;
    let mut cell_ranges = std::mem::take(&mut state.cell_ranges);
    let right_sibling = query.with_page(page_id, |bytes, _| {
        let page = SlottedPage::open(bytes, page_id, next_page_id)?;
        if page.header().page_type != PageType::Leaf {
            return Err(invalid_data("iterator sibling is not a leaf"));
        }
        if expected_left.is_some_and(|left| page.header().left != left) {
            return Err(invalid_data("asymmetric leaf sibling link"));
        }
        let count = usize::from(page.header().cell_count);
        cell_ranges.clear();
        cell_ranges
            .try_reserve(count)
            .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
        for index in 0..count {
            cell_ranges.push(page.cell_range(index)?);
        }
        Ok(page.header().right)
    })?;
    state.cell_ranges = cell_ranges;
    state.right_sibling = right_sibling;
    state.page_loaded = true;
    Ok(())
}

fn record_page_visit(visited: &mut HashSet<u64>, page_id: u64, cycle_error: &'static str) -> io::Result<()> {
    visited.try_reserve(1).map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
    if !visited.insert(page_id) {
        return Err(invalid_data(cycle_error));
    }
    Ok(())
}

fn cursor_next<K, V>(query: &mut BPlusTreeQuery<K, V>, state: &mut CursorState<K, V>) -> Option<io::Result<(K, V)>>
where
    K: Ord + for<'de> Deserialize<'de>,
    V: for<'de> Deserialize<'de>,
{
    if let Some(entry) = state.pending.take() {
        return Some(Ok(entry));
    }
    if state.finished {
        return None;
    }
    let mut entry_error = false;
    let result = (|| {
        if !state.initialized {
            state.leaf_page_id = match &state.start {
                Bound::Included(key) | Bound::Excluded(key) => query.locate_leaf(key)?,
                Bound::Unbounded => query.leftmost_leaf()?,
            };
            record_page_visit(
                &mut state.visited_leaves,
                state.leaf_page_id,
                "right sibling chain contains a cycle",
            )?;
            if let Bound::Included(key) | Bound::Excluded(key) = &state.start {
                let page_id = state.leaf_page_id;
                let next_page_id = query.header.next_page_id;
                state.slot_index = query.with_page(page_id, |bytes, _| {
                    let page = SlottedPage::open(bytes, page_id, next_page_id)?;
                    let index = match search_leaf(&page, key)? {
                        Ok(index) if matches!(state.start, Bound::Excluded(_)) => index + 1,
                        Ok(index) | Err(index) => index,
                    };
                    Ok(index)
                })?;
            }
            state.initialized = true;
        }
        loop {
            if !state.page_loaded {
                load_cursor_page(query, state)?;
            }
            let page_id = state.leaf_page_id;
            while state.slot_index < state.cell_ranges.len() {
                let index = state.slot_index;
                state.slot_index += 1;
                match query.decode_entry_range(page_id, state.cell_ranges[index].clone()) {
                    Ok(Some(entry)) => return Ok(Some(entry)),
                    Ok(None) => {}
                    Err(error) => {
                        entry_error = true;
                        return Err(error);
                    }
                }
            }
            let right = state.right_sibling;
            if right == 0 {
                return Ok(None);
            }
            record_page_visit(&mut state.visited_leaves, right, "right sibling chain contains a cycle")?;
            state.expected_left_sibling = Some(page_id);
            state.leaf_page_id = right;
            state.slot_index = 0;
            state.page_loaded = false;
            state.cell_ranges.clear();
        }
    })();
    match result {
        Ok(Some(entry)) => Some(Ok(entry)),
        Ok(None) => {
            state.finished = true;
            None
        }
        Err(err) => {
            state.finished = !entry_error;
            Some(Err(err))
        }
    }
}

pub struct BPlusTreeDiskIterator<'a, K, V> {
    query: &'a mut BPlusTreeQuery<K, V>,
    state: CursorState<K, V>,
}

impl<'a, K, V> BPlusTreeDiskIterator<'a, K, V> {
    fn new(query: &'a mut BPlusTreeQuery<K, V>) -> Self {
        Self { query, state: CursorState::new(Bound::Unbounded) }
    }

    fn from_bound(query: &'a mut BPlusTreeQuery<K, V>, start: Bound<K>) -> Self {
        Self { query, state: CursorState::new(start) }
    }
}

impl<K, V> BPlusTreeDiskIterator<'_, K, V>
where
    K: Ord + for<'de> Deserialize<'de>,
    V: for<'de> Deserialize<'de>,
{
    pub fn try_is_empty(&mut self) -> io::Result<bool> {
        match self.next() {
            None => Ok(true),
            Some(Ok(entry)) => {
                self.state.pending = Some(entry);
                Ok(false)
            }
            Some(Err(err)) => Err(err),
        }
    }

}

impl<K, V> Iterator for BPlusTreeDiskIterator<'_, K, V>
where
    K: Ord + for<'de> Deserialize<'de>,
    V: for<'de> Deserialize<'de>,
{
    type Item = io::Result<(K, V)>;

    fn next(&mut self) -> Option<Self::Item> { cursor_next(self.query, &mut self.state) }
}

pub struct BPlusTreeDiskIteratorOwned<K, V> {
    query: BPlusTreeQuery<K, V>,
    state: CursorState<K, V>,
}

impl<K, V> BPlusTreeDiskIteratorOwned<K, V> {
    fn new(query: BPlusTreeQuery<K, V>) -> Self { Self { query, state: CursorState::new(Bound::Unbounded) } }
}

impl<K, V> BPlusTreeDiskIteratorOwned<K, V>
where
    K: Ord + for<'de> Deserialize<'de>,
    V: for<'de> Deserialize<'de>,
{
    pub fn try_is_empty(&mut self) -> io::Result<bool> {
        match self.next() {
            None => Ok(true),
            Some(Ok(entry)) => {
                self.state.pending = Some(entry);
                Ok(false)
            }
            Some(Err(err)) => Err(err),
        }
    }

}

impl<K, V> Iterator for BPlusTreeDiskIteratorOwned<K, V>
where
    K: Ord + for<'de> Deserialize<'de>,
    V: for<'de> Deserialize<'de>,
{
    type Item = io::Result<(K, V)>;

    fn next(&mut self) -> Option<Self::Item> { cursor_next(&mut self.query, &mut self.state) }
}

pub struct BPlusTreeRangeIterator<'a, K, V> {
    iterator: BPlusTreeDiskIterator<'a, K, V>,
    start: Bound<K>,
    end: Bound<K>,
}

fn within_start<K: Ord>(key: &K, bound: &Bound<K>) -> bool {
    match bound {
        Bound::Included(start) => key >= start,
        Bound::Excluded(start) => key > start,
        Bound::Unbounded => true,
    }
}

fn past_end<K: Ord>(key: &K, bound: &Bound<K>) -> bool {
    match bound {
        Bound::Included(end) => key > end,
        Bound::Excluded(end) => key >= end,
        Bound::Unbounded => false,
    }
}

impl<K, V> Iterator for BPlusTreeRangeIterator<'_, K, V>
where
    K: Ord + for<'de> Deserialize<'de>,
    V: for<'de> Deserialize<'de>,
{
    type Item = io::Result<(K, V)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let entry = self.iterator.next()?;
            match entry {
                Ok((key, _value)) if past_end(&key, &self.end) => {
                    self.iterator.state.finished = true;
                    return None;
                }
                Ok((key, value)) if within_start(&key, &self.start) => return Some(Ok((key, value))),
                Ok(_) => {}
                Err(err) => return Some(Err(err)),
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct VerificationReport {
    pub(crate) live_entries: u64,
    pub(crate) tree_pages: u64,
    pub(crate) overflow_pages: u64,
    pub(crate) free_pages: u64,
}

struct VerifyLeaf<K> {
    page_id: u64,
    left: u64,
    right: u64,
    minimum: Option<K>,
    maximum: Option<K>,
}

enum Visit<K> {
    Enter(u64, Option<K>, Option<K>),
    Exit(u64),
}

enum VerifyValue {
    Inline,
    Overflow(Compression, u32, u32, u64, u32),
    Tombstone,
}

fn finish_verified_page<K: Ord + Clone>(
    page_id: u64,
    active: &mut HashSet<u64>,
    internal_children: &mut HashMap<u64, Vec<(u64, Option<K>)>>,
    page_minimum: &mut HashMap<u64, Option<K>>,
) -> io::Result<()> {
    if !active.remove(&page_id) {
        return Err(invalid_data("tree verifier active set is inconsistent"));
    }
    let Some(children) = internal_children.remove(&page_id) else { return Ok(()) };
    for (child, separator) in children.iter().skip(1) {
        let actual = page_minimum
            .get(child)
            .and_then(Option::as_ref)
            .ok_or_else(|| invalid_data("internal child has no minimum key"))?;
        if separator.as_ref() != Some(actual) {
            return Err(invalid_data("internal separator is not the right child minimum"));
        }
    }
    let minimum = children
        .first()
        .and_then(|(child, _)| page_minimum.get(child))
        .cloned()
        .ok_or_else(|| invalid_data("internal leftmost child has no verified minimum"))?;
    page_minimum.insert(page_id, minimum);
    Ok(())
}

fn verify_leaf_page<K, V>(
    query: &mut BPlusTreeQuery<K, V>,
    page_id: u64,
    lower: Option<&K>,
    upper: Option<&K>,
    overflow_pages: &mut HashSet<u64>,
) -> io::Result<(VerifyLeaf<K>, u64)>
where
    K: Ord + for<'de> Deserialize<'de> + Clone,
{
    let next_page_id = query.header.next_page_id;
    let (left, right, count) = query.with_page(page_id, |bytes, _| {
        let page = SlottedPage::open(bytes, page_id, next_page_id)?;
        Ok((page.header().left, page.header().right, usize::from(page.header().cell_count)))
    })?;
    let mut minimum = None;
    let mut maximum = None;
    let mut live_entries = 0u64;
    for index in 0..count {
        let (key, value) = query.with_page(page_id, |bytes, _| {
            let page = SlottedPage::open(bytes, page_id, next_page_id)?;
            let cell = LeafCellRef::decode(page.cell(index)?, page_id, next_page_id)?;
            let value = match cell.value {
                LeafValueRef::Inline { .. } => VerifyValue::Inline,
                LeafValueRef::Overflow { compression, logical_len, stored_len, head, crc32 } => {
                    VerifyValue::Overflow(compression, logical_len, stored_len, head, crc32)
                }
                LeafValueRef::Tombstone => VerifyValue::Tombstone,
            };
            Ok((binary_deserialize::<K>(cell.key_bytes)?, value))
        })?;
        if maximum.as_ref().is_some_and(|previous| previous >= &key) {
            return Err(invalid_data("leaf keys are not strictly ordered"));
        }
        if lower.is_some_and(|bound| &key < bound) || upper.is_some_and(|bound| &key >= bound) {
            return Err(invalid_data("leaf key is outside parent separator range"));
        }
        minimum.get_or_insert_with(|| key.clone());
        maximum = Some(key);
        match value {
            VerifyValue::Tombstone => continue,
            VerifyValue::Inline => {}
            VerifyValue::Overflow(compression, logical_len, stored_len, head, crc32) => query
                .assemble_overflow_chain(
                    compression,
                    logical_len,
                    stored_len,
                    head,
                    crc32,
                    Some(overflow_pages),
                )?,
        }
        live_entries = live_entries
            .checked_add(1)
            .ok_or_else(|| invalid_data("live entry count overflow"))?;
    }
    Ok((VerifyLeaf { page_id, left, right, minimum, maximum }, live_entries))
}

fn verify_internal_page<K, V>(query: &mut BPlusTreeQuery<K, V>, page_id: u64) -> io::Result<Vec<(u64, Option<K>)>>
where
    K: Ord + for<'de> Deserialize<'de> + Clone,
{
    let next_page_id = query.header.next_page_id;
    query.with_page(page_id, |bytes, _| {
        let page = SlottedPage::open(bytes, page_id, next_page_id)?;
        let mut children = Vec::with_capacity(usize::from(page.header().cell_count) + 1);
        children.push((InternalPreamble::decode(bytes, page_id, next_page_id)?.leftmost_child, None));
        let mut previous = None;
        for cell in page.cells() {
            let cell = InternalCellRef::decode(cell?, page_id, next_page_id)?;
            let key = binary_deserialize::<K>(cell.key_bytes)?;
            if previous.as_ref().is_some_and(|prior| prior >= &key) {
                return Err(invalid_data("internal separators are not strictly ordered"));
            }
            previous = Some(key.clone());
            children.push((cell.right_child, Some(key)));
        }
        Ok(children)
    })
}

fn push_child_visits<K: Clone>(
    stack: &mut Vec<Visit<K>>,
    children: &[(u64, Option<K>)],
    lower: Option<&K>,
    upper: Option<&K>,
) -> io::Result<()> {
    for index in (0..children.len()).rev() {
        let child_lower = if index == 0 { lower.cloned() } else { children.get(index).and_then(|(_, key)| key.clone()) };
        let child_upper = children.get(index + 1).and_then(|(_, key)| key.clone()).or_else(|| upper.cloned());
        let child = children
            .get(index)
            .map(|(child, _)| *child)
            .ok_or_else(|| invalid_data("internal child index is invalid"))?;
        stack.push(Visit::Enter(child, child_lower, child_upper));
    }
    Ok(())
}

fn verify_leaf_links<K: Ord>(leaves: &[VerifyLeaf<K>]) -> io::Result<()> {
    for (index, leaf) in leaves.iter().enumerate() {
        let expected_left = index.checked_sub(1).map_or(0, |previous| leaves[previous].page_id);
        let expected_right = leaves.get(index + 1).map_or(0, |next| next.page_id);
        if leaf.left != expected_left || leaf.right != expected_right {
            return Err(invalid_data("leaf sibling links do not match tree order"));
        }
        if index > 0
            && leaves[index - 1]
                .maximum
                .as_ref()
                .zip(leaf.minimum.as_ref())
                .is_some_and(|(previous, current)| previous >= current)
        {
            return Err(invalid_data("keys are inverted across leaf siblings"));
        }
    }
    Ok(())
}

fn verify_free_pages<K, V>(
    query: &mut BPlusTreeQuery<K, V>,
    tree_pages: &HashSet<u64>,
    overflow_pages: &HashSet<u64>,
) -> io::Result<HashSet<u64>> {
    let mut free_pages = HashSet::new();
    let mut free = query.header.free_page_head;
    while free != 0 {
        record_page_visit(&mut free_pages, free, "free list contains a duplicate or cycle")?;
        if tree_pages.contains(&free) || overflow_pages.contains(&free) {
            return Err(invalid_data("page is reachable from both live data and free list"));
        }
        let next_page_id = query.header.next_page_id;
        free = query.with_page(free, |bytes, _| {
            let page = SlottedPage::open(bytes, free, next_page_id)?;
            if page.header().page_type != PageType::Free {
                return Err(invalid_data("free list references a non-free page"));
            }
            Ok(page.header().right)
        })?;
    }
    Ok(free_pages)
}

fn verify_page_ownership(
    next_page_id: u64,
    tree_pages: &HashSet<u64>,
    overflow_pages: &HashSet<u64>,
    free_pages: &HashSet<u64>,
) -> io::Result<()> {
    for page_id in 1..next_page_id {
        let memberships = u8::from(tree_pages.contains(&page_id))
            + u8::from(overflow_pages.contains(&page_id))
            + u8::from(free_pages.contains(&page_id));
        if memberships != 1 {
            return Err(invalid_data("database contains an orphan or multiply-owned page"));
        }
    }
    Ok(())
}

pub(crate) fn verify_full<K, V>(query: &mut BPlusTreeQuery<K, V>) -> io::Result<VerificationReport>
where
    K: Ord + for<'de> Deserialize<'de> + Clone,
{
    let mut tree_pages = HashSet::new();
    let mut active = HashSet::new();
    let mut overflow_pages = HashSet::new();
    let mut leaves = Vec::<VerifyLeaf<K>>::new();
    let mut page_minimum = HashMap::<u64, Option<K>>::new();
    let mut internal_children = HashMap::<u64, Vec<(u64, Option<K>)>>::new();
    let mut live_entries = 0u64;
    let mut stack = vec![Visit::Enter(query.header.root_page_id, None, None)];

    while let Some(visit) = stack.pop() {
        let (page_id, lower, upper) = match visit {
            Visit::Exit(page_id) => {
                finish_verified_page(page_id, &mut active, &mut internal_children, &mut page_minimum)?;
                continue;
            }
            Visit::Enter(page_id, lower, upper) => (page_id, lower, upper),
        };
        if active.contains(&page_id) {
            return Err(invalid_data("tree child graph contains a cycle"));
        }
        if !tree_pages.insert(page_id) {
            return Err(invalid_data("tree page has multiple parents"));
        }
        active.insert(page_id);
        stack.push(Visit::Exit(page_id));
        let next_page_id = query.header.next_page_id;
        let page_type = query.with_page(page_id, |bytes, _| {
            SlottedPage::open(bytes, page_id, next_page_id).map(|page| page.header().page_type)
        })?;
        match page_type {
            PageType::Leaf => {
                let (leaf, page_live_entries) =
                    verify_leaf_page(query, page_id, lower.as_ref(), upper.as_ref(), &mut overflow_pages)?;
                live_entries = live_entries
                    .checked_add(page_live_entries)
                    .ok_or_else(|| invalid_data("live entry count overflow"))?;
                page_minimum.insert(page_id, leaf.minimum.clone());
                leaves.push(leaf);
            }
            PageType::Internal => {
                let children = verify_internal_page(query, page_id)?;
                push_child_visits(&mut stack, &children, lower.as_ref(), upper.as_ref())?;
                internal_children.insert(page_id, children);
            }
            PageType::Overflow | PageType::Free => return Err(invalid_data("tree child has the wrong page type")),
        }
    }

    verify_leaf_links(&leaves)?;
    let free_pages = verify_free_pages(query, &tree_pages, &overflow_pages)?;
    verify_page_ownership(query.header.next_page_id, &tree_pages, &overflow_pages, &free_pages)?;
    Ok(VerificationReport {
        live_entries,
        tree_pages: u64::try_from(tree_pages.len()).map_err(|_| invalid_data("tree page count exceeds u64"))?,
        overflow_pages: u64::try_from(overflow_pages.len()).map_err(|_| invalid_data("overflow page count exceeds u64"))?,
        free_pages: u64::try_from(free_pages.len()).map_err(|_| invalid_data("free page count exceeds u64"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        repository::bplustree::v3::{
            format::{
                encode_inline_leaf_cell, encode_internal_cell, encode_overflow_leaf_cell,
                encode_tombstone_leaf_cell, encode_value, Compression, PageHeader, PageType,
                OVERFLOW_PAYLOAD_LEN, PAGE_HEADER_LEN, PAGE_SIZE,
            },
            page::{
                encode_free_page, encode_overflow_page, page_open_count, reset_page_open_count, SlottedPage,
            },
        },
        utils::binary_serialize,
    };
    use std::{
        fs, io,
        path::{Path, PathBuf},
        process::Command,
        sync::mpsc::{self, Receiver, RecvTimeoutError},
        thread::{self, JoinHandle},
        time::Duration,
    };
    use fs2::FileExt as _;

    const PAGE_ID: u64 = 7;
    const NEXT_PAGE_ID: u64 = 20;

    fn invalid_data<T>(result: io::Result<T>) -> io::Result<()> {
        match result {
            Err(err) if err.kind() == io::ErrorKind::InvalidData => Ok(()),
            Err(err) => Err(io::Error::other(format!("expected InvalidData, got {err}"))),
            Ok(_) => Err(io::Error::other("expected InvalidData")),
        }
    }

    fn invalid_input<T>(result: io::Result<T>) -> io::Result<()> {
        match result {
            Err(err) if err.kind() == io::ErrorKind::InvalidInput => Ok(()),
            Err(err) => Err(io::Error::other(format!("expected InvalidInput, got {err}"))),
            Ok(_) => Err(io::Error::other("expected InvalidInput")),
        }
    }

    fn database_header(path: &Path) -> io::Result<DatabaseHeader> {
        let bytes = fs::read(path)?;
        DatabaseHeader::decode(
            bytes
                .get(..PAGE_SIZE)
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "database header is truncated"))?,
        )
    }

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
    fn prepared_upsert_batch_is_key_sorted_and_stable() -> io::Result<()> {
        let keys = [3u32, 1, 2, 1];
        let values = ["three", "first", "two", "last"].map(String::from);
        let items = keys.iter().zip(&values).collect::<Vec<_>>();

        let prepared = BPlusTreeUpdate::<u32, String>::prepare_upsert_batch(&items)?;
        let decoded = prepared
            .into_iter()
            .map(|(key, value)| binary_deserialize::<String>(&value).map(|value| (key, value)))
            .collect::<io::Result<Vec<_>>>()?;
        assert_eq!(
            decoded,
            vec![
                (1, String::from("first")),
                (1, String::from("last")),
                (2, String::from("two")),
                (3, String::from("three")),
            ]
        );
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
        let value = random_value()
            .get(..300)
            .ok_or_else(|| io::Error::other("random test value is too short"))?
            .to_vec();
        let mut tree = BPlusTree::new();
        tree.insert(7u32, value.clone());

        let report = tree.store_verified(&path)?;

        assert_eq!(report.overflow_pages, 0);
        let mut query = BPlusTreeQuery::<u32, Vec<u8>>::try_new(&path)?;
        assert_eq!(query.query(&7).map_err(BPlusTreeError::to_io)?, Some(value));
        Ok(())
    }

    fn overflow_head(path: &Path, key: u32) -> io::Result<u64> {
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
        let smaller = large
            .get(..5_000)
            .ok_or_else(|| io::Error::other("random test value is too short"))?
            .to_vec();
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
        assert_eq!(query.iter().collect::<io::Result<Vec<_>>>()?, vec![(1, "one".into()), (2, "two".into()), (3, "three".into())]);
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
        let value = random_value()
            .get(..200)
            .ok_or_else(|| io::Error::other("random test value is too short"))?
            .to_vec();
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
        let new_right = query.with_page(left_leaf, |bytes, _| {
            Ok(SlottedPage::open(bytes, left_leaf, after.next_page_id)?.header().right)
        })?;
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

    fn long_split_key(index: u32, marker: char) -> String {
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
    fn immediate_commit_clears_wal_then_invalidates_sorted_index() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("immediate.db");
        let index_path = crate::repository::storage::get_file_path_for_db_index(&path);
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
    fn missing_delete_and_empty_commit_are_true_noops() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("no-op.db");
        let index_path = crate::repository::storage::get_file_path_for_db_index(&path);
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
        let index_path = crate::repository::storage::get_file_path_for_db_index(&path);
        let mut tree = BPlusTree::new();
        tree.insert(1u32, String::from("ccc"));
        tree.insert(2u32, String::from("a"));
        tree.insert(3u32, String::from("bb"));

        assert_ne!(tree.store_with_index(&path, String::len)?, 0);

        let query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
        let mut sorted = crate::repository::bplustree::sorted_index::v4::OwnedIterator::<u32, String, usize>::open(
            query,
            &index_path,
        )?;
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
        let index_path = crate::repository::storage::get_file_path_for_db_index(&path);
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
        let index_path = crate::repository::storage::get_file_path_for_db_index(&path);
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
        let page = SlottedPage::open(
            snapshot.as_slice(),
            leaf_id,
            active.transaction.next_header.next_page_id,
        )?;
        let mut cells = page.cells().map(|cell| cell.map(<[u8]>::to_vec)).collect::<io::Result<Vec<_>>>()?;
        let duplicate = cells
            .first()
            .cloned()
            .ok_or_else(|| io::Error::other("test leaf has no cells"))?;
        *cells.get_mut(1).ok_or_else(|| io::Error::other("test leaf lacks a second cell"))? = duplicate;
        let next_page_id = active.transaction.next_header.next_page_id;
        let dirty = active.transaction.page_mut(active.base.as_slice(), leaf_id)?;
        SlottedPage::open(dirty.as_mut_slice(), leaf_id, next_page_id)?
            .rebuild_ordered(cells.iter().map(Vec::as_slice))?;

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

    #[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
    struct ConditionalSerialize {
        value: String,
        fail: bool,
    }

    impl Serialize for ConditionalSerialize {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            if self.fail {
                return Err(serde::ser::Error::custom("injected serialization failure"));
            }
            self.value.serialize(serializer)
        }
    }

    impl<'de> Deserialize<'de> for ConditionalSerialize {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            String::deserialize(deserializer).map(|value| Self { value, fail: false })
        }
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

    fn pending_path(database: &Path, suffix: &str) -> PathBuf {
        let mut name = database.as_os_str().to_os_string();
        name.push(suffix);
        PathBuf::from(name)
    }

    fn spawn_replacement_writer(path: PathBuf) -> io::Result<(Receiver<io::Result<u64>>, JoinHandle<()>)> {
        let (started_sender, started_receiver) = mpsc::channel();
        let (result_sender, result_receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let mut replacement = BPlusTree::new();
            replacement.insert(2u32, String::from("replacement"));
            let _ = started_sender.send(());
            let _ = result_sender.send(replacement.store(&path));
        });
        started_receiver
            .recv_timeout(Duration::from_secs(5))
            .map_err(|error| io::Error::other(format!("replacement writer did not start: {error}")))?;
        Ok((result_receiver, handle))
    }

    fn assert_writer_is_blocked(receiver: &Receiver<io::Result<u64>>) -> io::Result<()> {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Err(RecvTimeoutError::Timeout) => Ok(()),
            Err(RecvTimeoutError::Disconnected) => Err(io::Error::other("replacement writer disconnected")),
            Ok(result) => {
                let root = result?;
                Err(io::Error::other(format!("replacement writer completed early with root page {root}")))
            }
        }
    }

    fn finish_writer(receiver: &Receiver<io::Result<u64>>, handle: JoinHandle<()>) -> io::Result<()> {
        receiver
            .recv_timeout(Duration::from_secs(5))
            .map_err(|error| io::Error::other(format!("replacement writer stayed blocked: {error}")))??;
        handle.join().map_err(|_| io::Error::other("replacement writer panicked"))
    }

    fn try_exclusive_sidecar(database: &Path) -> io::Result<bool> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(crate::repository::bplustree::common::sidecar_lock_path(database))?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                fs2::FileExt::unlock(&file)?;
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn run_exclusive_probe_child(database: &Path, expected: &str) -> io::Result<()> {
        let status = Command::new(std::env::current_exe()?)
            .arg("--exact")
            .arg("repository::bplustree::v3::tree::tests::exclusive_sidecar_probe_child")
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
        assert!(try_exclusive_sidecar(&path)?);
        Ok(())
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
        assert!(try_exclusive_sidecar(&path)?);
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

    fn empty_leaf(page_id: u64, next_page_id: u64) -> io::Result<[u8; PAGE_SIZE]> {
        let mut page = [0; PAGE_SIZE];
        PageHeader {
            page_type: PageType::Leaf,
            cell_count: 0,
            free_start: u16::try_from(PAGE_HEADER_LEN).map_err(io::Error::other)?,
            free_end: u16::try_from(PAGE_SIZE).map_err(io::Error::other)?,
            left: 0,
            right: 0,
        }
        .encode_into(&mut page, page_id, next_page_id)?;
        Ok(page)
    }

    fn leaf_cell_of_footprint(footprint: usize) -> io::Result<Vec<u8>> {
        let key_length = footprint
            .checked_sub(4 + 24)
            .ok_or_else(|| io::Error::other("footprint is too small"))?;
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

        encode_overflow_leaf_cell(b"large", 5000, Compression::None, 5000, 2, 0x1234_5678, PAGE_ID, NEXT_PAGE_ID, &mut cell)?;
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
    fn encoded_key_limit_is_2004_bytes() -> io::Result<()> {
        let mut cell = Vec::new();
        encode_tombstone_leaf_cell(&vec![b'k'; 2004], &mut cell)?;
        assert_eq!(cell.len() + 4, 2032);
        invalid_input(encode_tombstone_leaf_cell(&vec![b'k'; 2005], &mut cell))?;

        encode_internal_cell(&vec![b'k'; 2004], 9, PAGE_ID, NEXT_PAGE_ID, &mut cell)?;
        invalid_input(encode_internal_cell(&vec![b'k'; 2005], 9, PAGE_ID, NEXT_PAGE_ID, &mut cell))
    }

    #[test]
    fn adversarial_leaf_splits_reject_old_limit_and_accept_capped_cells() -> io::Result<()> {
        let first_witness = [
            leaf_cell_of_footprint(2026)?,
            leaf_cell_of_footprint(2040)?,
            leaf_cell_of_footprint(2038)?,
        ];
        invalid_input(choose_leaf_split(&first_witness))?;

        let second_witness = [
            leaf_cell_of_footprint(1984)?,
            leaf_cell_of_footprint(2296)?,
            leaf_cell_of_footprint(1984)?,
        ];
        invalid_input(choose_leaf_split(&second_witness))?;

        let capped = [
            leaf_cell_of_footprint(2026)?,
            leaf_cell_of_footprint(2032)?,
            leaf_cell_of_footprint(2032)?,
        ];
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
                let insert_at = if cells.is_empty() {
                    0
                } else {
                    usize::try_from(seed).map_err(io::Error::other)? % (cells.len() + 1)
                };
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
        assert_eq!(
            read_leaf_value(&database, &value, next_page_id, raw.len(), &mut scratch)?,
            Some(raw.as_slice())
        );
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
        invalid_data(read_leaf_value(
            &database,
            &empty_chain_value,
            NEXT_PAGE_ID,
            6000,
            &mut scratch,
        ))
    }

    fn page_range(page_id: u64) -> io::Result<std::ops::Range<usize>> {
        let start = usize::try_from(page_id)
            .map_err(io::Error::other)?
            .checked_mul(PAGE_SIZE)
            .ok_or_else(|| io::Error::other("test page offset overflow"))?;
        Ok(start..start + PAGE_SIZE)
    }

    fn rewrite_page_checksum(database: &mut [u8], page_id: u64) -> io::Result<()> {
        let range = page_range(page_id)?;
        crate::repository::bplustree::v3::format::write_page_checksum(
            database.get_mut(range).ok_or_else(|| io::Error::other("test page missing"))?,
        )
    }

    fn set_page_reference(database: &mut [u8], page_id: u64, offset: usize, reference: u64) -> io::Result<()> {
        let page = page_range(page_id)?;
        let start = page.start + offset;
        database
            .get_mut(start..start + 8)
            .ok_or_else(|| io::Error::other("test reference missing"))?
            .copy_from_slice(&reference.to_le_bytes());
        rewrite_page_checksum(database, page_id)
    }

    fn verify_rejects<K, V>(database: &[u8]) -> io::Result<()>
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

    fn stored_tree_fixture() -> io::Result<(Vec<u8>, u64, u64, u64, u64)> {
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
        let second = query.with_page(first, |bytes, _| {
            SlottedPage::open(bytes, first, next_page_id).map(|page| page.header().right)
        })?;
        if second == 0 {
            return Err(io::Error::other("test tree did not split"));
        }
        let mut last = second;
        loop {
            let right = query.with_page(last, |bytes, _| {
                SlottedPage::open(bytes, last, next_page_id).map(|page| page.header().right)
            })?;
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
        let (page_id, range) = query
            .locate_cell(&2)?
            .ok_or_else(|| io::Error::other("test key missing"))?;
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
        *database
            .get_mut(absolute)
            .ok_or_else(|| io::Error::other("test value byte missing"))? = 0xc1;
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
    fn range_page_does_not_preallocate_the_requested_limit() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("range-limit.db");
        let mut tree = BPlusTree::new();
        tree.insert(1u32, String::from("one"));
        tree.insert(2u32, String::from("two"));
        tree.store(&path)?;

        let mut query = BPlusTreeQuery::<u32, String>::try_new(&path)?;
        let (entries, has_more) = query
            .range_page(Bound::Unbounded, Bound::Unbounded, 0, usize::MAX)
            .map_err(BPlusTreeError::to_io)?;
        assert_eq!(entries, vec![(1, String::from("one")), (2, String::from("two"))]);
        assert!(!has_more);
        Ok(())
    }

    #[derive(Debug)]
    struct RejectValueDeserialize;

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
    fn full_verifier_rejects_tree_sibling_free_and_orphan_corruption() -> io::Result<()> {
        let (database, root, first, second, last) = stored_tree_fixture()?;

        let mut child_cycle = database.clone();
        set_page_reference(&mut child_cycle, root, 32, root)?;
        verify_rejects::<u32, String>(&child_cycle)?;

        let mut duplicate_child = database.clone();
        let root_start = page_range(root)?.start;
        let leftmost = u64::from_le_bytes(
            duplicate_child[root_start + 32..root_start + 40]
                .try_into()
                .map_err(io::Error::other)?,
        );
        let first_cell = u16::from_le_bytes(
            duplicate_child[root_start + 40..root_start + 42]
                .try_into()
                .map_err(io::Error::other)?,
        );
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
        let first_count = u16::from_le_bytes(
            inverted[first_start + 2..first_start + 4]
                .try_into()
                .map_err(io::Error::other)?,
        );
        let last_slot = first_start + PAGE_HEADER_LEN + (usize::from(first_count) - 1) * SLOT_LEN;
        let left_cell = usize::from(u16::from_le_bytes(
            inverted[last_slot..last_slot + 2].try_into().map_err(io::Error::other)?,
        ));
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

    fn random_value() -> Vec<u8> {
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

    fn overflow_heads(query: &mut BPlusTreeQuery<u32, Vec<u8>>) -> io::Result<(u64, u64, u64)> {
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
        let start = page_range(second_leaf)?.start
            + usize::try_from(second_cell_offset).map_err(io::Error::other)?
            + 12;
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
        let descriptor = page_range(second_leaf)?.start
            + usize::try_from(second_cell_offset).map_err(io::Error::other)?;
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
            Ok(u16::from_le_bytes(
                bytes[slot..slot + 2].try_into().map_err(io::Error::other)?,
            ))
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

        let error = publish_database(&temporary, &destination, |_| {
            Err(io::Error::other("injected directory sync failure"))
        })
        .err()
        .ok_or_else(|| io::Error::other("post-commit sync failure was hidden"))?;
        assert!(error.to_string().contains("database published but directory sync failed; durability unknown"));
        assert!(!temporary.exists());
        let mut query = BPlusTreeQuery::<u32, Vec<u8>>::try_new(&destination)?;
        assert_eq!(query.query(&1).map_err(BPlusTreeError::to_io)?, None);
        assert_eq!(query.query(&2).map_err(BPlusTreeError::to_io)?, Some(vec![2]));
        Ok(())
    }

    #[test]
    fn publish_failure_removes_the_temporary_file() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let destination = dir.path().join("destination-directory");
        fs::create_dir(&destination)?;
        let temporary = dir.path().join("database.tx.v3.tmp");
        fs::write(&temporary, b"new")?;

        assert!(publish_database(&temporary, &destination, sync_parent_directory).is_err());
        assert!(!temporary.exists());
        Ok(())
    }
}

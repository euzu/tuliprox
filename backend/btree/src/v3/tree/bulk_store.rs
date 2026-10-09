use super::{
    checked_page_usage, encode_inline_leaf_cell, encode_internal_cell, encode_overflow_leaf_cell, encode_overflow_page,
    encode_value, invalid_data, invalid_input, invalidate_sorted_index, stored_value_checksum, BPlusTreeQuery,
    InternalCellRef, InternalPreamble, LeafCellRef, LeafValueRef, PageHeader, PageType, PublishDatabaseError, Slot,
    SlottedPage, VerificationReport, VerifyLeaf, VerifyValue, MAX_CELL_FOOTPRINT, MAX_INLINE_STORED_VALUE,
    OVERFLOW_PAYLOAD_LEN, PAGE_HEADER_LEN, PAGE_SIZE, PAGE_SIZE_U64, SLOT_LEN,
};
use crate::{
    codec::{binary_deserialize, binary_serialize},
    common::write_all_at_offset,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs::File,
    io,
    path::{Path, PathBuf},
};

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
pub(super) struct PageSink {
    pub(super) file: File,
    pub(super) next_page_id: u64,
}

impl PageSink {
    pub(super) const fn new(file: File) -> Self { Self { file, next_page_id: 0 } }

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

    pub(super) fn write(&self, page_id: u64, page: &[u8; PAGE_SIZE]) -> io::Result<()> {
        let offset = page_id.checked_mul(PAGE_SIZE_U64).ok_or_else(|| invalid_input("page offset overflow"))?;
        write_all_at_offset(&self.file, page, offset)
    }
}

pub(super) fn leaf_page<T: AsRef<[u8]>>(
    page_id: u64,
    left: u64,
    right: u64,
    cells: &[T],
) -> io::Result<[u8; PAGE_SIZE]> {
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
    SlottedPage::open(bytes.as_mut_slice(), page_id, u64::MAX)?.rebuild_ordered(cells.iter().map(AsRef::as_ref))?;
    Ok(bytes)
}

pub(super) fn internal_page<T: AsRef<[u8]>>(
    page_id: u64,
    leftmost_child: u64,
    cells: &[T],
) -> io::Result<[u8; PAGE_SIZE]> {
    let first = cells.first().ok_or_else(|| invalid_input("internal page must contain a separator"))?.as_ref();
    let first_offset = PAGE_SIZE.checked_sub(first.len()).ok_or_else(|| invalid_input("internal cell exceeds page"))?;
    let mut bytes = [0; PAGE_SIZE];
    InternalPreamble { leftmost_child }.encode_into(&mut bytes, page_id, u64::MAX)?;
    bytes.get_mut(40..44).ok_or_else(|| invalid_input("internal slot is outside page"))?.copy_from_slice(
        &Slot {
            offset: u16::try_from(first_offset).map_err(|_| invalid_input("internal cell offset exceeds u16"))?,
            length: u16::try_from(first.len()).map_err(|_| invalid_input("internal cell length exceeds u16"))?,
        }
        .encode(),
    );
    bytes.get_mut(first_offset..).ok_or_else(|| invalid_input("internal cell is outside page"))?.copy_from_slice(first);
    PageHeader {
        page_type: PageType::Internal,
        cell_count: 1,
        free_start: 44,
        free_end: u16::try_from(first_offset).map_err(|_| invalid_input("internal free_end exceeds u16"))?,
        left: 0,
        right: 0,
    }
    .encode_into(&mut bytes, page_id, u64::MAX)?;
    SlottedPage::open(bytes.as_mut_slice(), page_id, u64::MAX)?.rebuild_ordered(cells.iter().map(AsRef::as_ref))?;
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
        groups.get_mut(last - 1).ok_or_else(|| invalid_input("internal grouping is missing its previous group"))?.1 =
            moved;
        groups.get_mut(last).ok_or_else(|| invalid_input("internal grouping is missing its last group"))?.0 = moved;
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
pub(super) fn build_pages<K, V>(entries: &BTreeMap<K, V>, sink: &mut PageSink) -> io::Result<u64>
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

pub(super) fn temporary_path(filepath: &Path) -> io::Result<PathBuf> {
    let name = filepath.file_name().ok_or_else(|| invalid_input("database path has no file name"))?.to_string_lossy();
    Ok(filepath.with_file_name(format!("{name}.{}.v3.tmp", uuid::Uuid::new_v4())))
}

pub(in crate::v3) fn publish_database(
    temporary: &Path,
    destination: &Path,
    sync_directory: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<(), PublishDatabaseError> {
    let temporary = match tempfile::TempPath::try_from_path(temporary) {
        Ok(path) => path,
        Err(error) => {
            let _ = std::fs::remove_file(temporary);
            return Err(PublishDatabaseError::NotPublished(error));
        }
    };
    temporary.persist(destination).map_err(io::Error::from).map_err(PublishDatabaseError::NotPublished)?;
    sync_directory(destination).map_err(|error| {
        PublishDatabaseError::PublishedDurabilityUnknown(io::Error::new(
            error.kind(),
            format!("database published but directory sync failed; durability unknown: {error}"),
        ))
    })
}

/// Invalidates the sorted index whenever publication made the replacement database visible.
pub(in crate::v3) fn publish_database_and_invalidate_sorted_index(
    temporary: &Path,
    destination: &Path,
    sync_directory: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let publish_error = match publish_database(temporary, destination, sync_directory) {
        Ok(()) => None,
        Err(error) if error.database_was_published() => Some(io::Error::from(error)),
        Err(error) => return Err(io::Error::from(error)),
    };
    let invalidation_error = invalidate_sorted_index(destination).err();
    match (publish_error, invalidation_error) {
        (Some(publish_error), Some(invalidation_error)) => Err(io::Error::new(
            publish_error.kind(),
            format!(
                "database {} was published with unknown directory durability: {publish_error}; its previous sorted index could not be invalidated: {invalidation_error}",
                destination.display()
            ),
        )),
        (Some(publish_error), None) => Err(io::Error::new(
            publish_error.kind(),
            format!(
                "database {} was published and its previous sorted index was invalidated, but publication durability remains unknown: {publish_error}",
                destination.display()
            ),
        )),
        (None, Some(error)) => Err(io::Error::new(
            error.kind(),
            format!(
                "database {} was published, but its previous sorted index could not be invalidated: {error}",
                destination.display()
            ),
        )),
        (None, None) => Ok(()),
    }
}

pub(super) struct StoredDatabase {
    pub(super) root_page_id: u64,
    pub(super) verification: VerificationReport,
}

pub(super) fn verify_leaf_page<K, V>(
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
            VerifyValue::Overflow(compression, logical_len, stored_len, head, crc32) => query.assemble_overflow_chain(
                compression,
                logical_len,
                stored_len,
                head,
                crc32,
                Some(overflow_pages),
            )?,
        }
        live_entries = live_entries.checked_add(1).ok_or_else(|| invalid_data("live entry count overflow"))?;
    }
    Ok((VerifyLeaf { page_id, left, right, minimum, maximum }, live_entries))
}

pub(super) fn verify_internal_page<K, V>(
    query: &mut BPlusTreeQuery<K, V>,
    page_id: u64,
) -> io::Result<Vec<(u64, Option<K>)>>
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

#[cfg(test)]
use super::{database_page, INTERNAL_KEY_DECODE_COUNT};
use super::{
    decompress_value_in_place, decompress_value_into, invalid_data, invalid_input, overflow_payload, record_page_visit,
    recover_pending, recovery_required, wal_path, wal_temporary_path, BPlusTreeDiskIterator,
    BPlusTreeDiskIteratorOwned, BPlusTreeMetadata, BPlusTreeQuery, BPlusTreeRangeIterator, Compression, DatabaseHeader,
    InternalCellRef, InternalPreamble, LeafCellRef, LeafValueRef, Locator, PageType, PageValidation,
    SharedSidecarGuard, SlottedPage, PAGE_SIZE,
};
use crate::{
    codec::binary_deserialize,
    common::{mmap_with_advice, read_exact_at_offset, Advice, BPlusTreeError},
};
use memmap2::Mmap;
use serde::Deserialize;
use std::{
    collections::HashSet,
    fs::File,
    io,
    marker::PhantomData,
    ops::{Bound, Range},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

pub(super) fn decode_key<K>(encoded: &[u8]) -> io::Result<K>
where
    K: for<'de> Deserialize<'de>,
{
    rmp_serde::from_slice(encoded)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, format!("invalid serialized key: {err}")))
}

#[cfg(test)]
pub(super) fn reset_internal_key_decode_count() { INTERNAL_KEY_DECODE_COUNT.set(0); }

#[cfg(test)]
pub(super) fn internal_key_decode_count() -> usize { INTERNAL_KEY_DECODE_COUNT.get() }

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
    let cell = LeafCellRef::decode(page.cell(usize::from(locator.slot_index))?, page.page_id(), page.next_page_id())?;
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
            let logical_length =
                usize::try_from(logical_len).map_err(|_| invalid_data("logical length exceeds usize"))?;
            let stored_length = usize::try_from(stored_len).map_err(|_| invalid_data("stored length exceeds usize"))?;
            if logical_length > maximum_length || stored_length > maximum_length {
                return Err(invalid_data("overflow value exceeds allocation limit"));
            }
            scratch.clear();
            scratch.try_reserve(stored_length).map_err(|err| io::Error::new(io::ErrorKind::OutOfMemory, err))?;
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
                visited.try_reserve(1).map_err(|err| io::Error::new(io::ErrorKind::OutOfMemory, err))?;
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
    separators.try_reserve_exact(count).map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
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

pub(super) struct QuerySnapshot<K> {
    pub(super) file: Option<File>,
    pub(super) mmap: Option<Mmap>,
    pub(super) filepath: PathBuf,
    pub(super) page_validations: Vec<OnceLock<PageValidation>>,
    internal_routes: Vec<OnceLock<InternalRoute<K>>>,
    pub(super) sidecar_guard: Option<SharedSidecarGuard>,
}

impl<K, V> BPlusTreeQuery<K, V> {
    pub(super) fn from_file_unlocked(file: File) -> io::Result<Self> {
        let file_len =
            usize::try_from(file.metadata()?.len()).map_err(|_| invalid_data("database length exceeds usize"))?;
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
        let page_count =
            usize::try_from(header.next_page_id).map_err(|_| invalid_data("next page id exceeds usize"))?;
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
                    if matches!(error.kind(), io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem) =>
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

    /// A query handle over an empty in-memory snapshot, for tests that need a
    /// `BPlusTreeQuery` value without a database behind it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn clone_error_fixture() -> Self {
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

    /// The metadata carried in the database header of this snapshot.
    pub fn metadata(&self) -> &BPlusTreeMetadata { &self.header.metadata }

    pub fn snapshot_identity(&self) -> ([u8; 16], u64) { (self.header.database_id, self.header.generation) }

    pub(crate) fn snapshot_metadata(&self) -> &BPlusTreeMetadata { &self.header.metadata }

    fn value_allocation_limit(&self) -> usize {
        self.file_len.saturating_mul(256).min(usize::try_from(u32::MAX).unwrap_or(usize::MAX))
    }

    pub(super) fn assemble_overflow_chain(
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

    pub(super) fn with_page<R>(
        &mut self,
        page_id: u64,
        read: impl FnOnce(&[u8], &mut Vec<u8>) -> io::Result<R>,
    ) -> io::Result<R> {
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
            let slot =
                self.snapshot.page_validations.get(index).ok_or_else(|| invalid_data("page id is outside cache"))?;
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
        let slot =
            self.snapshot.internal_routes.get(index).ok_or_else(|| invalid_data("page id is outside route cache"))?;
        let _ = slot.set(route);
        Ok(())
    }

    pub(super) fn locate_leaf(&mut self, key: &K) -> io::Result<u64>
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
            let step = self.with_slotted_page(page_id, |page, _| match page.header().page_type {
                PageType::Leaf => Ok(LocateLeaf::Leaf),
                PageType::Internal => decode_internal_route(page).map(LocateLeaf::Internal),
                PageType::Overflow | PageType::Free => Err(invalid_data("tree references a non-tree page")),
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

    pub(super) fn locate_cell(&mut self, key: &K) -> io::Result<Option<(u64, Range<usize>)>>
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
            let step = self.with_slotted_page(page_id, |page, _| match page.header().page_type {
                PageType::Internal => decode_internal_route(page).map(LocateCell::Internal),
                PageType::Leaf => {
                    search_leaf(page, key)?.ok().map(|index| page.cell_range(index)).transpose().map(LocateCell::Leaf)
                }
                PageType::Overflow | PageType::Free => Err(invalid_data("tree references a non-tree page")),
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

    pub(super) fn leftmost_leaf(&mut self) -> io::Result<u64>
    where
        K: for<'de> Deserialize<'de>,
    {
        let mut page_id = self.header.root_page_id;
        let mut depth = 0u64;
        loop {
            if depth >= self.header.next_page_id {
                return Err(invalid_data("tree descent contains a cycle"));
            }
            let result = self.with_slotted_page(page_id, |page, _| match page.header().page_type {
                PageType::Leaf => Ok(None),
                PageType::Internal => InternalPreamble::decode(page.as_bytes(), page_id, page.next_page_id())
                    .map(|preamble| Some(preamble.leftmost_child)),
                PageType::Overflow | PageType::Free => Err(invalid_data("tree references a non-tree page")),
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

    pub(super) fn decode_entry_range(&mut self, leaf_page_id: u64, range: Range<usize>) -> io::Result<Option<(K, V)>>
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
    pub(super) fn query_io(&mut self, key: &K) -> io::Result<Option<V>> {
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
                ranges.try_reserve(count).map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
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
            result.try_reserve(locators.len()).map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
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
            let index = self.with_slotted_page(leaf_id, |page, _| search_leaf(page, key))?;
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

    pub fn range_iter(&mut self, start: Bound<&K>, end: Bound<&K>) -> BPlusTreeRangeIterator<'_, K, V>
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
                    result
                        .try_reserve(1)
                        .map_err(|error| BPlusTreeError::Io(io::Error::new(io::ErrorKind::OutOfMemory, error)))?;
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

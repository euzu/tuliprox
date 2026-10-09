use super::{
    decompress_value_in_place, decompress_value_into, encode_free_page, invalid_data, invalid_input,
    locate_transaction_leaf, overflow_chain_pages, overflow_payload, search_leaf, Compression, DatabaseHeader,
    LeafCellRef, LeafValueRef, PageType, SlottedPage, PAGE_SIZE,
};
use crate::{
    codec::binary_deserialize,
    common::{mmap_with_advice, Advice},
};
use memmap2::Mmap;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashSet},
    fs::File,
    io,
    io::Read,
    path::Path,
};

pub(super) struct DatabaseImage {
    pub(super) mmap: Option<Mmap>,
    pub(super) fallback: Vec<u8>,
}

impl DatabaseImage {
    pub(super) fn open(path: &Path) -> io::Result<(Self, DatabaseHeader)> {
        let mut file = File::open(path)?;
        let file_len =
            usize::try_from(file.metadata()?.len()).map_err(|_| invalid_data("database length exceeds usize"))?;
        let mmap = mmap_with_advice(&file, Advice::Normal, "v3 B+Tree update");
        let mut fallback = Vec::new();
        if mmap.is_none() {
            fallback.try_reserve_exact(file_len).map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
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

    pub(super) fn as_slice(&self) -> &[u8] { self.mmap.as_deref().unwrap_or(&self.fallback) }
}

pub(super) struct WriteTransaction {
    pub(super) original_header: DatabaseHeader,
    pub(super) next_header: DatabaseHeader,
    pub(super) original_file_len: u64,
    pub(super) dirty_pages: BTreeMap<u64, Box<[u8; PAGE_SIZE]>>,
    pub(super) allocated_pages: HashSet<u64>,
    pub(super) freed_pages: HashSet<u64>,
}

impl WriteTransaction {
    pub(super) fn new(header: DatabaseHeader, original_file_len: usize) -> io::Result<Self> {
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

    pub(super) fn page<'a>(&'a self, base: &'a [u8], page_id: u64) -> io::Result<&'a [u8]> {
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

    pub(super) fn page_copy(&self, base: &[u8], page_id: u64) -> io::Result<[u8; PAGE_SIZE]> {
        let mut copied = [0; PAGE_SIZE];
        copied.copy_from_slice(self.page(base, page_id)?);
        Ok(copied)
    }

    pub(super) fn page_mut<'a>(&'a mut self, base: &[u8], page_id: u64) -> io::Result<&'a mut [u8; PAGE_SIZE]> {
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
    pub(super) fn write_page(&mut self, page_id: u64, page: [u8; PAGE_SIZE]) -> io::Result<()> {
        if page_id == 0 || page_id >= self.next_header.next_page_id {
            return Err(invalid_input("written page id is outside transaction bounds"));
        }
        let _ = self.dirty_pages.insert(page_id, Box::new(page));
        Ok(())
    }

    pub(super) fn allocate_page(&mut self, base: &[u8]) -> io::Result<u64> {
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
        self.next_header.next_page_id = page_id.checked_add(1).ok_or_else(|| invalid_input("next page id overflow"))?;
        if !self.allocated_pages.insert(page_id) {
            return Err(invalid_data("appended page was allocated twice"));
        }
        Ok(page_id)
    }

    pub(super) fn free_page(&mut self, page_id: u64) -> io::Result<()> {
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

    pub(super) fn prepared_pages(&mut self) -> io::Result<Vec<(u64, &[u8; PAGE_SIZE])>> {
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

pub(super) fn page_byte_range(page_id: u64, database_len: usize) -> io::Result<std::ops::Range<usize>> {
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

#[derive(Default)]
pub(super) struct WriteScratch {
    pub(super) key: Vec<u8>,
    pub(super) value: Vec<u8>,
    pub(super) compression: Vec<u8>,
    pub(super) cell: Vec<u8>,
    pub(super) read_value: Vec<u8>,
}

pub(super) fn query_transaction<K, V>(
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
            read_transaction_overflow(transaction, base, compression, logical_len, stored_len, head, crc32, scratch)?
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
    scratch.try_reserve(stored_len).map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
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

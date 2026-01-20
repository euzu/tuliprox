use crate::repository::storage::get_file_path_for_db_index;
use crate::utils;
use crate::utils::{binary_deserialize, binary_serialize};
use fs2::FileExt as _;
use indexmap::IndexMap;
use log::error;
use memmap2::Mmap;
use serde::{Deserialize, Serialize};
use shared::error::{string_to_io_error, to_io_error};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::marker::PhantomData;
use std::mem::size_of;
#[cfg(unix)]
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

// Constants (Restored)
const PAGE_SIZE: u16 = 4096;
pub const PAGE_SIZE_USIZE: usize = PAGE_SIZE as usize;
const LEN_SIZE: usize = 4;
const FLAG_SIZE: usize = 1;
const MAGIC: &[u8; 4] = b"BTRE";
const STORAGE_VERSION: u32 = 1;
const HEADER_SIZE: u64 = PAGE_SIZE as u64;
const ROOT_OFFSET_POS: u64 = 8;
const METADATA_OFFSET_POS: u64 = 16;
// Reserve space for metadata (e.g. 4096 - 16 = 4080 bytes max, but let's be safe)
const METADATA_MAX_SIZE: usize = 4000;

const POINTER_SIZE: usize = 8;
const INFO_SIZE: usize = 12; // (u64, u32)

// Maximum number of blocks to cache in memory (~4MB at 4KB per block)
const CACHE_CAPACITY: usize = 1024;

// MessagePack overhead estimation
const MSGPACK_OVERHEAD_PER_ENTRY: usize = 22;

// Value packing configuration
const SMALL_VALUE_THRESHOLD: usize = 256;
const PACK_BLOCK_HEADER_SIZE: usize = 4;
const PACK_VALUE_HEADER_SIZE: usize = 4;

// LZ4 compression configuration
const COMPRESSION_MIN_SIZE: usize = 64;
const COMPRESSION_THRESHOLD_PERCENT: usize = 85;
const COMPRESSION_FLAG_NONE: u8 = 0x00;
pub const COMPRESSION_FLAG_LZ4: u8 = 0x01;

// Page Configuration
const PAGE_HEADER_SIZE: u16 = 16;
const PAGE_HEADER_SIZE_USIZE: usize = PAGE_HEADER_SIZE as usize;
const SLOT_SIZE: usize = 2; // u16

const MAGIC_METADATA_TARGET_ID_MAPPING: u8 = 0x01;

/*
    Page Header Layout

    ┌─────────────────────────────────────────────────────────────┐
    │ Page Header (16 bytes)                                      │
    ├─────────────────────────────────────────────────────────────┤
    │ Slot Directory (grows ↓)                                    │
    │ [Slot 0: u16] [Slot 1: u16] [Slot 2: u16] ...               │
    ├─────────────────────────────────────────────────────────────┤
    │                                                             │
    │                   < Free Space >                            │
    │                                                             │
    │ (Splits when Free Space < Cell Size)                        │
    ├─────────────────────────────────────────────────────────────┤
    │ Cell Data (grows ↑)                                         │
    │ ... [Cell 2] [Cell 1] [Cell 0]                              │
    └─────────────────────────────────────────────────────────────┘

    Leaf Cell (key + Value)
    ┌────────────┬─────────────┬───────────────┬─────────────────┐
    │ Header     │ Key         │ Value Header  │ Value Payload   │
    │ [len: var] │ [bytes...]  │ [flag: 1B]    │ [bytes...]      │
    └────────────┴─────────────┴───────────────┴─────────────────┘

    Note: Values > 1/4 Page Size (1KB) are moved to Overflow Pages, leaving a 12-byte pointer [OverflowPgId][Length].

    Internal Cell (Key + Pointer)
    ┌──────────────┬─────────────┬────────────┐
    │ Child Ptr    │ Key Len     │ Key Bytes  │
    │ [u64: 8B]    │ [varint]    │ [bytes...] │
    └──────────────┴─────────────┴────────────┘
*/

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PageType {
    Leaf = 1,
    Internal = 2,
    Overflow = 3,
}

#[derive(Debug, Clone, Copy)]
pub struct PageHeader {
    pub page_type: PageType, // 0x01=Leaf, 0x02=Internal, 0x03=Overflow
    pub cell_count: u16,     // Number of active cells
    pub free_start: u16,     // Offset to start of free space (after slots)
    pub free_end: u16,       // Offset to end of free space (before cells)
    pub right_sibling: u64,  // 0 if none, pointer to next leaf (for range scans)
    pub checksum: u32,       // TODO Data integrity check, currently not neccessary, maybe in future
}

impl PageHeader {
    pub fn new(page_type: PageType) -> Self {
        Self {
            page_type,
            cell_count: 0,
            free_start: PAGE_HEADER_SIZE,
            free_end: PAGE_SIZE,
            right_sibling: 0,
            checksum: 0,
        }
    }

    pub fn serialize(&self) -> [u8; PAGE_HEADER_SIZE_USIZE] {
        let mut buf = [0u8; PAGE_HEADER_SIZE_USIZE];
        buf[0] = self.page_type as u8;
        buf[1] = 0; // padding
        buf[2..4].copy_from_slice(&self.cell_count.to_le_bytes());
        buf[4..6].copy_from_slice(&self.free_start.to_le_bytes());
        buf[6..8].copy_from_slice(&self.free_end.to_le_bytes());
        buf[8..16].copy_from_slice(&self.right_sibling.to_le_bytes());
        buf
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self, PageError> {
        if buf.len() < PAGE_HEADER_SIZE_USIZE {
            return Err(PageError::Corrupted);
        }
        let page_type = match buf[0] {
            1 => PageType::Leaf,
            2 => PageType::Internal,
            3 => PageType::Overflow,
            _ => return Err(PageError::Corrupted),
        };

        // Use try_into to safely read bytes, although the length check above makes it safe.
        // we can map err.
        let cell_count = u16::from_le_bytes(buf[2..4].try_into().map_err(|_| PageError::Corrupted)?);
        let free_start = u16::from_le_bytes(buf[4..6].try_into().map_err(|_| PageError::Corrupted)?);
        let free_end = u16::from_le_bytes(buf[6..8].try_into().map_err(|_| PageError::Corrupted)?);
        let right_sibling = u64::from_le_bytes(buf[8..16].try_into().map_err(|_| PageError::Corrupted)?);

        Ok(Self {
            page_type,
            cell_count,
            free_start,
            free_end,
            right_sibling,
            checksum: 0,
        })
    }
}

pub struct SlottedPage<'a> {
    pub header: PageHeader,
    data: &'a mut [u8],
}

#[derive(Debug)]
pub enum PageError {
    NoSpace,
    InvalidIndex,
    Corrupted,
    Io(io::Error),
}

impl std::fmt::Display for PageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PageError::NoSpace => write!(f, "Page has no space for insertion"),
            PageError::InvalidIndex => write!(f, "Invalid cell index"),
            PageError::Corrupted => write!(f, "Page data is corrupted"),
            PageError::Io(err) => write!(f, "I/O error: {err}"),
        }
    }
}

impl std::error::Error for PageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PageError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for PageError {
    fn from(err: io::Error) -> Self {
        PageError::Io(err)
    }
}

/// Error types for B+Tree operations that distinguish between different failure modes.
/// This allows callers to handle "key not found" differently from actual errors like corruption.
#[derive(Debug)]
pub enum BPlusTreeError {
    /// An I/O error occurred during file operations
    Io(io::Error),
    /// Data corruption detected during deserialization
    Corrupted(String),
    /// The tree structure is invalid (e.g., missing child pointers)
    InvalidStructure(String),
    /// The requested key was not found in the tree (used for update operations)
    KeyNotFound,
}

impl std::fmt::Display for BPlusTreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BPlusTreeError::Io(err) => write!(f, "I/O error: {err}"),
            BPlusTreeError::Corrupted(msg) => write!(f, "Data corrupted: {msg}"),
            BPlusTreeError::InvalidStructure(msg) => write!(f, "Invalid structure: {msg}"),
            BPlusTreeError::KeyNotFound => write!(f, "Key not found"),
        }
    }
}

impl std::error::Error for BPlusTreeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BPlusTreeError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for BPlusTreeError {
    fn from(err: io::Error) -> Self {
        BPlusTreeError::Io(err)
    }
}


impl BPlusTreeError {
    pub fn to_io(self) -> io::Error {
        match self {
            BPlusTreeError::Io(e) => e,
            BPlusTreeError::KeyNotFound => io::Error::new(io::ErrorKind::NotFound, "Key not found"),
            err => io::Error::new(io::ErrorKind::InvalidData, err),
        }
    }
}


impl From<PageError> for BPlusTreeError {
    fn from(err: PageError) -> Self {
        match err {
            PageError::Io(e) => BPlusTreeError::Io(e),
            PageError::Corrupted => BPlusTreeError::Corrupted("Page data corrupted".into()),
            other => BPlusTreeError::InvalidStructure(format!("{other:?}")),
        }
    }
}


impl<'a> SlottedPage<'a> {
    pub fn new(data: &'a mut [u8], page_type: PageType) -> Result<Self, PageError> {
        if data.len() < PAGE_HEADER_SIZE_USIZE {
            return Err(PageError::NoSpace);
        }
        let header = PageHeader::new(page_type);
        // Initialize header in buffer
        let h_bytes = header.serialize();
        data[..PAGE_HEADER_SIZE_USIZE].copy_from_slice(&h_bytes);
        Ok(Self { header, data })
    }

    pub fn open(data: &'a mut [u8]) -> Result<Self, PageError> {
        if data.len() < PAGE_HEADER_SIZE_USIZE {
            return Err(PageError::Corrupted);
        }
        let header = PageHeader::deserialize(&data[..PAGE_HEADER_SIZE_USIZE])?;
        Ok(Self { header, data })
    }

    pub fn commit(&mut self) {
        let h_bytes = self.header.serialize();
        if self.data.len() >= PAGE_HEADER_SIZE_USIZE {
            self.data[..PAGE_HEADER_SIZE_USIZE].copy_from_slice(&h_bytes);
        }
    }

    pub fn free_space(&self) -> usize {
        if self.header.free_end >= self.header.free_start {
            (self.header.free_end - self.header.free_start) as usize
        } else {
            0
        }
    }

    /// Insert a cell directly. Caller must ensure specific order (e.g. invalidating current sort).
    /// Typically used by `insert_at_index`.
    fn append_cell(&mut self, cell_data: &[u8]) -> Result<u16, PageError> {
        let required = cell_data.len();
        if self.free_space() < required + SLOT_SIZE {
            return Err(PageError::NoSpace);
        }

        let req_u16 = u16::try_from(required).map_err(|_| PageError::NoSpace)?;
        // Data grows downwards. Safe cast due to page size check.
        let offset = self.header.free_end.checked_sub(req_u16).ok_or(PageError::NoSpace)?;

        // Bounds check
        if (offset as usize) + required > self.data.len() {
            return Err(PageError::NoSpace);
        }

        self.data[offset as usize..(offset as usize + required)].copy_from_slice(cell_data);

        self.header.free_end = offset;
        Ok(offset)
    }

    pub fn insert_at_index(&mut self, index: usize, val: &[u8]) -> Result<(), PageError> {
        // 1. Append cell data
        let offset = self.append_cell(val)?;

        // 2. Insert slot
        let slot_area_start = PAGE_HEADER_SIZE_USIZE;
        let count = self.header.cell_count as usize;

        if index > count {
            return Err(PageError::InvalidIndex);
        }

        // Shift slots if necessary
        let insert_pos = slot_area_start + (index * SLOT_SIZE);
        if self.data.len() < insert_pos + SLOT_SIZE {
            return Err(PageError::NoSpace); // Should cover src_start..src_end too if valid
        }

        if index < count {
            let src_start = insert_pos;
            let src_end = slot_area_start + (count * SLOT_SIZE);
            let dest_start = insert_pos + SLOT_SIZE;

            if self.data.len() < dest_start + (src_end - src_start) {
                return Err(PageError::NoSpace);
            }
            self.data.copy_within(src_start..src_end, dest_start);
        }

        // Write new slot
        if insert_pos + 2 > self.data.len() {
            return Err(PageError::NoSpace);
        }
        self.data[insert_pos..insert_pos + 2].copy_from_slice(&offset.to_le_bytes());

        // Update header
        self.header.cell_count += 1;
        self.header.free_start += u16::try_from(SLOT_SIZE).map_err(|_| PageError::NoSpace)?;
        self.commit();

        Ok(())
    }

    // assumes all cells start with a 4-byte length header
    // This creates tight coupling between SlottedPage (a generic page structure)
    // and the specific cell format used by BPlusTreeNode.
    pub fn get_cell(&self, index: usize) -> Option<&[u8]> {
        if index >= self.header.cell_count as usize {
            return None;
        }
        let slot_pos = PAGE_HEADER_SIZE_USIZE + (index * SLOT_SIZE);
        // Safe slice access
        if slot_pos + 2 > self.data.len() { return None; }
        let offset = u16::from_le_bytes(self.data[slot_pos..slot_pos + 2].try_into().ok()?);

        // Bounds check for length header
        if (offset as usize) + 4 > self.data.len() { return None; }
        let len = u32::from_le_bytes(self.data[offset as usize..offset as usize + 4].try_into().ok()?) as usize;

        if (offset as usize) + 4 + len > self.data.len() { return None; }
        Some(&self.data[offset as usize..offset as usize + 4 + len])
    }

    pub fn get_cell_offset(&self, index: usize) -> Option<u16> {
        let slot_pos = PAGE_HEADER_SIZE_USIZE + (index * SLOT_SIZE);
        if slot_pos + 2 > self.data.len() { return None; }
        Some(u16::from_le_bytes(self.data[slot_pos..slot_pos + 2].try_into().ok()?))
    }

    pub fn compact(&mut self) -> Result<(), PageError> {
        let mut temp = vec![0u8; PAGE_SIZE_USIZE];
        {
            let mut new_page = SlottedPage::new(&mut temp, self.header.page_type)?;
            for i in 0..self.header.cell_count as usize {
                if let Some(cell) = self.get_cell(i) {
                    if let Err(e) = new_page.insert_at_index(i, cell) {
                        error!("Compact insert failed at index {i}: {e:?}");
                        return Err(e);
                    }
                } else {
                    error!("Compact get_cell failed at index {i}");
                    return Err(PageError::Corrupted);
                }
            }
        }
        self.data.copy_from_slice(&temp);
        self.header = PageHeader::deserialize(&self.data[..PAGE_HEADER_SIZE_USIZE])?;
        Ok(())
    }

    pub fn split_off(&mut self) -> Result<Option<Vec<u8>>, PageError> {
        let count = self.header.cell_count as usize;
        let mut total_bytes = 0;
        let mut split_idx = count / 2;

        let mut sizes = Vec::with_capacity(count);
        for i in 0..count {
            if let Some(cell) = self.get_cell(i) {
                sizes.push(cell.len());
                total_bytes += cell.len();
            } else {
                sizes.push(0);
            }
        }

        let target = total_bytes / 2;
        let mut current = 0;
        for (i, &s) in sizes.iter().enumerate() {
            current += s;
            if current >= target {
                split_idx = i + 1;
                break;
            }
        }

        // Fix for split logic:
        if count == 0 {
            return Err(PageError::InvalidIndex); // Cannot split empty page
        }
        if count == 1 {
            // Cannot split single item fundamentally.
            // Return Ok(None) explicitly to indicate no-op.
            return Ok(None);
        }

        if split_idx >= count { split_idx = count.saturating_sub(1); }
        if split_idx == 0 && count > 1 { split_idx = 1; }

        let mut new_buffer = vec![0u8; PAGE_SIZE_USIZE];
        {
            let mut new_page = SlottedPage::new(&mut new_buffer, self.header.page_type)?;
            for i in split_idx..count {
                if let Some(cell) = self.get_cell(i) {
                    new_page.insert_at_index(i - split_idx, cell)?;
                }
            }
        }

        self.header.cell_count = u16::try_from(split_idx).map_err(|_| PageError::InvalidIndex)?;
        let new_free_start = PAGE_HEADER_SIZE_USIZE + split_idx * SLOT_SIZE;
        self.header.free_start = u16::try_from(new_free_start).map_err(|_| PageError::NoSpace)?;
        self.commit();

        self.compact()?;

        Ok(Some(new_buffer))
    }
}

#[inline]
fn u32_from_bytes(bytes: &[u8]) -> io::Result<u32> {
    Ok(u32::from_le_bytes(bytes.try_into().map_err(to_io_error)?))
}

#[inline]
fn get_entry_index_upper_bound<K>(keys: &[K], key: &K) -> usize
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
{
    let mut left = 0;
    let mut right = keys.len();
    while left < right {
        let mid = left + ((right - left) >> 1);
        if &keys[mid] <= key {
            left = mid + 1;
        } else {
            right = mid;
        }
    }
    left
}

// Adaptively compress value bytes if beneficial.
// Returns (compression_flag, payload_bytes).
fn compress_if_beneficial(raw_bytes: &[u8]) -> (u8, Vec<u8>) {
    if raw_bytes.len() >= COMPRESSION_MIN_SIZE {
        let compressed = lz4_flex::compress_prepend_size(raw_bytes);
        let threshold = (raw_bytes.len() * COMPRESSION_THRESHOLD_PERCENT) / 100;

        if compressed.len() < threshold {
            // Compression is effective
            (COMPRESSION_FLAG_LZ4, compressed)
        } else {
            // Compression not worth it - Return copy of raw
            (COMPRESSION_FLAG_NONE, raw_bytes.to_vec())
        }
    } else {
        // Too small to compress - Return copy of raw
        (COMPRESSION_FLAG_NONE, raw_bytes.to_vec())
    }
}


/// Represents how a value is stored on disk
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum ValueStorageMode {
    /// Multiple small values packed in one block
    /// (`block_offset`, `value_index_in_block`)
    Packed(u64, u16),

    /// Single value in dedicated block(s)
    /// (`block_offset`)
    Single(u64),
}

/// Extended value info that includes storage mode and length
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ValueInfo {
    mode: ValueStorageMode,
    length: u32,
    #[serde(skip)]
    compressed_cache: Option<(u8, Vec<u8>)>, // (flag, payload)
}


#[derive(Debug, Clone)]
struct BPlusTreeNode<K, V> {
    keys: Vec<K>,
    children: Vec<BPlusTreeNode<K, V>>,
    is_leaf: bool,
    value_info: Vec<ValueInfo>,
    values: Vec<V>, // only used in leaf nodes
}

impl<K, V> BPlusTreeNode<K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    #[inline]
    const fn new(is_leaf: bool) -> Self {
        Self {
            is_leaf,
            keys: vec![],
            children: vec![],
            value_info: vec![],
            values: vec![],
        }
    }

    #[inline]
    fn is_overflow(&self, order: usize) -> bool {
        self.keys.len() > order
    }

    #[inline]
    const fn get_median_index(order: usize) -> usize {
        order >> 1
    }

    fn find_leaf_entry(node: &Self) -> Option<&K> {
        if node.is_leaf {
            node.keys.first()
        } else if let Some(child) = node.children.first() {
            Self::find_leaf_entry(child)
        } else {
            None
        }
    }

    fn query(&self, key: &K) -> Option<&V> {
        if self.is_leaf {
            return self.keys.binary_search(key).map_or(None, |idx| self.values.get(idx));
        }
        self.children.get(self.get_entry_index_upper_bound(key))?.query(key)
    }

    fn get_entry_index_upper_bound(&self, key: &K) -> usize {
        get_entry_index_upper_bound::<K>(&self.keys, key)
    }

    fn insert(&mut self, key: K, v: V, inner_order: usize, leaf_order: usize) -> Option<Self> {
        if self.is_leaf {
            // Use single binary search instead of redundant searches
            match self.keys.binary_search(&key) {
                Ok(pos) => {
                    // Key exists, update value
                    self.values[pos] = v;
                    return None;
                }
                Err(pos) => {
                    // Key doesn't exist, insert at the correct position
                    self.keys.insert(pos, key);
                    self.values.insert(pos, v);
                    if self.is_overflow(leaf_order) {
                        return Some(self.split(leaf_order));
                    }
                }
            }
        } else {
            let pos = self.get_entry_index_upper_bound(&key);
            let child = self.children.get_mut(pos)?;
            let node = child.insert(key.clone(), v, inner_order, leaf_order);
            if let Some(tree_node) = node {
                if let Some(leaf_key) = Self::find_leaf_entry(&tree_node) {
                    let idx = self.get_entry_index_upper_bound(leaf_key);
                    if self.keys.binary_search(leaf_key).is_err() {
                        self.keys.insert(idx, leaf_key.clone());
                        self.children.insert(idx + 1, tree_node);
                        if self.is_overflow(inner_order) {
                            return Some(self.split(inner_order));
                        }
                    }
                }
            }
        }
        None
    }

    fn split(&mut self, order: usize) -> Self {
        let median = Self::get_median_index(order);
        if self.is_leaf {
            let mut node = Self::new(true);
            node.keys = self.keys.split_off(median);
            node.values = self.values.split_off(median);
            node
        } else {
            let mut node = Self::new(false);
            node.keys = self.keys.split_off(median + 1);
            node.children = self.children.split_off(median + 1);
            // No need to clone and push - split_off already handles the split correctly
            node
        }
    }

    /// Find the largest key <= `key` in this subtree.
    /// Returns a reference to (key, value) if found (only valid for leaf entries).
    fn find_le(&self, key: &K) -> Option<(&K, &V)> {
        if self.is_leaf {
            // find index of first key > key, then step one back
            let idx = self.get_entry_index_upper_bound(key);
            if idx == 0 {
                None
            } else {
                let i = idx - 1;
                // safe: leaf guarantees values.len() == keys.len()
                Some((&self.keys[i], &self.values[i]))
            }
        } else {
            // descend into the appropriate child (child index = upper_bound)
            let child_idx = self.get_entry_index_upper_bound(key);
            // child_idx can be equal to children.len() if key > all keys; children.get handles that
            if let Some(child) = self.children.get(child_idx) {
                child.find_le(key)
            } else {
                // fallback: if child_idx is out of bounds, try last child (defensive)
                self.children.last().and_then(|c| c.find_le(key))
            }
        }
    }

    pub fn len(&self) -> usize {
        if self.is_leaf {
            self.keys.len()
        } else {
            self.children.iter().map(BPlusTreeNode::len).sum()
        }
    }

    pub fn traverse<F>(&self, visit: &mut F)
    where
        F: FnMut(&Vec<K>, &Vec<V>),
    {
        if self.is_leaf {
            visit(&self.keys, &self.values);
        }
        self.children.iter().for_each(|child| child.traverse(visit));
    }

    /// Write a packed value block to disk
    fn write_packed_block<W: Write + Seek>(
        file: &mut W,
        buffer: &mut [u8],
        offset: u64,
        values: &[(u16, &[u8])],
    ) -> io::Result<()> {
        file.seek(SeekFrom::Start(offset))?;

        // Write count
        let count = u32::try_from(values.len()).map_err(to_io_error)?;
        buffer[0..4].copy_from_slice(&count.to_le_bytes());
        let mut pos = 4;

        // Write each value: length + data
        for (_, value_bytes) in values {
            let len = u32::try_from(value_bytes.len()).map_err(to_io_error)?;
            buffer[pos..pos + 4].copy_from_slice(&len.to_le_bytes());
            pos += 4;
            buffer[pos..pos + value_bytes.len()].copy_from_slice(value_bytes);
            pos += value_bytes.len();
        }

        // Zero remaining space
        if pos < PAGE_SIZE_USIZE {
            buffer[pos..PAGE_SIZE_USIZE].fill(0u8);
        }

        file.write_all(&buffer[..PAGE_SIZE_USIZE])?;
        Ok(())
    }

    /// Calculate the serialized size of this node in bytes (rounded up to block size)
    fn calculate_serialized_size(&self) -> io::Result<u64> {
        // Header: is_leaf flag
        let mut size = FLAG_SIZE;

        // Keys: length + serialized data
        let keys_encoded = binary_serialize(&self.keys)?;
        size += LEN_SIZE + keys_encoded.len();

        if self.is_leaf {
            // Leaf nodes now store value_info instead of values
            // value_info: length + Vec<(u64, u32)>
            let info_encoded = binary_serialize(&self.value_info)?;
            size += LEN_SIZE + info_encoded.len();
        } else {
            // Internal node: pointer length + pointers
            // We use current children count for estimate
            let mut pointer_vec: Vec<u64> = vec![0; self.children.len()];
            pointer_vec.resize(self.children.len(), 0);
            let pointer_encoded = binary_serialize(&pointer_vec)?;
            size += LEN_SIZE + pointer_encoded.len();
        }

        // Round up to block size
        let blocks = size.div_ceil(PAGE_SIZE_USIZE);
        Ok((blocks * PAGE_SIZE_USIZE) as u64)
    }

    fn serialize_to_block<W: Write + Seek>(
        &self,
        file: &mut W,
        buffer: &mut Vec<u8>,
        offset: u64,
    ) -> io::Result<u64> {
        let keys_encoded = binary_serialize(&self.keys)?;
        let keys_len = u32::try_from(keys_encoded.len()).map_err(to_io_error)?;

        if self.is_leaf {
            let info_encoded = binary_serialize(&self.value_info)?;
            let info_len = u32::try_from(info_encoded.len()).map_err(to_io_error)?;

            let content_size = FLAG_SIZE + LEN_SIZE + keys_encoded.len() + LEN_SIZE + info_encoded.len();
            let blocks = content_size.div_ceil(PAGE_SIZE_USIZE);

            file.seek(SeekFrom::Start(offset))?;

            let capacity = blocks * PAGE_SIZE_USIZE;
            if buffer.len() < capacity {
                buffer.resize(capacity, 0);
            }
            buffer[..capacity].fill(0);

            let mut pos = 0;
            buffer[pos] = 1u8;
            pos += FLAG_SIZE;

            buffer[pos..pos + LEN_SIZE].copy_from_slice(&keys_len.to_le_bytes());
            pos += LEN_SIZE;

            buffer[pos..pos + keys_encoded.len()].copy_from_slice(&keys_encoded);
            pos += keys_encoded.len();

            buffer[pos..pos + LEN_SIZE].copy_from_slice(&info_len.to_le_bytes());
            pos += LEN_SIZE;

            buffer[pos..pos + info_encoded.len()].copy_from_slice(&info_encoded);

            file.write_all(buffer)?;

            Ok(offset + (blocks as u64 * PAGE_SIZE_USIZE as u64))
        } else {
            let ptr_count = self.children.len();
            let ptr_encoded_size = 8 + 8 * ptr_count;

            let content_size = FLAG_SIZE + LEN_SIZE + keys_encoded.len() + LEN_SIZE + ptr_encoded_size;
            let blocks_needed = content_size.div_ceil(PAGE_SIZE_USIZE);

            let parent_start = offset;
            let mut current_offset = parent_start + (blocks_needed as u64 * PAGE_SIZE_USIZE as u64);

            let mut pointers = Vec::with_capacity(ptr_count);
            for child in &self.children {
                pointers.push(current_offset);
                current_offset = child.serialize_to_block(file, buffer, current_offset)?;
            }

            let pointers_encoded = binary_serialize(&pointers)?;
            let pointers_len = u32::try_from(pointers_encoded.len()).map_err(to_io_error)?;

            file.seek(SeekFrom::Start(parent_start))?;
            let mut data = Vec::with_capacity(blocks_needed * PAGE_SIZE_USIZE);
            data.push(0u8);
            data.extend_from_slice(&keys_len.to_le_bytes());
            data.extend_from_slice(&keys_encoded);
            data.extend_from_slice(&pointers_len.to_le_bytes());
            data.extend_from_slice(&pointers_encoded);

            let pad_len = (blocks_needed * PAGE_SIZE_USIZE) - data.len();
            if pad_len > 0 {
                data.extend(std::iter::repeat_n(0, pad_len));
            }
            file.write_all(&data)?;

            Ok(current_offset)
        }
    }

    /// Serialize the tree in breadth-first order for better disk locality
    /// This improves query performance by keeping nodes at the same level contiguous
    #[allow(clippy::too_many_lines)]
    fn serialize_breadth_first<W: Write + Seek>(
        &mut self,
        file: &mut W,
        buffer: &mut Vec<u8>,
        start_offset: u64,
    ) -> io::Result<u64> {
        use std::collections::HashMap;

        // Pass 1: Populate value_info for all leaf nodes (Mutable)
        // This calculates value sizes and determines packing WITHOUT assigning final offsets yet.
        // We use placeholder offsets (0) which will be corrected after node layout is determined.
        {
            let mut current_level_mut = vec![&mut *self];
            while !current_level_mut.is_empty() {
                let mut next_level_mut = Vec::new();
                for node in current_level_mut {
                    if node.is_leaf {
                        node.value_info.clear();

                        // Serialize all values first to determine sizes
                        let mut serialized_values: Vec<Vec<u8>> = Vec::new();
                        for value in &node.values {
                            let value_bytes = binary_serialize(value)?;
                            serialized_values.push(value_bytes);
                        }

                        // Determine packing structure with placeholder offsets (0)
                        // Final offsets will be assigned in Pass 3
                        let mut current_pack_index: u16 = 0;
                        let mut current_pack_size = PACK_BLOCK_HEADER_SIZE;
                        let mut pack_count = 0u32; // Track which pack block we're on

                        for value_bytes in serialized_values {
                            let size = value_bytes.len();

                            if size <= SMALL_VALUE_THRESHOLD {
                                let entry_size = PACK_VALUE_HEADER_SIZE + size;

                                if current_pack_size + entry_size <= PAGE_SIZE_USIZE {
                                    // Add to current pack
                                    node.value_info.push(ValueInfo {
                                        mode: ValueStorageMode::Packed(u64::from(pack_count), current_pack_index),
                                        length: u32::try_from(size).map_err(to_io_error)?,
                                        compressed_cache: None,
                                    });
                                    current_pack_index += 1;
                                    current_pack_size += entry_size;
                                } else {
                                    // Start new pack
                                    pack_count += 1;
                                    current_pack_index = 1;
                                    current_pack_size = PACK_BLOCK_HEADER_SIZE + entry_size;

                                    node.value_info.push(ValueInfo {
                                        mode: ValueStorageMode::Packed(u64::from(pack_count), 0),
                                        length: u32::try_from(size).map_err(to_io_error)?,
                                        compressed_cache: None,
                                    });
                                }
                            } else {
                                // Large value - use Single storage with optional compression
                                // Pre-calculate compressed size if applicable
                                let (flag, payload) = compress_if_beneficial(&value_bytes);
                                let stored_size = 1 + payload.len(); // flag + payload

                                let cache = if flag == COMPRESSION_FLAG_LZ4 {
                                    Some((flag, payload))
                                } else {
                                    None // Don't cache uncompressed data to save memory
                                };

                                node.value_info.push(ValueInfo {
                                    mode: ValueStorageMode::Single(u64::MAX), // Placeholder
                                    length: u32::try_from(stored_size).map_err(to_io_error)?,
                                    compressed_cache: cache,
                                });
                            }
                        }
                    } else {
                        for child in &mut node.children {
                            next_level_mut.push(child);
                        }
                    }
                }
                current_level_mut = next_level_mut;
            }
        }

        // Pass 2: Calculate offsets for all nodes in breadth-first order (Immutable)
        // Now value_info is populated, so calculate_serialized_size() returns correct sizes
        let mut node_offset_map: HashMap<*const BPlusTreeNode<K, V>, u64> = HashMap::new();
        let mut current_offset = start_offset;

        {
            let mut current_level = vec![&*self];
            node_offset_map.insert(std::ptr::from_ref(self), current_offset);
            current_offset += self.calculate_serialized_size()?;

            while !current_level.is_empty() {
                let mut next_level = Vec::new();
                for node in current_level {
                    if !node.is_leaf {
                        for child in &node.children {
                            let child_ptr = std::ptr::from_ref(child);
                            node_offset_map.insert(child_ptr, current_offset);
                            current_offset += child.calculate_serialized_size()?;
                            next_level.push(child);
                        }
                    }
                }
                current_level = next_level;
            }
        }

        // Pass 3: Assign final value block offsets and update value_info (Mutable)
        // current_offset now points past all nodes, we can allocate value blocks here
        {
            let mut current_level_mut = vec![&mut *self];
            while !current_level_mut.is_empty() {
                let mut next_level_mut = Vec::new();
                for node in current_level_mut {
                    if node.is_leaf {
                        // Track pack block offsets: pack_count -> actual_offset
                        let mut pack_block_offsets: HashMap<u64, u64> = HashMap::new();

                        // First pass: assign offsets to pack blocks and single values
                        for info in &mut node.value_info {
                            match &mut info.mode {
                                ValueStorageMode::Packed(pack_idx, _index) => {
                                    if !pack_block_offsets.contains_key(pack_idx) {
                                        pack_block_offsets.insert(*pack_idx, current_offset);
                                        current_offset += PAGE_SIZE_USIZE as u64;
                                    }
                                }
                                ValueStorageMode::Single(offset) if *offset == u64::MAX => {
                                    // Assign actual offset for single value (byte-aligned)
                                    *offset = current_offset;
                                    // info.length already contains the correct stored size
                                    current_offset += u64::from(info.length);
                                }
                                ValueStorageMode::Single(_) => {}
                            }
                        }

                        // Second pass: update pack indices to actual offsets
                        for info in &mut node.value_info {
                            if let ValueStorageMode::Packed(pack_idx, index) = &mut info.mode {
                                let actual_offset = pack_block_offsets[pack_idx];
                                *pack_idx = actual_offset;
                                // index stays the same
                                let _ = index; // Silence unused warning
                            }
                        }
                    } else {
                        for child in &mut node.children {
                            next_level_mut.push(child);
                        }
                    }
                }
                current_level_mut = next_level_mut;
            }
        }

        // Pass 4: Write nodes with their keys and value pointers (Immutable)
        {
            let mut current_level_indices = vec![&*self];
            while !current_level_indices.is_empty() {
                let mut next_level = Vec::new();
                for node in current_level_indices {
                    let node_ptr = std::ptr::from_ref(node);
                    let node_offset = node_offset_map[&node_ptr];

                    if node.is_leaf {
                        node.serialize_to_block(file, buffer, node_offset)?;
                    } else {
                        let child_offsets: Vec<u64> = node.children.iter()
                            .map(|c| node_offset_map[&std::ptr::from_ref(c)])
                            .collect();

                        node.serialize_internal_with_offsets(file, buffer, node_offset, &child_offsets)?;
                        for child in &node.children {
                            next_level.push(child);
                        }
                    }
                }
                current_level_indices = next_level;
            }
        }

        // Pass 5: Write all value blocks (packed and single) (Immutable)
        {
            let mut current_level_values = vec![&*self];
            while !current_level_values.is_empty() {
                let mut next_level = Vec::new();
                for node in current_level_values {
                    if node.is_leaf {
                        // Group values by their storage location
                        let mut pack_blocks: HashMap<u64, Vec<(u16, Vec<u8>)>> = HashMap::new();

                        for (value, info) in node.values.iter().zip(node.value_info.iter()) {
                            let value_bytes = binary_serialize(value)?;

                            match info.mode {
                                ValueStorageMode::Packed(block_offset, index) => {
                                    pack_blocks.entry(block_offset)
                                        .or_default()
                                        .push((index, value_bytes));
                                }
                                ValueStorageMode::Single(block_offset) => {
                                    // Write single value with compression format
                                    file.seek(SeekFrom::Start(block_offset))?;

                                    // Apply adaptive compression or use cache
                                    let (flag, payload_ref) = if let Some((c_flag, c_payload)) = &info.compressed_cache {
                                        (*c_flag, c_payload.as_slice())
                                    } else {
                                        // If not cached, it means it wasn't beneficial (or we chose not to cache it)
                                        // So we write raw bytes with NONE flag
                                        (COMPRESSION_FLAG_NONE, value_bytes.as_slice())
                                    };

                                    // Write: [flag:1][payload]
                                    file.write_all(&[flag])?;
                                    file.write_all(payload_ref)?;
                                }
                            }
                        }

                        // Write packed blocks
                        for (block_offset, mut values) in pack_blocks {
                            // Sort by index to ensure correct order
                            values.sort_by_key(|(idx, _)| *idx);

                            // Convert to slice references
                            let value_refs: Vec<(u16, &[u8])> = values.iter()
                                .map(|(idx, bytes)| (*idx, bytes.as_slice()))
                                .collect();

                            Self::write_packed_block(file, buffer, block_offset, &value_refs)?;
                        }
                    } else {
                        for child in &node.children {
                            next_level.push(child);
                        }
                    }
                }
                current_level_values = next_level;
            }
        }

        Ok(current_offset)
    }

    /// Serialize an internal node with pre-calculated child offsets
    fn serialize_internal_with_offsets<W: Write + Seek>(
        &self,
        file: &mut W,
        buffer: &mut [u8],
        offset: u64,
        child_offsets: &[u64],
    ) -> io::Result<u64> {
        // Similar to serialize_to_block but for internal nodes with known child offsets
        let buffer_slice = &mut buffer[..];
        buffer_slice[0] = u8::from(self.is_leaf);
        let mut write_pos = FLAG_SIZE;

        // Write keys
        let keys_encoded = binary_serialize(&self.keys)?;
        let keys_len = keys_encoded.len();
        buffer_slice[write_pos..write_pos + LEN_SIZE]
            .copy_from_slice(&u32::try_from(keys_len).map_err(to_io_error)?.to_le_bytes());
        write_pos += LEN_SIZE;
        buffer_slice[write_pos..write_pos + keys_len].copy_from_slice(&keys_encoded);
        write_pos += keys_len;
        drop(keys_encoded);

        let pointer_offset_within_first_block = offset + write_pos as u64;

        // Zero unused portion and write first block
        if write_pos < PAGE_SIZE_USIZE {
            buffer_slice[write_pos..PAGE_SIZE_USIZE].fill(0u8);
        }
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(&buffer_slice[..PAGE_SIZE_USIZE])?;

        // Write child pointers
        let pointer_encoded = binary_serialize(child_offsets)?;
        let pointer_len = u32::try_from(pointer_encoded.len()).map_err(to_io_error)?;

        // CRITICAL CHECK: Ensure pointers fit in the remaining space of the first block or we've allocated enough
        if write_pos + LEN_SIZE + pointer_encoded.len() > PAGE_SIZE_USIZE {
            return Err(io::Error::other(format!("Internal node overflow: keys ({}) + pointers ({}) exceeds block size. keys sizes might be too large for the current PAGE_SIZE ({}). Consider reducing key sizes or increasing PAGE_SIZE.", keys_len, pointer_encoded.len(), PAGE_SIZE_USIZE)));
        }

        file.seek(SeekFrom::Start(pointer_offset_within_first_block))?;
        file.write_all(&pointer_len.to_le_bytes())?;
        file.write_all(&pointer_encoded)?;

        Ok(offset + PAGE_SIZE_USIZE as u64)
    }

    fn deserialize_from_block<R: Read + Seek>(
        file: &mut R,
        buffer: &mut Vec<u8>,
        offset: u64,
        nested: bool,
    ) -> io::Result<(Self, Option<Vec<u64>>)> {
        file.seek(SeekFrom::Start(offset))?;

        let header_required = FLAG_SIZE + LEN_SIZE;
        if buffer.len() < header_required {
            buffer.resize(header_required, 0);
        }

        file.read_exact(&mut buffer[0..header_required])?;

        let is_leaf = buffer[0] != 0;
        #[allow(clippy::range_plus_one)]
        let keys_len = u32_from_bytes(&buffer[FLAG_SIZE..FLAG_SIZE + LEN_SIZE])? as usize;

        let min_required = header_required + keys_len + LEN_SIZE;
        if buffer.len() < min_required {
            buffer.resize(min_required, 0);
        }

        file.read_exact(&mut buffer[header_required..min_required])?;

        let mut read_pos = header_required;
        let keys: Vec<K> = binary_deserialize(&buffer[read_pos..read_pos + keys_len])?;
        read_pos += keys_len;

        let payload_len = u32_from_bytes(&buffer[read_pos..read_pos + LEN_SIZE])? as usize;
        read_pos += LEN_SIZE;

        let total_required = min_required + payload_len;
        if buffer.len() < total_required {
            buffer.resize(total_required, 0);
        }

        file.read_exact(&mut buffer[min_required..total_required])?;

        let (value_info, values, children, children_pointer) = if is_leaf {
            let info: Vec<ValueInfo> = binary_deserialize(&buffer[read_pos..read_pos + payload_len])?;
            let vals = if nested {
                let mut v = Vec::with_capacity(info.len());
                for i in &info {
                    v.push(Self::load_value_from_info(file, i)?);
                }
                v
            } else {
                Vec::new()
            };
            (info, vals, Vec::new(), None)
        } else {
            let pointers: Vec<u64> = binary_deserialize(&buffer[read_pos..read_pos + payload_len])?;
            let nodes = if nested {
                let mut n = Vec::with_capacity(pointers.len());
                let mut child_buf = Vec::with_capacity(PAGE_SIZE_USIZE);
                for &ptr in &pointers {
                    let (child, _) = Self::deserialize_from_block(file, &mut child_buf, ptr, nested)?;
                    n.push(child);
                }
                n
            } else {
                Vec::new()
            };
            (Vec::new(), Vec::new(), nodes, Some(pointers))
        };

        Ok((Self { keys, children, is_leaf, value_info, values }, children_pointer))
    }

    fn deserialize_from_mmap<R: Read + Seek>(
        mmap: &[u8],
        file: &mut R,
        offset: u64,
        nested: bool,
    ) -> io::Result<(Self, Option<Vec<u64>>)> {
        let start = usize::try_from(offset).map_err(to_io_error)?;
        let header_end = start.checked_add(FLAG_SIZE + LEN_SIZE)
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "Mmap offset overflow"))?;
        // Basic safety check for mmap bounds
        if header_end > mmap.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Mmap access out of bounds"));
        }

        let keys_len = u32_from_bytes(&mmap[start + FLAG_SIZE..start + FLAG_SIZE + LEN_SIZE])? as usize;
        let keys_start = header_end;
        let len_pos = keys_start
            .checked_add(keys_len)
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "Mmap offset overflow"))?;

        if len_pos + LEN_SIZE > mmap.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Mmap access out of bounds"));
        }
        let payload_len = u32_from_bytes(&mmap[len_pos..len_pos + LEN_SIZE])? as usize;
        let total = len_pos
            .checked_add(LEN_SIZE + payload_len)
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "Mmap offset overflow"))?;

        if total > mmap.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Mmap access out of bounds"));
        }

        // We need to know the total size of the node to slice the mmap
        // For simplicity, we can just slice a PAGE_SIZE or slightly more if we know it overflows.
        // Actually, our serialize_to_block uses PAGE_SIZE blocks.

        //let slice = &mmap[start..];
        let slice = &mmap[start..total];
        Self::deserialize_from_block_slice(slice, file, nested)
    }

    fn deserialize_from_block_slice<R: Read + Seek>(
        slice: &[u8],
        file: &mut R,
        nested: bool,
    ) -> io::Result<(Self, Option<Vec<u64>>)> {
        // Node type
        let is_leaf = slice[0] == 1u8;
        let mut read_pos = FLAG_SIZE;

        // ---- Keys ----
        let keys_length = u32_from_bytes(&slice[read_pos..read_pos + LEN_SIZE])? as usize;
        read_pos += LEN_SIZE;
        let keys: Vec<K> = binary_deserialize(&slice[read_pos..read_pos + keys_length])?;
        read_pos += keys_length;

        // ---- Value info (offset, length) for leaf nodes ----
        let (value_info, values): (Vec<ValueInfo>, Vec<V>) = if is_leaf {
            // Read value_info
            let info_length = u32_from_bytes(&slice[read_pos..read_pos + LEN_SIZE])? as usize;
            read_pos += LEN_SIZE;
            let info: Vec<ValueInfo> = binary_deserialize(&slice[read_pos..read_pos + info_length])?;

            // Values are loaded on-demand when nested=true
            if nested {
                let mut vals = Vec::with_capacity(info.len());
                for value_info in &info {
                    let value = Self::load_value_from_info(file, value_info)?;
                    vals.push(value);
                }
                (info, vals)
            } else {
                (info, Vec::new())
            }
        } else {
            (Vec::new(), Vec::new())
        };

        // ---- Pointers for internal nodes ----
        let (children, children_pointer): (Vec<Self>, Option<Vec<u64>>) = if is_leaf {
            (Vec::new(), None)
        } else {
            let pointers_length = u32_from_bytes(&slice[read_pos..read_pos + LEN_SIZE])? as usize;
            read_pos += LEN_SIZE;
            let pointers: Vec<u64> = binary_deserialize(&slice[read_pos..read_pos + pointers_length])?;
            if nested {
                let mut nodes = Vec::with_capacity(pointers.len());
                let mut child_buffer = vec![0u8; PAGE_SIZE_USIZE];
                for &ptr in &pointers {
                    let (child, _) = Self::deserialize_from_block(file, &mut child_buffer, ptr, nested)?;
                    nodes.push(child);
                }
                (nodes, None)
            } else {
                (Vec::new(), Some(pointers))
            }
        };

        Ok((Self { keys, children, is_leaf, value_info, values }, children_pointer))
    }

    /// Load a value based on its storage info
    fn load_value_from_info<R: Read + Seek>(file: &mut R, info: &ValueInfo) -> io::Result<V> {
        match info.mode {
            ValueStorageMode::Single(offset) => {
                // Load single value (existing logic)
                Self::load_value_with_len(file, offset, info.length)
            }
            ValueStorageMode::Packed(block_offset, index) => {
                // Load from packed block
                Self::load_value_from_packed_block(file, block_offset, index, info.length)
            }
        }
    }

    /// Load a value from a packed block
    fn load_value_from_packed_block<R: Read + Seek>(
        file: &mut R,
        block_offset: u64,
        value_index: u16,
        _expected_length: u32,
    ) -> io::Result<V> {
        file.seek(SeekFrom::Start(block_offset))?;

        let mut block_buffer = vec![0u8; PAGE_SIZE_USIZE];
        file.read_exact(&mut block_buffer)?;

        // Read count
        let count = u32::from_le_bytes(block_buffer[0..4].try_into().map_err(to_io_error)?);
        let mut pos = 4;

        // Skip to target value
        for i in 0..=value_index {
            if pos + 4 > PAGE_SIZE_USIZE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Packed block corrupted: position {pos} exceeds block size"),
                ));
            }

            let len = u32::from_le_bytes(block_buffer[pos..pos + 4].try_into().map_err(to_io_error)?) as usize;
            pos += 4;

            if i == value_index {
                // Found target value
                if pos + len > PAGE_SIZE_USIZE {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("Packed value corrupted: length {len} at position {pos} exceeds block size"),
                    ));
                }
                let value_data = &block_buffer[pos..pos + len];
                return binary_deserialize(value_data);
            }

            pos += len;
        }

        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Value index {value_index} not found in packed block (count: {count})"),
        ))
    }

    fn load_value_with_len<R: Read + Seek>(file: &mut R, offset: u64, stored_len: u32) -> io::Result<V> {
        file.seek(SeekFrom::Start(offset))?;

        // Read compression flag
        let mut flag = [0u8; 1];
        file.read_exact(&mut flag)?;

        let data = if flag[0] == COMPRESSION_FLAG_LZ4 {
            // Compressed: [flag:1][lz4_payload_with_prepended_size]
            // We do NOT store explicit original length anymore, it's inside LZ4 blob

            let compressed_len = stored_len as usize - 1; // 1 (flag)
            let mut compressed = vec![0u8; compressed_len];
            file.read_exact(&mut compressed)?;

            lz4_flex::decompress_size_prepended(&compressed)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("LZ4 decompression failed: {e}")))?
        } else {
            // Uncompressed: [flag:1][payload]
            let payload_len = stored_len as usize - 1;
            let mut data = vec![0u8; payload_len];
            file.read_exact(&mut data)?;
            data
        };

        binary_deserialize(&data)
    }
}
// -----------------------------------------------------------------------------
// Metadata Enum
// -----------------------------------------------------------------------------
#[derive(Clone, Debug, PartialEq)]
pub enum BPlusTreeMetadata {
    Empty,
    TargetIdMapping(u32),
}

impl BPlusTreeMetadata {
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::Empty => Vec::new(),
            Self::TargetIdMapping(val) => {
                let mut bytes = vec![MAGIC_METADATA_TARGET_ID_MAPPING]; // Type tag
                bytes.extend_from_slice(&val.to_le_bytes());
                bytes
            }
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> Self {
        match bytes.len() {
            5 if bytes[0] == MAGIC_METADATA_TARGET_ID_MAPPING => {
                let arr: [u8; 4] = bytes[1..5].try_into().unwrap_or([0; 4]);
                Self::TargetIdMapping(u32::from_le_bytes(arr))
            }
            _ => Self::Empty, // Unknown metadata treated as Empty for now
        }
    }
}

#[derive(Debug, Clone)]
pub struct BPlusTree<K, V> {
    root: BPlusTreeNode<K, V>,
    inner_order: usize,
    leaf_order: usize,
    metadata: BPlusTreeMetadata,
    dirty: bool,
}

const fn calc_order<K>() -> (usize, usize) {
    // Internal: FLAG (1) + LEN_K (4) + KEYS + LEN_P (4) + POINTERS (8 each)
    // Leaf:    FLAG (1) + LEN_K (4) + KEYS + LEN_INFO (4) + VALUE_INFO (12 each)

    let base_overhead = FLAG_SIZE + LEN_SIZE + LEN_SIZE + 64; // flag + keys_len + info_len + safety buffer
    let key_size = size_of::<K>();

    let inner_order = (PAGE_SIZE_USIZE - base_overhead) / (key_size + POINTER_SIZE + MSGPACK_OVERHEAD_PER_ENTRY);
    let leaf_order = (PAGE_SIZE_USIZE - base_overhead) / (key_size + INFO_SIZE + MSGPACK_OVERHEAD_PER_ENTRY);

    // Ensure we have at least a minimal order (manual max for const fn)
    let final_inner = if inner_order < 2 { 2 } else { inner_order };
    let final_leaf = if leaf_order < 2 { 2 } else { leaf_order };
    (final_inner, final_leaf)
}

impl<K, V> Default for BPlusTree<K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> BPlusTree<K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub const fn new() -> Self {
        let (inner_order, leaf_order) = calc_order::<K>();
        Self {
            root: BPlusTreeNode::<K, V>::new(true),
            inner_order,
            leaf_order,
            metadata: BPlusTreeMetadata::Empty,
            dirty: true, // an empty tree is stored!
        }
    }

    /// Helper to access metadata
    pub fn get_metadata(&self) -> &BPlusTreeMetadata {
        &self.metadata
    }

    /// Helper to set metadata
    pub fn set_metadata(&mut self, data: BPlusTreeMetadata) {
        self.metadata = data;
        self.dirty = true;
    }

    pub fn is_empty(&self) -> bool {
        self.root.keys.is_empty()
    }

    pub fn len(&self) -> usize {
        self.root.len()
    }

    pub fn insert(&mut self, key: K, value: V) {
        self.dirty = true;
        if self.root.keys.is_empty() {
            self.root.keys.push(key);
            self.root.values.push(value);
            return;
        }

        if let Some(node) = self.root.insert(key, value, self.inner_order, self.leaf_order) {
            let child_key_opt = if node.is_leaf {
                node.keys.first()
            } else {
                BPlusTreeNode::<K, V>::find_leaf_entry(&node)
            };

            if let Some(child_key) = child_key_opt {
                let mut new_root = BPlusTreeNode::<K, V>::new(false);
                new_root.keys.push(child_key.clone());
                new_root.children.push(std::mem::replace(&mut self.root, BPlusTreeNode::new(true)));
                new_root.children.push(node);

                self.root = new_root;
            } else {
                error!("Failed to insert child key");
            }
        }
    }

    pub fn query(&self, key: &K) -> Option<&V> {
        self.root.query(key)
    }

    pub fn store(&mut self, filepath: &Path) -> io::Result<u64> {
        if self.dirty {
            // Advisory lock to prevent concurrent COW updates
            let _lock = FileLock::try_lock(filepath)?;
            self.store_internal(filepath)
        } else {
            Ok(0)
        }
    }

    /// Store the tree and build a sorted index file.
    ///
    /// # Arguments
    /// * `filepath` - Path to store the `BPlusTree`
    /// * `sort_key_extractor` - Closure that extracts the sort key from a value
    ///
    /// # Example
    /// ```ignore
    /// tree.store_with_index(&db_path, |v| v.name.clone())?;
    /// ```
    pub fn store_with_index<SortKey, F>(
        &mut self,
        filepath: &Path,
        sort_key_extractor: F,
    ) -> io::Result<u64>
    where
        SortKey: Ord + Serialize,
        F: Fn(&V) -> SortKey,
    {
        // Store the tree first
        let result = self.store(filepath)?;
        if result > 0 {
            Self::store_index(filepath, sort_key_extractor)?;
        }

        Ok(result)
    }

    pub fn store_index<SortKey, F>(filepath: &Path, sort_key_extractor: F) -> io::Result<()>
    where
        SortKey: Ord + Serialize,
        F: Fn(&V) -> SortKey,
    {
        let index_path = get_file_path_for_db_index(filepath);

        // Re-open the stored tree to get value locations
        let mut query = BPlusTreeQuery::<K, V>::try_new(filepath)?;
        let entries_with_locations = query.collect_with_locations()?;

        // Collect (sort_key, primary_key, location) and sort
        let mut sorted_entries: Vec<(SortKey, K, super::sorted_index::ValueLocation)> =
            entries_with_locations
                .into_iter()
                .map(|(k, v, loc)| (sort_key_extractor(&v), k, loc))
                .collect();

        // Sort by sort key
        sorted_entries.sort_by(|a, b| a.0.cmp(&b.0));

        // Write index file
        let mut writer = super::sorted_index::SortedIndexWriter::new(&index_path)?;
        for (sort_key, primary_key, location) in &sorted_entries {
            writer.push(sort_key, primary_key, *location)?;
        }
        writer.finish()?;
        Ok(())
    }

    /// Internal store without locking, used for compaction or initial save.
    fn store_internal(&mut self, filepath: &Path) -> io::Result<u64> {
        let tempfile = NamedTempFile::new()?;
        let mut file = utils::file_writer(&tempfile);
        let mut buffer = vec![0u8; PAGE_SIZE_USIZE];

        // Write header block 0
        let mut header = [0u8; PAGE_SIZE_USIZE];
        header[0..4].copy_from_slice(MAGIC);
        header[4..8].copy_from_slice(&STORAGE_VERSION.to_le_bytes());
        // Placeholder for root offset, will be updated after serialization
        header[8..16].copy_from_slice(&HEADER_SIZE.to_le_bytes());

        let meta_bytes = self.metadata.to_bytes();
        if meta_bytes.len() > METADATA_MAX_SIZE || 20 + meta_bytes.len() > PAGE_SIZE_USIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Metadata too large for header page"));
        }
        if !meta_bytes.is_empty() {
            let len = u32::try_from(meta_bytes.len()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            header[16..20].copy_from_slice(&len.to_le_bytes());
            header[20..20 + meta_bytes.len()].copy_from_slice(&meta_bytes);
        }

        file.write_all(&header)?;

        // We need to ensure we pad to PAGE_SIZE before continuing
        file.seek(SeekFrom::Start(HEADER_SIZE))?;

        // Use breadth-first serialization for better disk locality
        match self.root.serialize_breadth_first(&mut file, &mut buffer, HEADER_SIZE) {
            Ok(result) => {
                file.flush()?;
                drop(file);
                if let Err(err) = utils::rename_or_copy(tempfile.path(), filepath, false) {
                    return Err(string_to_io_error(format!("Temp file rename/copy did not work {} {err}", tempfile.path().to_string_lossy())));
                }
                self.dirty = false;
                Ok(result)
            }
            Err(err) => Err(err),
        }
    }

    /// Bulk build a tree from pre-calculated `ValueInfos` (streaming compact helper).
    /// Writes nodes to `file` starting at `start_offset`.
    /// Returns the offset of the root node used to update the file header.
    fn build_levels_from_pointers<W: Write + Seek>(
        &self,
        file: &mut W,
        mut next_level_pointers: Vec<(K, u64)>,
        mut current_offset: u64,
        write_buffer: &mut [u8],
    ) -> io::Result<u64> {
        if next_level_pointers.is_empty() {
            return Ok(current_offset);
        }

        while next_level_pointers.len() > 1 {
            let mut parent_level_pointers: Vec<(K, u64)> = Vec::new();
            let children = next_level_pointers;

            for chunk in children.chunks(self.inner_order + 1) {
                let mut node = BPlusTreeNode::<K, V>::new(false);
                let mut pointers = Vec::new();

                if let Some((_, off)) = chunk.first() {
                    pointers.push(*off);
                }

                for (k, off) in &chunk[1..] {
                    node.keys.push(k.clone());
                    pointers.push(*off);
                }

                let node_offset = current_offset;
                current_offset = node.serialize_internal_with_offsets(file, write_buffer, node_offset, &pointers)?;

                if let Some((k, _)) = chunk.first() {
                    parent_level_pointers.push((k.clone(), node_offset));
                }
            }
            next_level_pointers = parent_level_pointers;
        }

        if let Some((_, root_off)) = next_level_pointers.first() {
            Ok(*root_off)
        } else {
            Ok(current_offset)
        }
    }


    pub fn load(filepath: &Path) -> io::Result<Self> {
        let mut file = File::open(filepath)?;

        // Verify Header
        let mut header = [0u8; PAGE_SIZE_USIZE]; // Read the whole header page
        file.read_exact(&mut header)?;
        if &header[0..4] != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid magic number"));
        }
        let version = u32::from_le_bytes(header[4..8].try_into().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid version slice"))?);
        if version != STORAGE_VERSION {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Unsupported storage version: {version}")));
        }
        let root_offset = u64::from_le_bytes(header[8..16].try_into().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid root offset slice"))?);

        // Read metadata
        let metadata_len = u32::from_le_bytes(header[16..20].try_into().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid metadata length slice"))?);
        let metadata = if metadata_len > 0 {
            if (20 + metadata_len as usize) > PAGE_SIZE_USIZE {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "Metadata length exceeds header page size"));
            }
            header[20..(20 + metadata_len as usize)].to_vec()
        } else {
            Vec::new()
        };

        let mut reader = utils::file_reader(file);
        let mut buffer = vec![0u8; PAGE_SIZE_USIZE];
        // Start after header block
        let (root, _) = BPlusTreeNode::<K, V>::deserialize_from_block(&mut reader, &mut buffer, root_offset, true)?;

        let (inner_order, leaf_order) = calc_order::<K>();
        Ok(Self {
            root,
            inner_order,
            leaf_order,
            metadata: BPlusTreeMetadata::from_bytes(&metadata),
            dirty: false,
        })
    }

    /// Find the largest key <= `key` in the in-memory tree and return references to (key, value).
    pub fn find_le(&self, key: &K) -> Option<(&K, &V)> {
        // empty tree
        if self.root.keys.is_empty() && self.root.is_leaf && self.root.values.is_empty() {
            return None;
        }
        self.root.find_le(key)
    }

    pub fn traverse<F>(&self, mut visit: F)
    where
        F: FnMut(&Vec<K>, &Vec<V>),
    {
        self.root.traverse(&mut visit);
    }
}

fn query_tree<K, V, R: Read + Seek>(
    file: &mut R,
    buffer: &mut Vec<u8>,
    cache: &mut IndexMap<u64, Vec<u8>>,
    key: &K,
    start_offset: u64,
) -> Result<Option<V>, BPlusTreeError>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    let mut offset = start_offset;
    loop {
        // Try Cache First
        let (node, pointers) = if let Some(data) = cache.shift_remove(&offset) {
            // LRU: Use shift_remove + insert to avoid cloning or repeated lookups
            let res = BPlusTreeNode::<K, V>::deserialize_from_block_slice(&data, file, false)
                .map_err(BPlusTreeError::from)?;
            cache.insert(offset, data);
            res
        } else {
            // Disk Read
            match BPlusTreeNode::<K, V>::deserialize_from_block(file, buffer, offset, false) {
                Ok((node, pointers)) => {
                    // Update Cache
                    if cache.len() >= CACHE_CAPACITY {
                        cache.shift_remove_index(0);
                    }
                    cache.insert(offset, buffer.to_owned());
                    (node, pointers)
                }
                Err(err) => {
                    return Err(BPlusTreeError::from(err));
                }
            }
        };

        if node.is_leaf {
            return match node.keys.binary_search(key) {
                Ok(idx) => {
                    match node.value_info.get(idx) {
                        Some(info) => {
                            let value = BPlusTreeNode::<K, V>::load_value_from_info(file, info)?;
                            Ok(Some(value))
                        }
                        None => Ok(None),
                    }
                }
                Err(_) => Ok(None),
            };
        }

        let child_idx = get_entry_index_upper_bound::<K>(&node.keys, key);
        if let Some(child_offsets) = pointers {
            if let Some(child_offset) = child_offsets.get(child_idx) {
                offset = *child_offset;
            } else {
                return Ok(None);
            }
        } else {
            return Ok(None);
        }
    }
}


fn query_tree_le<K, V, R: Read + Seek>(
    file: &mut R,
    buffer: &mut Vec<u8>,
    cache: &mut IndexMap<u64, Vec<u8>>,
    key: &K,
    start_offset: u64,
) -> Result<Option<V>, BPlusTreeError>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    let mut offset = start_offset;
    loop {
        // Try Cache First
        let (node, pointers) = if let Some(data) = cache.shift_remove(&offset) {
            // LRU
            let res = BPlusTreeNode::<K, V>::deserialize_from_block_slice(&data, file, false)
                .map_err(BPlusTreeError::from)?;
            cache.insert(offset, data);
            res
        } else {
            // Disk Read
            match BPlusTreeNode::<K, V>::deserialize_from_block(file, buffer, offset, false) {
                Ok((node, pointers)) => {
                    // Update Cache
                    if cache.len() >= CACHE_CAPACITY {
                        cache.shift_remove_index(0);
                    }
                    cache.insert(offset, buffer.to_owned());
                    (node, pointers)
                }
                Err(err) => {
                    return Err(BPlusTreeError::from(err));
                }
            }
        };

        if node.is_leaf {
            let idx = get_entry_index_upper_bound::<K>(&node.keys, key);
            if idx == 0 {
                return Ok(None);
            }
            return match node.value_info.get(idx - 1) {
                Some(info) => {
                    let value = BPlusTreeNode::<K, V>::load_value_from_info(file, info)?;
                    Ok(Some(value))
                }
                None => Ok(None),
            };
        }

        let child_idx = get_entry_index_upper_bound::<K>(&node.keys, key);
        if let Some(child_offsets) = pointers {
            if let Some(child_offset) = child_offsets.get(child_idx) {
                offset = *child_offset;
            } else if let Some(last) = child_offsets.last() {
                offset = *last;
            } else {
                return Ok(None);
            }
        } else {
            return Ok(None);
        }
    }
}

fn count_items<K, V, R: Read + Seek>(
    file: &mut R,
    buffer: &mut Vec<u8>,
    cache: &mut IndexMap<u64, Vec<u8>>,
    start_offset: u64,
) -> Result<usize, BPlusTreeError>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    let mut count = 0;
    let mut stack = vec![start_offset];
    while let Some(offset) = stack.pop() {
        let (node, pointers) = if let Some(data) = cache.shift_remove(&offset) {
            let res = BPlusTreeNode::<K, V>::deserialize_from_block_slice(&data, file, false)
                .map_err(BPlusTreeError::from)?;
            cache.insert(offset, data);
            res
        } else {
            match BPlusTreeNode::<K, V>::deserialize_from_block(file, buffer, offset, false) {
                Ok((node, pointers)) => {
                    if cache.len() >= CACHE_CAPACITY {
                        cache.shift_remove_index(0);
                    }
                    cache.insert(offset, buffer.to_owned());
                    (node, pointers)
                }
                Err(err) => return Err(BPlusTreeError::from(err)),
            }
        };

        if node.is_leaf {
            count += node.keys.len();
        } else if let Some(ptrs) = pointers {
            stack.extend(ptrs);
        }
    }
    Ok(count)
}

fn count_items_mmap<K, V>(
    mmap: &[u8],
    start_offset: u64,
) -> Result<usize, BPlusTreeError>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    let mut count = 0;
    let mut stack = vec![start_offset];
    let mut cursor = io::Cursor::new(mmap);
    while let Some(offset) = stack.pop() {
        let (node, pointers) = BPlusTreeNode::<K, V>::deserialize_from_mmap(mmap, &mut cursor, offset, false)?;

        if node.is_leaf {
            count += node.keys.len();
        } else if let Some(ptrs) = pointers {
            stack.extend(ptrs);
        }
    }
    Ok(count)
}

fn query_tree_mmap<K, V>(
    mmap: &[u8],
    cursor: &mut io::Cursor<&[u8]>,
    key: &K,
    start_offset: u64,
) -> Result<Option<V>, BPlusTreeError>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    let mut offset = start_offset;
    loop {
        let (node, pointers) = BPlusTreeNode::<K, V>::deserialize_from_mmap(mmap, cursor, offset, false)?;

        if node.is_leaf {
            return match node.keys.binary_search(key) {
                Ok(idx) => {
                    match node.value_info.get(idx) {
                        Some(info) => {
                            let value = BPlusTreeNode::<K, V>::load_value_from_info(cursor, info)?;
                            Ok(Some(value))
                        }
                        None => Ok(None),
                    }
                }
                Err(_) => Ok(None),
            };
        }

        let child_idx = get_entry_index_upper_bound::<K>(&node.keys, key);
        if let Some(child_offsets) = pointers {
            if let Some(child_offset) = child_offsets.get(child_idx) {
                offset = *child_offset;
            } else {
                return Ok(None);
            }
        } else {
            return Ok(None);
        }
    }
}

fn query_tree_le_mmap<K, V>(
    mmap: &[u8],
    cursor: &mut io::Cursor<&[u8]>,
    key: &K,
    start_offset: u64,
) -> Result<Option<V>, BPlusTreeError>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    let mut offset = start_offset;
    loop {
        let (node, pointers) = BPlusTreeNode::<K, V>::deserialize_from_mmap(mmap, cursor, offset, false)?;

        if node.is_leaf {
            let idx = get_entry_index_upper_bound::<K>(&node.keys, key);
            if idx == 0 {
                return Ok(None);
            }
            return match node.value_info.get(idx - 1) {
                Some(info) => {
                    let value = BPlusTreeNode::<K, V>::load_value_from_info(cursor, info)?;
                    Ok(Some(value))
                }
                None => Ok(None),
            };
        }

        let child_idx = get_entry_index_upper_bound::<K>(&node.keys, key);
        if let Some(child_offsets) = pointers {
            if let Some(child_offset) = child_offsets.get(child_idx) {
                offset = *child_offset;
            } else {
                return Ok(None);
            }
        } else {
            return Ok(None);
        }
    }
}


/// `BPlusTreeQuery` performs on-disk queries without loading the entire tree into memory.
/// For frequent queries, consider using `BPlusTree::load()` instead, which loads the full tree into memory
/// at the cost of higher memory usage.
pub struct BPlusTreeQuery<K, V> {
    file: Option<BufReader<File>>,
    mmap: Option<Mmap>,
    filepath: PathBuf,
    buffer: Vec<u8>,
    cache: IndexMap<u64, Vec<u8>>,
    root_offset: u64,
    _marker_k: PhantomData<K>,
    _marker_v: PhantomData<V>,
}

impl<K, V> BPlusTreeQuery<K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub fn try_from_file(file: File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        let file_len = metadata.len();

        if file_len < HEADER_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "File too small"));
        }

        // Try Mmap
        let mmap = unsafe { Mmap::map(&file).ok() };

        // Verify Header
        let mut header = [0u8; 16];
        #[cfg(unix)]
        file.read_exact_at(&mut header, 0)?;
        #[cfg(not(unix))]
        {
            let mut f = &file;
            f.seek(SeekFrom::Start(0))?;
            f.read_exact(&mut header)?;
        }

        if &header[0..4] != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid magic number"));
        }
        let version = u32::from_le_bytes(header[4..8].try_into().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid version slice"))?);
        if version != STORAGE_VERSION {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Unsupported storage version: {version}")));
        }
        let root_offset = u64::from_le_bytes(header[8..16].try_into().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid root offset slice"))?);

        Ok(Self {
            file: if mmap.is_some() { None } else { Some(utils::file_reader(file)) },
            mmap,
            filepath: PathBuf::new(),
            buffer: vec![0u8; PAGE_SIZE_USIZE],
            cache: IndexMap::with_capacity(CACHE_CAPACITY),
            root_offset,
            _marker_k: PhantomData,
            _marker_v: PhantomData,
        })
    }

    pub fn try_new(filepath: &Path) -> io::Result<Self> {
        let file = File::open(filepath)?;
        let mut query = Self::try_from_file(file)?;
        query.filepath = filepath.to_path_buf();
        Ok(query)
    }

    /// Returns the filepath this query was opened from.
    pub fn filepath(&self) -> &Path {
        &self.filepath
    }

    pub fn query(&mut self, key: &K) -> Result<Option<V>, BPlusTreeError> {
        if let Some(mmap) = &self.mmap {
            let mut cursor = io::Cursor::new(mmap.as_ref());
            query_tree_mmap(mmap, &mut cursor, key, self.root_offset)
        } else if let Some(file) = &mut self.file {
            query_tree(file, &mut self.buffer, &mut self.cache, key, self.root_offset)
        } else {
            Err(BPlusTreeError::InvalidStructure("No data source available".into()))
        }
    }

    pub fn query_le(&mut self, key: &K) -> Result<Option<V>, BPlusTreeError> {
        if let Some(mmap) = &self.mmap {
            let mut cursor = io::Cursor::new(mmap.as_ref());
            query_tree_le_mmap(mmap, &mut cursor, key, self.root_offset)
        } else if let Some(file) = &mut self.file {
            query_tree_le(file, &mut self.buffer, &mut self.cache, key, self.root_offset)
        } else {
            Err(BPlusTreeError::InvalidStructure("No data source available".into()))
        }
    }

    pub fn len(&mut self) -> Result<usize, BPlusTreeError> {
        if let Some(mmap) = &self.mmap {
            count_items_mmap::<K, V>(mmap, self.root_offset)
        } else if let Some(file) = &mut self.file {
            count_items::<K, V, _>(file, &mut self.buffer, &mut self.cache, self.root_offset)
        } else {
            Err(BPlusTreeError::InvalidStructure("No data source available".into()))
        }
    }

    pub fn is_empty(&mut self) -> Result<bool, BPlusTreeError> {
        let (node, _) = if let Some(mmap) = &self.mmap {
            let mut cursor = io::Cursor::new(mmap.as_ref());
            BPlusTreeNode::<K, V>::deserialize_from_mmap(mmap, &mut cursor, self.root_offset, false)?
        } else if let Some(file) = &mut self.file {
            BPlusTreeNode::<K, V>::deserialize_from_block(file, &mut self.buffer, self.root_offset, false)?
        } else {
            return Err(BPlusTreeError::InvalidStructure("No data source available".into()));
        };
        Ok(node.is_leaf && node.keys.is_empty())
    }


    /// Provides a disk-backed iterator that traverses the entire tree in order.
    pub fn iter(&mut self) -> BPlusTreeDiskIterator<'_, K, V> {
        BPlusTreeDiskIterator::new(self)
    }

    /// Owned iterator that traverses the tree in order defined by a secondary sorted index.
    ///
    /// The index path is automatically derived from the tree filepath by changing
    /// the extension to `.idx`. For example, if the tree is at `/data/items.bin`,
    /// the index is expected at `/data/items.idx`.
    ///
    /// This iterator reads values directly from stored offsets in O(1) time,
    /// avoiding O(log n) tree traversal per item.
    pub fn disk_iter_sorted<SortKey>(self) -> io::Result<super::sorted_index::BPlusTreeSortedIteratorOwned<K, V, SortKey>>
    where
        SortKey: for<'de> Deserialize<'de>,
    {
        super::sorted_index::BPlusTreeSortedIteratorOwned::<K, V, SortKey>::new_hybrid(self.filepath.clone(), self.file, self.mmap)
    }

    /// Owned iterator with explicit index path.
    pub fn disk_iter_sorted_with_path<SortKey>(self, index_path: &Path) -> io::Result<super::sorted_index::BPlusTreeSortedIteratorOwned<K, V, SortKey>>
    where
        SortKey: for<'de> Deserialize<'de>,
    {
        super::sorted_index::BPlusTreeSortedIteratorOwned::<K, V, SortKey>::with_index_path_hybrid(self.filepath.clone(), self.file, self.mmap, index_path)
    }

    /// Traverses the tree and calls the provided closure for each leaf's keys and values.
    pub fn traverse<F>(&mut self, mut f: F) -> io::Result<()>
    where
        F: FnMut(&[K], &[V]),
    {
        let mut it = self.iter();
        while !it.is_empty() {
            if let Some((keys, values)) = it.next_leaf()? {
                f(&keys, &values);
            }
        }
        Ok(())
    }

    /// Collects all entries with their value locations.
    /// Used for building sorted indexes that need direct value access.
    pub fn collect_with_locations(&mut self) -> io::Result<Vec<(K, V, super::sorted_index::ValueLocation)>> {
        let mut result = Vec::new();
        let mut stack = vec![self.root_offset];

        while let Some(offset) = stack.pop() {
            let (node, pointers) = if let Some(mmap) = &self.mmap {
                let mut cursor = io::Cursor::new(mmap.as_ref());
                BPlusTreeNode::<K, V>::deserialize_from_mmap(mmap, &mut cursor, offset, false)?
            } else if let Some(file) = &mut self.file {
                BPlusTreeNode::<K, V>::deserialize_from_block(file, &mut self.buffer, offset, false)?
            } else {
                return Err(io::Error::other("No data source available"));
            };

            if node.is_leaf {
                if let Some(mmap) = &self.mmap {
                    let mut cursor = io::Cursor::new(mmap.as_ref());
                    for (key, info) in node.keys.into_iter().zip(node.value_info.iter()) {
                        let value = BPlusTreeNode::<K, V>::load_value_from_info(&mut cursor, info)?;
                        let location = match info.mode {
                            ValueStorageMode::Single(offset) => super::sorted_index::ValueLocation::Single { offset, length: info.length },
                            ValueStorageMode::Packed(block_offset, index) => super::sorted_index::ValueLocation::Packed { block_offset, index, length: info.length },
                        };
                        result.push((key, value, location));
                    }
                } else if let Some(file) = &mut self.file {
                    for (key, info) in node.keys.into_iter().zip(node.value_info.iter()) {
                        let value = BPlusTreeNode::<K, V>::load_value_from_info(file, info)?;
                        let location = match info.mode {
                            ValueStorageMode::Single(offset) => super::sorted_index::ValueLocation::Single { offset, length: info.length },
                            ValueStorageMode::Packed(block_offset, index) => super::sorted_index::ValueLocation::Packed { block_offset, index, length: info.length },
                        };
                        result.push((key, value, location));
                    }
                }
            } else if let Some(ptrs) = pointers {
                for ptr in ptrs.into_iter().rev() {
                    stack.push(ptr);
                }
            }
        }
        Ok(result)
    }

    /// Provides an owned disk-backed iterator.
    pub fn disk_iter(self) -> BPlusTreeDiskIteratorOwned<K, V> {
        BPlusTreeDiskIteratorOwned::new(self)
    }
}


pub struct BPlusTreeDiskIteratorOwned<K, V> {
    query: BPlusTreeQuery<K, V>,
    stack: Vec<(u64, usize)>,
    leaf_keys: Vec<K>,
    leaf_values: Vec<V>,
    leaf_idx: usize,
}

impl<K, V> BPlusTreeDiskIteratorOwned<K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn new(query: BPlusTreeQuery<K, V>) -> Self {
        let root_offset = query.root_offset;
        Self {
            query,
            stack: vec![(root_offset, 0)],
            leaf_keys: Vec::new(),
            leaf_values: Vec::new(),
            leaf_idx: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.stack.is_empty() && self.leaf_idx >= self.leaf_keys.len()
    }

    fn next_leaf(&mut self) -> io::Result<Option<(Vec<K>, Vec<V>)>> {
        loop {
            let Some((offset, child_idx)) = self.stack.pop() else { return Ok(None) };

            let (node, pointers) = if let Some(mmap) = &self.query.mmap {
                let mut cursor = io::Cursor::new(mmap.as_ref());
                BPlusTreeNode::<K, V>::deserialize_from_mmap(mmap, &mut cursor, offset, false)?
            } else if let Some(file) = &mut self.query.file {
                BPlusTreeNode::<K, V>::deserialize_from_block(file, &mut self.query.buffer, offset, false)?
            } else {
                return Err(io::Error::other("No data source available"));
            };

            if node.is_leaf {
                let mut vals = Vec::with_capacity(node.value_info.len());
                if let Some(mmap) = &self.query.mmap {
                    let mut cursor = io::Cursor::new(mmap.as_ref());
                    for value_info in &node.value_info {
                        let v = BPlusTreeNode::<K, V>::load_value_from_info(&mut cursor, value_info)?;
                        vals.push(v);
                    }
                } else if let Some(file) = &mut self.query.file {
                    for value_info in &node.value_info {
                        let v = BPlusTreeNode::<K, V>::load_value_from_info(file, value_info)?;
                        vals.push(v);
                    }
                }
                return Ok(Some((node.keys, vals)));
            } else if let Some(pters) = pointers {
                if child_idx < pters.len() {
                    let next_ptr = pters[child_idx];
                    self.stack.push((offset, child_idx + 1));
                    self.stack.push((next_ptr, 0));
                }
            }
        }
    }
}

impl<K, V> Iterator for BPlusTreeDiskIteratorOwned<K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = (K, V);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.leaf_idx < self.leaf_keys.len() {
                let key = self.leaf_keys[self.leaf_idx].clone();
                let value = self.leaf_values[self.leaf_idx].clone();
                self.leaf_idx += 1;
                return Some((key, value));
            }

            match self.next_leaf() {
                Ok(Some((keys, values))) => {
                    self.leaf_keys = keys;
                    self.leaf_values = values;
                    self.leaf_idx = 0;
                }
                _ => return None,
            }
        }
    }
}

/// Generic reader that can be either a sorted index iterator or a regular disk iterator.
/// Used for fallback logic (Sorted -> Unsorted).
pub enum PlaylistIteratorReader<V> {
    Sorted(super::sorted_index::BPlusTreeSortedIteratorOwned<u32, V, u32>),
    Unsorted(BPlusTreeDiskIteratorOwned<u32, V>),
}

impl<V> Iterator for PlaylistIteratorReader<V>
where
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = io::Result<(u32, V)>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            PlaylistIteratorReader::Sorted(iter) => iter.next(),
            PlaylistIteratorReader::Unsorted(iter) => iter.next().map(Ok),
        }
    }
}

pub struct BPlusTreeDiskIterator<'a, K, V> {
    query: &'a mut BPlusTreeQuery<K, V>,
    stack: Vec<(u64, usize)>, // (node_offset, next_child_index)
    leaf_keys: Vec<K>,
    leaf_values: Vec<V>,
    leaf_idx: usize,
}

impl<'a, K, V> BPlusTreeDiskIterator<'a, K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn new(query: &'a mut BPlusTreeQuery<K, V>) -> Self {
        let root_offset = query.root_offset;
        Self {
            query,
            stack: vec![(root_offset, 0)],
            leaf_keys: Vec::new(),
            leaf_values: Vec::new(),
            leaf_idx: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.stack.is_empty() && self.leaf_idx >= self.leaf_keys.len()
    }

    /// Internal method to load the next leaf and return its content.
    fn next_leaf(&mut self) -> io::Result<Option<(Vec<K>, Vec<V>)>> {
        loop {
            let Some((offset, child_idx)) = self.stack.pop() else { return Ok(None) };

            let (node, pointers) = if let Some(mmap) = &self.query.mmap {
                let mut cursor = io::Cursor::new(mmap.as_ref());
                BPlusTreeNode::<K, V>::deserialize_from_mmap(mmap, &mut cursor, offset, false)?
            } else if let Some(file) = &mut self.query.file {
                BPlusTreeNode::<K, V>::deserialize_from_block(file, &mut self.query.buffer, offset, false)?
            } else {
                return Err(io::Error::other("No data source available"));
            };

            if node.is_leaf {
                let mut vals = Vec::with_capacity(node.value_info.len());
                if let Some(mmap) = &self.query.mmap {
                    let mut cursor = io::Cursor::new(mmap.as_ref());
                    for value_info in &node.value_info {
                        let v = BPlusTreeNode::<K, V>::load_value_from_info(&mut cursor, value_info)?;
                        vals.push(v);
                    }
                } else if let Some(file) = &mut self.query.file {
                    for value_info in &node.value_info {
                        let v = BPlusTreeNode::<K, V>::load_value_from_info(file, value_info)?;
                        vals.push(v);
                    }
                }
                return Ok(Some((node.keys, vals)));
            } else if let Some(pters) = pointers {
                if child_idx < pters.len() {
                    let next_ptr = pters[child_idx];
                    self.stack.push((offset, child_idx + 1));
                    self.stack.push((next_ptr, 0));
                }
            }
        }
    }
}

impl<K, V> Iterator for BPlusTreeDiskIterator<'_, K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = (K, V);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.leaf_idx < self.leaf_keys.len() {
                let key = self.leaf_keys[self.leaf_idx].clone();
                let value = self.leaf_values[self.leaf_idx].clone();
                self.leaf_idx += 1;
                return Some((key, value));
            }

            match self.next_leaf() {
                Ok(Some((keys, values))) => {
                    self.leaf_keys = keys;
                    self.leaf_values = values;
                    self.leaf_idx = 0;
                }
                Err(_err) => {
                    // It is possible the tree is empty or the file is being written to, so we just return None
                    return None;
                }
                _ => return None,
            }
        }
    }
}

pub struct BPlusTreeUpdate<K, V> {
    file: BufReader<File>,
    read_buffer: Vec<u8>,
    write_buffer: Vec<u8>,
    cache: IndexMap<u64, Vec<u8>>,
    root_offset: u64,
    inner_order: usize,
    leaf_order: usize,
    #[allow(dead_code)]
    lock: FileLock,
    _marker_k: PhantomData<K>,
    _marker_v: PhantomData<V>,
}

fn lock_path(filepath: &Path) -> PathBuf {
    if let Some(stem) = filepath.file_stem() {
        // filename with dot to hide
        let mut name = OsString::from(".");
        name.push(stem);
        name.push(".lock");
        filepath.with_file_name(name)
    } else {
        // Fallback: without dot
        filepath.with_extension("lock")
    }
}

struct FileLock {
    // We hold the file handle to keep the advisory lock active.
    // When this struct is dropped, the file handle closes and OS releases the lock.
    _file: File,
}

impl FileLock {
    fn try_lock(filepath: &Path) -> io::Result<Self> {
        // Sidecar Lock Pattern: Lock a separate .lock file, not the data file itself.
        // This ensures implementation works on Windows where locked files cannot be renamed/deleted.
        let lock_path_filename = lock_path(filepath);

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true) // Create if missing
            .truncate(false) // Do not truncate, just open
            .open(&lock_path_filename)?;

        // Try to acquire exclusive advisory lock.
        // If another process holds it, this returns immediately with Error (WouldBlock).
        file.try_lock_exclusive()?;

        Ok(Self { _file: file })
    }
}
// Drop implementation is implicit: closing the _file releases the lock.
// The .lock file remains on filesystem.

impl<K, V> BPlusTreeUpdate<K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub fn try_new(filepath: &Path) -> io::Result<Self> {
        if !filepath.exists() {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("File not found {}", filepath.to_str().unwrap_or("?"))));
        }
        // Acquire lock first
        let lock = FileLock::try_lock(filepath)?;

        let file = utils::open_read_write_file(filepath)?;
        let mut file = utils::file_reader(file);

        // Verify Header
        let mut header = [0u8; 16];
        file.read_exact(&mut header)?;
        if &header[0..4] != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid magic number"));
        }
        let version = u32::from_le_bytes(header[4..8].try_into().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid version slice"))?);
        if version != STORAGE_VERSION {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Unsupported storage version: {version}")));
        }
        let root_offset = u64::from_le_bytes(header[8..16].try_into().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid root offset slice"))?);
        let (inner_order, leaf_order) = calc_order::<K>();

        Ok(Self {
            file,
            read_buffer: vec![0u8; PAGE_SIZE_USIZE],
            write_buffer: vec![0u8; PAGE_SIZE_USIZE],
            cache: IndexMap::with_capacity(CACHE_CAPACITY),
            root_offset,
            inner_order,
            leaf_order,
            lock,
            _marker_k: PhantomData,
            _marker_v: PhantomData,
        })
    }

    /// Helper to access metadata
    pub fn get_metadata(&mut self) -> io::Result<BPlusTreeMetadata> {
        self.file.seek(SeekFrom::Start(METADATA_OFFSET_POS))?;
        let mut len_buf = [0u8; 4];
        if self.file.read_exact(&mut len_buf).is_err() {
            return Ok(BPlusTreeMetadata::Empty);
        }
        let len = u32::from_le_bytes(len_buf);
        if len == 0 || len as usize > METADATA_MAX_SIZE {
            return Ok(BPlusTreeMetadata::Empty);
        }
        let mut buf = vec![0u8; len as usize];
        self.file.read_exact(&mut buf)?;

        Ok(BPlusTreeMetadata::from_bytes(&buf))
    }

    /// Helper to set metadata
    pub fn set_metadata(&mut self, data: &BPlusTreeMetadata) -> io::Result<()> {
        let bytes = data.to_bytes();
        if bytes.len() > METADATA_MAX_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("Metadata too large: {} > {}", bytes.len(), METADATA_MAX_SIZE)));
        }

        self.file.seek(SeekFrom::Start(METADATA_OFFSET_POS))?;
        {
            let file = self.file.get_mut();
            file.write_all(&u32::try_from(bytes.len()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?.to_le_bytes())?;
            file.write_all(&bytes)?;
            file.flush()?; // Ensure it hits disk
        }
        // Resync/clear reader buffer after direct writes.
        self.file.seek_relative(0)?; // same as self.file.seek(SeekFrom::Current(0))?;

        Ok(())
    }

    pub fn query(&mut self, key: &K) -> Result<Option<V>, BPlusTreeError> {
        let mut reader = utils::file_reader(&mut self.file);
        query_tree(&mut reader, &mut self.read_buffer, &mut self.cache, key, self.root_offset)
    }

    pub fn is_empty(&mut self) -> Result<bool, BPlusTreeError> {
        let mut reader = utils::file_reader(&mut self.file);
        let (node, _) = BPlusTreeNode::<K, V>::deserialize_from_block(
            &mut reader,
            &mut self.read_buffer,
            self.root_offset,
            false,
        )?;
        Ok(node.is_leaf && node.keys.is_empty())
    }

    pub fn len(&mut self) -> Result<usize, BPlusTreeError> {
        count_items::<K, V, _>(&mut self.file, &mut self.read_buffer, &mut self.cache, self.root_offset)
    }

    pub fn query_le(&mut self, key: &K) -> Result<Option<V>, BPlusTreeError> {
        query_tree_le(&mut self.file, &mut self.read_buffer, &mut self.cache, key, self.root_offset)
    }


    pub fn update(&mut self, key: &K, value: V) -> Result<u64, BPlusTreeError> {
        let refs = [(key, &value)];
        let new_root_offset = self.update_batch_recursive(self.root_offset, &refs)?;

        // Atomic Header Swap
        self.file.get_mut().seek(SeekFrom::Start(ROOT_OFFSET_POS)).map_err(BPlusTreeError::Io)?;
        self.file.get_mut().write_all(&new_root_offset.to_le_bytes()).map_err(BPlusTreeError::Io)?;
        self.file.get_mut().flush().map_err(BPlusTreeError::Io)?;
        self.file.get_mut().sync_all().map_err(BPlusTreeError::Io)?;

        self.root_offset = new_root_offset;
        Ok(new_root_offset)
    }

    /// Update multiple items in batch. This is more efficient than calling `update()` multiple times
    /// as it performs all updates and then commits the final root offset once.
    /// returns The final root offset after all updates, or an error if any update fails
    fn update_batch_recursive(
        &mut self,
        offset: u64,
        items: &[(&K, &V)],
    ) -> Result<u64, BPlusTreeError> {
        let (mut node, pointers_opt) = BPlusTreeNode::<K, V>::deserialize_from_block(&mut self.file, &mut self.read_buffer, offset, false)?;

        if node.is_leaf {
            for (key, value) in items {
                match node.keys.binary_search(key) {
                    Ok(idx) => {
                        let (val_off, val_len) = self.insert_value_to_disk(value).map_err(BPlusTreeError::Io)?;
                        node.value_info[idx] = ValueInfo {
                            mode: ValueStorageMode::Single(val_off),
                            length: val_len,
                            compressed_cache: None,
                        };
                    }
                    Err(_) => return Err(BPlusTreeError::KeyNotFound),
                }
            }
            let new_offset = self.write_node(&node).map_err(BPlusTreeError::Io)?;
            Ok(new_offset)
        } else {
            let mut pointers = pointers_opt.ok_or_else(|| BPlusTreeError::InvalidStructure("Internal node missing pointers".into()))?;

            let mut current_idx = 0;
            while current_idx < items.len() {
                let first_key_in_group = items[current_idx].0;
                let child_idx = get_entry_index_upper_bound::<K>(&node.keys, first_key_in_group);

                let mut group_end = current_idx + 1;
                while group_end < items.len() && get_entry_index_upper_bound::<K>(&node.keys, items[group_end].0) == child_idx {
                    group_end += 1;
                }

                let sub_items = &items[current_idx..group_end];
                let new_child_offset = self.update_batch_recursive(pointers[child_idx], sub_items)?;
                pointers[child_idx] = new_child_offset;

                current_idx = group_end;
            }

            let new_offset = self.write_internal_node(&node, &pointers).map_err(BPlusTreeError::Io)?;
            Ok(new_offset)
        }
    }

    /// Update multiple items in batch. This is more efficient than calling `update()` multiple times
    /// as it performs all updates and then commits the final root offset once.
    /// returns The final root offset after all updates, or an error if any update fails
    pub fn update_batch(&mut self, items: &[(&K, &V)]) -> Result<u64, BPlusTreeError> {
        if items.is_empty() {
            return Ok(self.root_offset);
        }

        let mut sorted_items = items.to_vec();
        sorted_items.sort_by(|a, b| a.0.cmp(b.0));

        let new_root_offset = self.update_batch_recursive(self.root_offset, &sorted_items)?;

        // Atomic Header Swap - only once at the end
        self.file.get_mut().seek(SeekFrom::Start(ROOT_OFFSET_POS)).map_err(BPlusTreeError::Io)?;
        self.file.get_mut().write_all(&new_root_offset.to_le_bytes()).map_err(BPlusTreeError::Io)?;
        self.file.get_mut().flush().map_err(BPlusTreeError::Io)?;
        self.file.get_mut().sync_all().map_err(BPlusTreeError::Io)?;

        self.root_offset = new_root_offset;
        Ok(new_root_offset)
    }


    /// Insert or update multiple items in batch (upsert). If a key exists, it will be updated;
    /// if it doesn't exist, it will be inserted. This is more efficient than calling `update()`
    /// or `insert()` multiple times as it loads the tree once, performs all operations, and saves once.
    /// returns The final root offset after all upserts, or an error if any operation fails
    fn insert_value_to_disk(&mut self, value: &V) -> io::Result<(u64, u32)> {
        let raw_bytes = binary_serialize(value)?;

        // Decide whether to compress based on size and effectiveness
        let (flag, payload) = compress_if_beneficial(&raw_bytes);

        self.file.get_mut().seek(SeekFrom::End(0))?;
        let offset = self.file.get_mut().stream_position()?;

        // Write: [flag:1][payload]
        self.file.get_mut().write_all(&[flag])?;
        // NOTE: For LZ4, payload already includes original length (prepended)
        self.file.get_mut().write_all(&payload)?;

        // stored_len includes flag + payload
        let stored_len = 1 + payload.len();
        Ok((offset, u32::try_from(stored_len).map_err(to_io_error)?))
    }

    fn write_node(&mut self, node: &BPlusTreeNode<K, V>) -> io::Result<u64> {
        self.file.get_mut().seek(SeekFrom::End(0))?;
        let offset = self.file.get_mut().stream_position()?;
        node.serialize_to_block(self.file.get_mut(), &mut self.write_buffer, offset)?;
        Ok(offset)
    }

    fn write_internal_node(&mut self, node: &BPlusTreeNode<K, V>, pointers: &[u64]) -> io::Result<u64> {
        self.file.get_mut().seek(SeekFrom::End(0))?;
        let offset = self.file.get_mut().stream_position()?;
        node.serialize_internal_with_offsets(self.file.get_mut(), &mut self.write_buffer, offset, pointers)?;
        Ok(offset)
    }


    fn upsert_batch_recursive(
        &mut self,
        offset: u64,
        items: &[(&K, &V)],
    ) -> io::Result<(u64, Vec<(K, u64)>)> {
        let (mut node, pointers_opt) = BPlusTreeNode::<K, V>::deserialize_from_block(
            &mut self.file,
            &mut self.read_buffer,
            offset,
            false, // shallow
        )?;

        if node.is_leaf {
            for (key, value) in items {
                let (val_off, val_len) = self.insert_value_to_disk(value)?;
                let new_info = ValueInfo { mode: ValueStorageMode::Single(val_off), length: val_len, compressed_cache: None };

                match node.keys.binary_search(key) {
                    Ok(idx) => {
                        node.value_info[idx] = new_info;
                    }
                    Err(idx) => {
                        node.keys.insert(idx, (*key).clone());
                        node.value_info.insert(idx, new_info);
                    }
                }
            }

            let mut leaf_promotions = Vec::new();
            while node.keys.len() > self.leaf_order {
                let median_idx = node.keys.len() / 2;
                let mut right_node = BPlusTreeNode::new(true);
                right_node.keys = node.keys.split_off(median_idx);
                right_node.value_info = node.value_info.split_off(median_idx);

                let promoted_key = right_node.keys[0].clone();
                let right_offset = self.write_node(&right_node)?;
                leaf_promotions.push((promoted_key, right_offset));
            }

            let new_leaf_offset = self.write_node(&node)?;
            Ok((new_leaf_offset, leaf_promotions))
        } else {
            let mut pointers = pointers_opt.ok_or_else(|| io::Error::other("Internal node missing pointers"))?;
            let mut current_idx = 0;

            while current_idx < items.len() {
                let first_key_in_group = items[current_idx].0;
                let child_idx = get_entry_index_upper_bound::<K>(&node.keys, first_key_in_group);

                let mut group_end = current_idx + 1;
                while group_end < items.len() && get_entry_index_upper_bound::<K>(&node.keys, items[group_end].0) == child_idx {
                    group_end += 1;
                }

                let sub_items = &items[current_idx..group_end];
                let (new_child_offset, child_promotions) = self.upsert_batch_recursive(pointers[child_idx], sub_items)?;
                pointers[child_idx] = new_child_offset;

                for (median_key, right_child_offset) in child_promotions {
                    let insert_idx = get_entry_index_upper_bound::<K>(&node.keys, &median_key);
                    node.keys.insert(insert_idx, median_key);
                    pointers.insert(insert_idx + 1, right_child_offset);
                }

                current_idx = group_end;
            }

            let mut node_promotions = Vec::new();
            while node.keys.len() > self.inner_order {
                let median_idx = node.keys.len() / 2;
                let mut right_node = BPlusTreeNode::new(false);

                let promoted_key = node.keys.remove(median_idx);
                right_node.keys = node.keys.split_off(median_idx);
                let right_pointers = pointers.split_off(median_idx + 1);

                let right_offset = self.write_internal_node(&right_node, &right_pointers)?;
                node_promotions.push((promoted_key, right_offset));
            }

            let new_offset = self.write_internal_node(&node, &pointers)?;
            Ok((new_offset, node_promotions))
        }
    }

    fn build_higher_levels(&mut self, base_offset: u64, mut promotions: Vec<(K, u64)>) -> io::Result<u64> {
        if promotions.is_empty() {
            return Ok(base_offset);
        }
        promotions.sort_by(|a, b| a.0.cmp(&b.0));

        let mut node = BPlusTreeNode::<K, V>::new(false);
        let mut pointers = vec![base_offset];
        for (key, ptr) in promotions {
            node.keys.push(key);
            pointers.push(ptr);
        }

        if node.keys.len() <= self.inner_order {
            return self.write_internal_node(&node, &pointers);
        }

        let mut next_level_promotions = Vec::new();
        while node.keys.len() > self.inner_order {
            let median_idx = node.keys.len() / 2;
            let mut right_node = BPlusTreeNode::new(false);
            let promoted_key = node.keys.remove(median_idx);
            right_node.keys = node.keys.split_off(median_idx);
            let right_pointers = pointers.split_off(median_idx + 1);
            let right_offset = self.write_internal_node(&right_node, &right_pointers)?;
            next_level_promotions.push((promoted_key, right_offset));
        }
        let left_offset = self.write_internal_node(&node, &pointers)?;
        self.build_higher_levels(left_offset, next_level_promotions)
    }

    /// Insert or update multiple items in batch (upsert).
    /// Uses disk-based recursive traversal for efficiency, processing each node only once.
    pub fn upsert_batch(&mut self, items: &[(&K, &V)]) -> io::Result<u64> {
        if items.is_empty() {
            return Ok(self.root_offset);
        }

        let mut sorted_items = items.to_vec();
        sorted_items.sort_by(|a, b| a.0.cmp(b.0));

        let (mut current_root, promotions) = self.upsert_batch_recursive(self.root_offset, &sorted_items)?;

        // Handle promotions (splits) using balanced approach
        current_root = self.build_higher_levels(current_root, promotions)?;

        self.file.get_mut().seek(SeekFrom::Start(ROOT_OFFSET_POS))?;
        self.file.get_mut().write_all(&current_root.to_le_bytes())?;
        self.file.get_mut().flush()?;
        self.file.get_mut().sync_all()?;

        self.root_offset = current_root;
        Ok(current_root)
    }

    /// Garbage Collection: Compacts the file by rewriting only live blocks sequentially.
    pub fn compact(&mut self, filepath: &Path) -> io::Result<()> {
        let mut temp_file = NamedTempFile::new_in(filepath.parent().unwrap_or(Path::new(".")))?;

        // 1. Read existing metadata from source (manually to avoid full load)
        let mut metadata = Vec::new();
        {
            if let Ok(mut src) = File::open(filepath) {
                if src.seek(SeekFrom::Start(METADATA_OFFSET_POS)).is_ok() {
                    let mut lbuf = [0u8; 4];
                    if src.read_exact(&mut lbuf).is_ok() {
                        let l = u32::from_le_bytes(lbuf);
                        if l > 0 && l as usize <= METADATA_MAX_SIZE {
                            let mut b = vec![0u8; l as usize];
                            if src.read_exact(&mut b).is_ok() {
                                metadata = b;
                            } else {
                                error!("Failed to read metadata bytes during compaction");
                            }
                        } else if l > u32::try_from(METADATA_MAX_SIZE).unwrap_or(4000) {
                            error!("Metadata too large during compaction: {l}");
                        }
                    }
                }
            }
        }

        // 2. Write Header placeholder
        temp_file.seek(SeekFrom::Start(0))?;
        // Construct full header block
        let mut header = [0u8; PAGE_SIZE_USIZE];
        header[0..4].copy_from_slice(MAGIC);
        header[4..8].copy_from_slice(&STORAGE_VERSION.to_le_bytes());
        // Root offset placeholder (will be filled later)
        // Root offset placeholder (will be filled later)
        if !metadata.is_empty() {
            let len = u32::try_from(metadata.len()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            // Ensure we check max size?
            header[16..20].copy_from_slice(&len.to_le_bytes());
            header[20..20 + metadata.len()].copy_from_slice(&metadata);
        }
        temp_file.write_all(&header)?;

        // temp_file.seek(SeekFrom::Start(HEADER_SIZE))?; // Skip to start of content (implied by write_all(4096))

        let mut current_offset = HEADER_SIZE;
        let mut leaf_pointers: Vec<(K, u64)> = Vec::new();
        let mut current_leaf = BPlusTreeNode::<K, V>::new(true);

        // 2. Iterate source and write values + leaf nodes immediately (Streaming)
        {
            let mut query = BPlusTreeQuery::<K, V>::try_new(filepath).map_err(to_io_error)?;
            let mut write_buffer = std::io::BufWriter::new(&mut temp_file);
            let mut node_buffer = vec![0u8; PAGE_SIZE_USIZE];

            for (k, v) in query.iter() {
                let value_bytes = binary_serialize(&v)?;
                let val_offset = current_offset;

                // Write value header (flag + payload)
                let (flag, payload) = compress_if_beneficial(&value_bytes);
                write_buffer.write_all(&[flag])?;
                write_buffer.write_all(&payload)?;

                let stored_len = u32::try_from(1 + payload.len()).map_err(to_io_error)?;
                current_offset += u64::from(stored_len);

                current_leaf.keys.push(k);
                current_leaf.value_info.push(ValueInfo {
                    mode: ValueStorageMode::Single(val_offset),
                    length: stored_len,
                    compressed_cache: None,
                });

                if current_leaf.keys.len() >= self.leaf_order {
                    let first_key = current_leaf.keys[0].clone();
                    let node_offset = current_offset;
                    current_offset = current_leaf.serialize_to_block(&mut write_buffer, &mut node_buffer, node_offset)?;
                    leaf_pointers.push((first_key, node_offset));
                    current_leaf = BPlusTreeNode::new(true);
                }
            }

            // Handle trailing leaf
            if !current_leaf.keys.is_empty() {
                let first_key = current_leaf.keys[0].clone();
                let node_offset = current_offset;
                current_offset = current_leaf.serialize_to_block(&mut write_buffer, &mut node_buffer, node_offset)?;
                leaf_pointers.push((first_key, node_offset));
            }
            write_buffer.flush()?;
        }

        // 3. Build Internal Levels
        let tree = BPlusTree::<K, V>::new();
        let mut node_buffer = vec![0u8; PAGE_SIZE_USIZE];
        let root_offset = tree.build_levels_from_pointers(&mut temp_file, leaf_pointers, current_offset, &mut node_buffer)?;

        // 4. Update Header
        temp_file.seek(SeekFrom::Start(ROOT_OFFSET_POS))?;
        temp_file.write_all(&root_offset.to_le_bytes())?;

        temp_file.flush()?;
        temp_file.as_file().sync_all()?;

        // 5. Atomic Replace
        temp_file.persist(filepath).map_err(to_io_error)?;

        // 6. Refresh state
        self.root_offset = root_offset;
        let file = utils::open_read_write_file(filepath)?;
        self.file = utils::file_reader(file);
        self.cache.clear();

        Ok(())
    }
}

pub struct BPlusTreeIterator<'a, K, V> {
    stack: Vec<&'a BPlusTreeNode<K, V>>,
    current_keys: Option<&'a [K]>,
    current_values: Option<&'a [V]>,
    index: usize,
}

impl<'a, K, V> BPlusTreeIterator<'a, K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub fn new(tree: &'a BPlusTree<K, V>) -> Self {
        let stack = vec![&tree.root];
        Self {
            stack,
            current_keys: None,
            current_values: None,
            index: 0,
        }
    }
}

impl<'a, K, V> Iterator for BPlusTreeIterator<'a, K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // Try to return next item from current leaf
            if let Some(keys) = self.current_keys {
                if let Some(values) = self.current_values {
                    if self.index < keys.len() {
                        let key = &keys[self.index];
                        let value = &values[self.index];
                        self.index += 1;
                        return Some((key, value));
                    }
                }
            }

            // Current leaf exhausted, find next leaf
            loop {
                let node = self.stack.pop()?;

                if node.is_leaf {
                    // Found a leaf node
                    self.current_keys = Some(&node.keys);
                    self.current_values = Some(&node.values);
                    self.index = 0;
                    break; // Exit inner loop to process this leaf
                }
                // Push children in reverse order to maintain left-to-right traversal
                for child in node.children.iter().rev() {
                    self.stack.push(child);
                }
            }
        }
    }
}

impl<K, V> BPlusTree<K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub fn iter(&self) -> BPlusTreeIterator<'_, K, V> {
        BPlusTreeIterator::new(self)
    }
}

impl<'a, K, V> IntoIterator for &'a BPlusTree<K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = (&'a K, &'a V);
    type IntoIter = BPlusTreeIterator<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::io;

    use crate::repository::bplustree::{BPlusTree, BPlusTreeQuery, BPlusTreeUpdate};
    use serde::{Deserialize, Serialize};
    use shared::utils::generate_random_string;

    // Example usage with a simple struct
    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    struct Record {
        id: u32,
        data: String,
    }


    #[test]
    fn insert_test() -> io::Result<()> {
        let test_size = 500;
        let content = generate_random_string(1024);
        let mut tree = BPlusTree::<u32, Record>::new();
        for i in 0u32..=test_size {
            tree.insert(i, Record {
                id: i,
                data: format!("{content} {i}"),
            });
        }

        // // Traverse the tree
        // tree.traverse(|node| {
        //     println!("Node: {:?}", node);
        // });

        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_insert_test.bin");
        // Serialize the tree to a file
        tree.store(&filepath)?;
        // Deserialize the tree from the file
        tree = BPlusTree::<u32, Record>::load(&filepath)?;

        // Query the tree
        for i in 0u32..=test_size {
            let found = tree.query(&i);
            assert!(found.is_some(), "{content} {i} not found");
            assert!(found.unwrap().eq(&Record {
                id: i,
                data: format!("{content} {i}"),
            }), "{content} {i} not found");
        }

        let mut tree_query: BPlusTreeQuery<u32, Record> = BPlusTreeQuery::try_new(&filepath)?;
        for i in 0u32..=test_size {
            let found = tree_query.query(&i).expect("Query failed");
            assert!(found.is_some(), "{content} {i} not found");
            let entry = found.unwrap();
            assert!(entry.eq(&Record {
                id: i,
                data: format!("{content} {i}"),
            }), "{content} {i} not found");
        }

        let mut tree_update: BPlusTreeUpdate<u32, Record> = BPlusTreeUpdate::try_new(&filepath)?;

        for i in 0u32..=test_size {
            if let Ok(Some(record)) = tree_update.query(&i) {
                let new_record = Record {
                    id: record.id,
                    data: format!("{content} {}", record.id + 9000),
                };
                tree_update.update(&i, new_record).map_err(|e| e.to_io())?;
            } else {
                panic!("{content} {i} not found");
            }
        }

        // Verify with Query
        let mut tree_query: BPlusTreeQuery<u32, Record> = BPlusTreeQuery::try_new(&filepath)?;

        for i in 0u32..=test_size {
            let found = tree_query.query(&i).expect("Query failed");
            assert!(found.is_some(), "{content} {i} not found");
            let entry = found.unwrap();
            let expected = Record {
                id: i,
                data: format!("{content} {}", i + 9000),
            };
            assert!(entry.eq(&expected), "Entry not equal {entry:?} != {expected:?}");
        }

        Ok(())
    }


    #[test]
    fn insert_duplicate_test() {
        let content = "Entry";
        let mut tree = BPlusTree::<u32, Record>::new();
        for i in 0u32..=500 {
            tree.insert(i, Record {
                id: i,
                data: format!("{content} {i}"),
            });
        }
        for i in 0u32..=500 {
            tree.insert(i, Record {
                id: i,
                data: format!("{content} {}", i + 1),
            });
        }

        tree.traverse(|keys, values| {
            keys.iter().zip(values.iter()).for_each(|(k, v)| {
                assert!(format!("{content} {}", k + 1).eq(&v.data), "Wrong entry");
            });
        });
    }

    #[test]
    fn test_upsert_batch() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("upsert_batch_test.bin");

        // 1. Create initial tree
        let mut tree = BPlusTree::<u32, Record>::new();
        tree.insert(1, Record { id: 1, data: "original 1".to_string() });
        tree.insert(2, Record { id: 2, data: "original 2".to_string() });
        tree.store(&filepath)?;

        // 2. Open for update and upsert batch
        let mut update = BPlusTreeUpdate::<u32, Record>::try_new(&filepath)?;
        let r1_new = Record { id: 1, data: "updated 1".to_string() };
        let r3_new = Record { id: 3, data: "new 3".to_string() };

        update.upsert_batch(&[
            (&1, &r1_new),
            (&3, &r3_new),
        ])?;

        // 3. Verify with query
        let mut query = BPlusTreeQuery::<u32, Record>::try_new(&filepath)?;
        assert_eq!(query.query(&1).unwrap(), Some(r1_new));
        assert_eq!(query.query(&2).unwrap(), Some(Record { id: 2, data: "original 2".to_string() }));
        assert_eq!(query.query(&3).unwrap(), Some(r3_new));

        Ok(())
    }

    #[test]
    fn len_test() -> io::Result<()> {
        let test_size = 100;
        let mut tree = BPlusTree::<u32, Record>::new();

        // Initial state
        assert_eq!(tree.len(), 0);
        assert!(tree.is_empty());

        for i in 1..=test_size {
            tree.insert(i, Record {
                id: i,
                data: format!("data {i}"),
            });
            assert_eq!(tree.len(), i as usize);
            assert!(!tree.is_empty());
        }

        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("len_test.bin");
        tree.store(&filepath)?;

        // Test BPlusTreeQuery len
        let mut query: BPlusTreeQuery<u32, Record> = BPlusTreeQuery::try_new(&filepath)?;
        assert_eq!(query.len().expect("Query len failed"), test_size as usize);
        assert!(!query.is_empty().expect("Query is_empty failed"));

        // Test BPlusTreeUpdate len and modifications
        let mut update: BPlusTreeUpdate<u32, Record> = BPlusTreeUpdate::try_new(&filepath)?;
        assert_eq!(update.len().expect("Update len failed"), test_size as usize);

        // Update existing key - length should stay same
        update.update(&1, Record { id: 1, data: "updated".to_string() }).map_err(|e| e.to_io())?;
        assert_eq!(update.len().expect("Update len failed after update"), test_size as usize);

        // Insert new key - length should increase
        update.upsert_batch(&[
            (&(test_size + 1), &Record { id: test_size + 1, data: "new".to_string() })
        ])?;
        assert_eq!(update.len().expect("Update len failed after insert"), (test_size + 1) as usize);

        Ok(())
    }


    #[test]
    fn iterator_test() -> io::Result<()> {
        let mut tree = BPlusTree::<u32, Record>::new();
        let mut entry_set = HashSet::new();
        for i in 0u32..=500 {
            tree.insert(i, Record {
                id: i,
                data: format!("Entry {i}"),
            });
            entry_set.insert(i);
        }
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_iterator_test.bin");
        // Serialize the tree to a file
        tree.store(&filepath)?;
        let tree: BPlusTree<u32, Record> = BPlusTree::load(&filepath)?;

        // Traverse the tree
        for (key, value) in &tree {
            assert!(format!("Entry {key}").eq(&value.data), "Wrong entry");
            entry_set.remove(key);
        }
        assert!(entry_set.is_empty());
        Ok(())
    }

    #[test]
    fn persistence_update_and_iterate_test() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_update_iter.bin");
        let content = "InitialContent";
        let mut tree = BPlusTree::<u32, Record>::new();

        // Initial store
        for i in 0u32..100 {
            tree.insert(i, Record { id: i, data: format!("{content} {i}") });
        }
        tree.store(&filepath)?;
        drop(tree);

        // Update via BPlusTreeUpdate
        let mut tree_update = BPlusTreeUpdate::<u32, Record>::try_new(&filepath)?;
        for i in 0u32..100 {
            if i % 2 == 0 {
                tree_update.update(&i, Record { id: i, data: format!("UpdatedContent {i}") }).map_err(|e| e.to_io())?;
            }
        }

        // Reload and Verify via Query
        let mut tree_query: BPlusTreeQuery<u32, Record> = BPlusTreeQuery::try_new(&filepath)?;
        for i in 0u32..100 {
            let val = tree_query.query(&i).expect("Query failed").expect("Should find key");
            if i % 2 == 0 {
                assert_eq!(val.data, format!("UpdatedContent {i}"));
            } else {
                assert_eq!(val.data, format!("{content} {i}"));
            }
        }

        // Reload and Verify via Iterator
        let reloaded_tree = BPlusTree::<u32, Record>::load(&filepath)?;
        let mut count = 0;
        for (key, value) in &reloaded_tree {
            if *key % 2 == 0 {
                assert_eq!(value.data, format!("UpdatedContent {key}"), "Iterator returned wrong value for updated key {key}");
            } else {
                assert_eq!(value.data, format!("{content} {key}"), "Iterator returned wrong value for original key {key}");
            }
            count += 1;
        }
        assert_eq!(count, 100, "Iterator did not yield all entries");

        Ok(())
    }

    #[test]
    fn update_inplace_size_test() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_size_test.bin");
        let mut tree = BPlusTree::<u32, Record>::new();

        // Use fixed size string for predictable sizing
        let padding = "x".repeat(100);
        for i in 0u32..10 {
            tree.insert(i, Record { id: i, data: padding.clone() });
        }
        tree.store(&filepath)?;

        let initial_size = std::fs::metadata(&filepath)?.len();

        // Update with same size data
        let mut tree_update = BPlusTreeUpdate::<u32, Record>::try_new(&filepath)?;
        let same_size_padding = "y".repeat(100);
        for i in 0u32..10 {
            tree_update.update(&i, Record { id: i, data: same_size_padding.clone() }).map_err(|e| e.to_io())?;
        }

        let size_after_same_update = std::fs::metadata(&filepath)?.len();
        assert!(size_after_same_update > initial_size, "File should grow during COW same-size update");

        // Reload and verify
        let mut tree_query = BPlusTreeQuery::<u32, Record>::try_new(&filepath)?;
        for i in 0u32..10 {
            let val = tree_query.query(&i).expect("Query failed").expect("Should find key");
            assert_eq!(val.data, same_size_padding);
        }

        // Update with smaller size data
        let smaller_padding = "z".repeat(50);
        for i in 0u32..10 {
            tree_update.update(&i, Record { id: i, data: smaller_padding.clone() }).map_err(|e| e.to_io())?;
        }

        // Update with larger size data (force append)
        let larger_padding = "w".repeat(5000);
        for i in 0u32..1 {
            tree_update.update(&i, Record { id: i, data: larger_padding.clone() }).map_err(|e| e.to_io())?;
        }

        let size_before_compact = std::fs::metadata(&filepath)?.len();

        // Final verification: Compact should shrink the file
        tree_update.compact(&filepath)?;
        let size_after_compact = std::fs::metadata(&filepath)?.len();
        assert!(size_after_compact < size_before_compact, "Compaction should reduce file size");

        // Final data check after compact
        let mut final_query = BPlusTreeQuery::<u32, Record>::try_new(&filepath)?;
        assert_eq!(final_query.query(&0).unwrap().unwrap().data, larger_padding);
        for i in 1u32..10 {
            assert_eq!(final_query.query(&i).unwrap().unwrap().data, smaller_padding);
        }

        Ok(())
    }

    #[test]
    fn cow_deep_tree_compaction_test() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("deep_tree.idx");

        let test_size = 500u32; // Enough to force multiple levels
        let mut tree = BPlusTree::new();
        for i in 0..test_size {
            tree.insert(i, Record { id: i, data: format!("Content {i}") });
        }
        tree.store(&filepath)?;

        let mut tree_update = BPlusTreeUpdate::<u32, Record>::try_new(&filepath)?;

        // 1. Initial Queries
        for i in (0..test_size).step_by(50) {
            let val = tree_update.query(&i).expect("Query failed").expect("Should find initial key");
            assert_eq!(val.data, format!("Content {i}"));
        }

        // 2. Multiple Updates (COW)
        for i in (0..test_size).step_by(10) {
            tree_update.update(&i, Record { id: i, data: format!("UpdatedContent {i}") }).map_err(|e| e.to_io())?;
        }

        // 3. Verify Query Integrity (Must return NEW values)
        for i in (0..test_size).step_by(10) {
            let val = tree_update.query(&i).expect("Query failed").expect("Should find updated key");
            assert_eq!(val.data, format!("UpdatedContent {i}"));
        }

        // 4. Verify Query Integrity for non-updated keys (Must return OLD values)
        for i in (1..test_size).step_by(11) {
            if i % 10 == 0 { continue; } // skip updated ones
            let val = tree_update.query(&i).expect("Query failed").expect("Should find original key");
            assert_eq!(val.data, format!("Content {i}"));
        }

        let size_before_compact = std::fs::metadata(&filepath)?.len();

        // 5. GC / Compaction
        tree_update.compact(&filepath)?;

        let size_after_compact = std::fs::metadata(&filepath)?.len();
        assert!(size_after_compact < size_before_compact, "Compaction should reclaimed space from COW path copies");

        // 6. Final verification after GC
        let mut final_query = BPlusTreeQuery::<u32, Record>::try_new(&filepath)?;
        for i in (0..test_size).step_by(10) {
            let val = final_query.query(&i).expect("Query failed").expect("Should find updated key after GC");
            assert_eq!(val.data, format!("UpdatedContent {i}"));
        }

        Ok(())
    }

    #[test]
    fn query_le_cow_test() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("le_cow.idx");

        // 1. Build initial tree with gaps
        let mut tree = BPlusTree::new();
        for i in (0..100u32).step_by(10) {
            tree.insert(i, Record { id: i, data: format!("Content {i}") });
        }
        tree.store(&filepath)?;

        let mut tree_update = BPlusTreeUpdate::<u32, Record>::try_new(&filepath)?;

        // Initial LE check
        assert_eq!(tree_update.query_le(&15).unwrap().unwrap().id, 10);
        assert_eq!(tree_update.query_le(&5).unwrap().unwrap().id, 0);

        // 2. COW Update
        tree_update.update(&10, Record { id: 10, data: "NewVal".to_string() }).map_err(|e| e.to_io())?;

        // 3. Verify LE returns the LATEST value
        let val = tree_update.query_le(&15).expect("Query failed").expect("Should find LE key after COW update");
        assert_eq!(val.id, 10);
        assert_eq!(val.data, "NewVal");

        Ok(())
    }

    #[test]
    fn disk_iterator_test() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("disk_it.idx");

        let mut tree = BPlusTree::new();
        let test_size = 500u32;
        for i in 0..test_size {
            tree.insert(i, Record { id: i, data: format!("Value {i}") });
        }
        tree.store(&filepath)?;
        drop(tree);

        let mut query = BPlusTreeQuery::<u32, Record>::try_new(&filepath)?;

        // 1. Test Iterator
        let mut count = 0;
        for (k, v) in query.iter() {
            assert_eq!(k, count);
            assert_eq!(v.data, format!("Value {count}"));
            count += 1;
        }
        assert_eq!(count, test_size);

        // 2. Test Traverse helper
        let mut traverse_count = 0;
        query.traverse(|keys, values| {
            for (k, v) in keys.iter().zip(values.iter()) {
                assert_eq!(*k, traverse_count);
                assert_eq!(v.data, format!("Value {traverse_count}"));
                traverse_count += 1;
            }
        })?;
        assert_eq!(traverse_count, test_size);

        Ok(())
    }

    #[test]
    fn update_batch_basic_test() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_update_batch.bin");
        let mut tree = BPlusTree::<u32, Record>::new();

        // Create initial tree
        for i in 0u32..50 {
            tree.insert(i, Record {
                id: i,
                data: format!("Initial {i}"),
            });
        }
        tree.store(&filepath)?;
        drop(tree);

        // Test batch update
        let mut tree_update = BPlusTreeUpdate::<u32, Record>::try_new(&filepath)?;

        // Prepare batch updates
        let updates: Vec<(u32, Record)> = (0u32..50)
            .filter(|i| i % 5 == 0)
            .map(|i| (i, Record { id: i, data: format!("BatchUpdated {i}") }))
            .collect();

        let update_refs: Vec<(&u32, &Record)> = updates.iter()
            .map(|(k, v)| (k, v))
            .collect();

        tree_update.update_batch(&update_refs).map_err(|e| e.to_io())?;
        drop(tree_update);

        // Verify all updates
        let mut tree_query = BPlusTreeQuery::<u32, Record>::try_new(&filepath)?;
        for i in 0u32..50 {
            let val = tree_query.query(&i).expect("Query failed").expect("Should find key");
            if i % 5 == 0 {
                assert_eq!(val.data, format!("BatchUpdated {i}"), "Batch updated key {i} should have new value");
            } else {
                assert_eq!(val.data, format!("Initial {i}"), "Non-updated key {i} should have original value");
            }
        }

        Ok(())
    }

    #[test]
    fn update_batch_empty_test() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_update_batch_empty.bin");
        let mut tree = BPlusTree::<u32, Record>::new();

        // Create initial tree
        for i in 0u32..10 {
            tree.insert(i, Record {
                id: i,
                data: format!("Initial {i}"),
            });
        }
        tree.store(&filepath)?;
        drop(tree);

        let mut tree_update = BPlusTreeUpdate::<u32, Record>::try_new(&filepath)?;
        let initial_root = tree_update.root_offset;

        // Test empty batch - should be no-op
        let empty_batch: Vec<(&u32, &Record)> = vec![];
        let result = tree_update.update_batch(&empty_batch).map_err(|e| e.to_io())?;

        assert_eq!(result, initial_root, "Empty batch should not change root offset");

        // Verify data unchanged
        for i in 0u32..10 {
            let val = tree_update.query(&i).expect("Query failed").expect("Should find key");
            assert_eq!(val.data, format!("Initial {i}"));
        }

        Ok(())
    }

    #[test]
    fn update_batch_large_test() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_update_batch_large.bin");
        let mut tree = BPlusTree::<u32, Record>::new();

        let test_size = 200u32;

        // Create initial tree
        for i in 0..test_size {
            tree.insert(i, Record {
                id: i,
                data: format!("Initial {i}"),
            });
        }
        tree.store(&filepath)?;
        drop(tree);

        let mut tree_update = BPlusTreeUpdate::<u32, Record>::try_new(&filepath)?;

        // Prepare large batch update (every other item)
        let updates: Vec<(u32, Record)> = (0..test_size)
            .filter(|i| i % 2 == 0)
            .map(|i| (i, Record { id: i, data: format!("BatchUpdated {i}") }))
            .collect();

        let update_refs: Vec<(&u32, &Record)> = updates.iter()
            .map(|(k, v)| (k, v))
            .collect();

        // Perform batch update
        tree_update.update_batch(&update_refs).map_err(|e| e.to_io())?;
        drop(tree_update);

        // Verify all updates via iterator
        let reloaded_tree = BPlusTree::<u32, Record>::load(&filepath)?;
        let mut count = 0;
        for (key, value) in &reloaded_tree {
            if *key % 2 == 0 {
                assert_eq!(value.data, format!("BatchUpdated {key}"), "Even keys should be batch updated");
            } else {
                assert_eq!(value.data, format!("Initial {key}"), "Odd keys should remain unchanged");
            }
            count += 1;
        }
        assert_eq!(count, test_size, "Should have all entries");

        Ok(())
    }

    #[test]
    fn update_batch_with_compaction_test() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_update_batch_compact.bin");
        let mut tree = BPlusTree::<u32, Record>::new();

        // Create initial tree with larger data
        let large_data = "x".repeat(1000);
        for i in 0u32..100 {
            tree.insert(i, Record {
                id: i,
                data: large_data.clone(),
            });
        }
        tree.store(&filepath)?;
        drop(tree);

        let mut tree_update = BPlusTreeUpdate::<u32, Record>::try_new(&filepath)?;

        // Batch update with smaller data
        let small_data = "y".repeat(50);
        let updates: Vec<(u32, Record)> = (0u32..100)
            .map(|i| (i, Record { id: i, data: small_data.clone() }))
            .collect();

        let update_refs: Vec<(&u32, &Record)> = updates.iter()
            .map(|(k, v)| (k, v))
            .collect();

        tree_update.update_batch(&update_refs).map_err(|e| e.to_io())?;

        let size_before_compact = std::fs::metadata(&filepath)?.len();

        // Compact to reclaim space
        tree_update.compact(&filepath)?;

        let size_after_compact = std::fs::metadata(&filepath)?.len();
        assert!(size_after_compact < size_before_compact,
                "Compaction should reduce file size after batch update");

        // Verify all data is correct after compaction
        drop(tree_update);
        let mut tree_query = BPlusTreeQuery::<u32, Record>::try_new(&filepath)?;
        for i in 0u32..100 {
            let val = tree_query.query(&i).expect("Query failed").expect("Should find key after compaction");
            assert_eq!(val.data, small_data, "Data should be updated after compaction");
        }

        Ok(())
    }


    #[test]
    fn compact_reopen_test() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_compact_reopen.bin");
        let mut tree = BPlusTree::<u32, Record>::new();

        // Initial write
        tree.insert(1, Record { id: 1, data: "Initial".to_string() });
        tree.store(&filepath)?;
        drop(tree);

        let mut tree_update = BPlusTreeUpdate::<u32, Record>::try_new(&filepath)?;

        // 1. Write something
        let r2 = Record { id: 2, data: "BeforeCompact".to_string() };
        tree_update.upsert_batch(&[(&2, &r2)])?;

        // 2. Compact (this replaces the file)
        tree_update.compact(&filepath)?;

        // 3. Write something else
        let r3 = Record { id: 3, data: "AfterCompact".to_string() };
        tree_update.upsert_batch(&[(&3, &r3)])?;

        drop(tree_update);

        // Verify all data is present in the NEW file
        let mut tree_check = BPlusTreeQuery::<u32, Record>::try_new(&filepath)?;

        assert!(tree_check.query(&1).map_err(|e| e.to_io())?.is_some(), "Should have key 1");
        assert!(tree_check.query(&2).map_err(|e| e.to_io())?.is_some(), "Should have key 2");
        assert!(tree_check.query(&3).map_err(|e| e.to_io())?.is_some(), "Should have key 3 - if missing, file handle wasn't updated");

        Ok(())
    }

    #[test]
    fn upsert_batch_mixed_test() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_upsert_batch_mixed.bin");
        let mut tree = BPlusTree::<u32, Record>::new();

        // Create initial tree with keys 0-49
        for i in 0u32..50 {
            tree.insert(i, Record {
                id: i,
                data: format!("Initial {i}"),
            });
        }
        tree.store(&filepath)?;
        drop(tree);

        let mut tree_update = BPlusTreeUpdate::<u32, Record>::try_new(&filepath)?;

        // Prepare upsert batch: update existing keys 0-24, insert new keys 50-74
        let mut updates: Vec<(u32, Record)> = Vec::new();

        // Updates to existing keys
        for i in 0u32..25 {
            updates.push((i, Record { id: i, data: format!("Updated {i}") }));
        }

        // Inserts for new keys
        for i in 50u32..75 {
            updates.push((i, Record { id: i, data: format!("Inserted {i}") }));
        }

        let update_refs: Vec<(&u32, &Record)> = updates.iter()
            .map(|(k, v)| (k, v))
            .collect();

        tree_update.upsert_batch(&update_refs)?;
        drop(tree_update);

        // Verify all 75 entries exist with correct values
        let mut tree_query = BPlusTreeQuery::<u32, Record>::try_new(&filepath)?;

        // Check updated keys (0-24)
        for i in 0u32..25 {
            let val = tree_query.query(&i).expect("Query failed").expect("Should find updated key");
            assert_eq!(val.data, format!("Updated {i}"), "Key {i} should be updated");
        }

        // Check unchanged keys (25-49)
        for i in 25u32..50 {
            let val = tree_query.query(&i).expect("Query failed").expect("Should find unchanged key");
            assert_eq!(val.data, format!("Initial {i}"), "Key {i} should remain unchanged");
        }

        // Check inserted keys (50-74)
        for i in 50u32..75 {
            let val = tree_query.query(&i).expect("Query failed").expect("Should find inserted key");
            assert_eq!(val.data, format!("Inserted {i}"), "Key {i} should be inserted");
        }

        Ok(())
    }

    #[test]
    fn upsert_batch_all_new_test() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_upsert_batch_new.bin");
        let mut tree = BPlusTree::<u32, Record>::new();

        // Create initial tree with unrelated keys
        for i in 0u32..10 {
            tree.insert(i, Record {
                id: i,
                data: format!("Initial {i}"),
            });
        }
        tree.store(&filepath)?;
        drop(tree);

        let mut tree_update = BPlusTreeUpdate::<u32, Record>::try_new(&filepath)?;

        // Upsert all new keys (100-149)
        let updates: Vec<(u32, Record)> = (100u32..150)
            .map(|i| (i, Record { id: i, data: format!("New {i}") }))
            .collect();

        let update_refs: Vec<(&u32, &Record)> = updates.iter()
            .map(|(k, v)| (k, v))
            .collect();

        tree_update.upsert_batch(&update_refs)?;
        drop(tree_update);

        // Verify all keys exist
        let mut tree_query = BPlusTreeQuery::<u32, Record>::try_new(&filepath)?;

        // Original keys should still exist
        for i in 0u32..10 {
            let val = tree_query.query(&i).expect("Query failed").expect("Should find original key");
            assert_eq!(val.data, format!("Initial {i}"));
        }

        // New keys should be inserted
        for i in 100u32..150 {
            let val = tree_query.query(&i).expect("Query failed").expect("Should find new key");
            assert_eq!(val.data, format!("New {i}"));
        }

        Ok(())
    }

    #[test]
    fn upsert_batch_all_existing_test() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_upsert_batch_existing.bin");
        let mut tree = BPlusTree::<u32, Record>::new();

        // Create initial tree
        for i in 0u32..100 {
            tree.insert(i, Record {
                id: i,
                data: format!("Initial {i}"),
            });
        }
        tree.store(&filepath)?;
        drop(tree);

        let mut tree_update = BPlusTreeUpdate::<u32, Record>::try_new(&filepath)?;

        // Upsert all existing keys (should behave like update)
        let updates: Vec<(u32, Record)> = (0u32..100)
            .map(|i| (i, Record { id: i, data: format!("Updated {i}") }))
            .collect();

        let update_refs: Vec<(&u32, &Record)> = updates.iter()
            .map(|(k, v)| (k, v))
            .collect();

        tree_update.upsert_batch(&update_refs)?;
        drop(tree_update);

        // Verify all values were updated
        let reloaded_tree = BPlusTree::<u32, Record>::load(&filepath)?;
        let mut count = 0;
        for (key, value) in &reloaded_tree {
            assert_eq!(value.data, format!("Updated {key}"), "All keys should be updated");
            count += 1;
        }
        assert_eq!(count, 100, "Should have exactly 100 entries");

        Ok(())
    }

    #[test]
    fn test_value_packing_efficiency() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("packing_test.bin");
        let mut tree = BPlusTree::<u32, String>::new();

        // Insert 1000 small values (approx 50 bytes each)
        let small_value = "x".repeat(50);
        let count = 1000;
        for i in 0..count {
            tree.insert(i, small_value.clone());
        }

        tree.store(&filepath)?;

        let file_size = std::fs::metadata(&filepath)?.len();

        // Expected size without packing:
        // 1000 items * 4096 bytes/block = 4,096,000 bytes (~4MB)
        // Plus internal nodes
        let unpacked_size_estimate = count as u64 * super::PAGE_SIZE_USIZE as u64;

        println!("File size with packing: {} bytes", file_size);
        println!("Estimated unpacked size: {} bytes", unpacked_size_estimate);

        // We expect significant savings.
        // 1000 items * ~60 bytes / 4096 bytes/block ~= 15 blocks
        // Plus tree structure overhead.
        // Let's be conservative and say it should be less than 10% of unpacked size.
        assert!(file_size < unpacked_size_estimate / 10, "Packing should reduce size by at least 90%");

        Ok(())
    }

    #[test]
    fn test_mixed_value_packing() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("mixed_packing.bin");
        let mut tree = BPlusTree::<u32, String>::new();

        // Insert mixed values:
        // 0-99: Small (50 bytes) -> Packed
        // 100-109: Large (5000 bytes) -> Single (2 blocks)
        // 110-209: Small (50 bytes) -> Packed

        // Insert in order
        for i in 0..100 {
            tree.insert(i, "s".repeat(50));
        }
        for i in 100..110 {
            tree.insert(i, "L".repeat(5000));
        }
        for i in 110..210 {
            tree.insert(i, "s".repeat(50));
        }

        tree.store(&filepath)?;

        // Verify we can read them back correctly
        let mut query = BPlusTreeQuery::<u32, String>::try_new(&filepath)?;

        for i in 0..100 {
            let val = query.query(&i).expect("Query failed").expect("Should find small value");
            assert_eq!(val.len(), 50);
        }
        for i in 100..110 {
            let val = query.query(&i).expect("Query failed").expect("Should find large value");
            assert_eq!(val.len(), 5000);
        }
        for i in 110..210 {
            let val = query.query(&i).expect("Query failed").expect("Should find small value 2");
            assert_eq!(val.len(), 50);
        }

        Ok(())
    }
    #[test]
    fn test_upsert_huge_values_chunking() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_upsert_huge.bin");

        // Initialize tree
        let mut tree = BPlusTree::<u32, String>::new();
        tree.store(&filepath)?;
        drop(tree);

        let mut tree_update = BPlusTreeUpdate::<u32, String>::try_new(&filepath)?;

        // Insert values > PAGE_SIZE_USIZE (4096).
        // 10K value -> 3 chunks.
        let val1 = "A".repeat(10000);
        let val2 = "B".repeat(10000);

        let updates = vec![
            (1, val1.clone()),
            (2, val2.clone()),
        ];

        let update_refs: Vec<(&u32, &String)> = updates.iter().map(|(k, v)| (k, v)).collect();
        tree_update.upsert_batch(&update_refs)?;
        drop(tree_update);

        let mut query = BPlusTreeQuery::<u32, String>::try_new(&filepath)?;
        assert_eq!(query.query(&1).unwrap(), Some(val1));
        assert_eq!(query.query(&2).unwrap(), Some(val2));

        Ok(())
    }

    #[test]
    fn test_upsert_deep_split() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_upsert_split.bin");
        let mut tree = BPlusTree::<u32, u32>::new();
        tree.store(&filepath)?;
        drop(tree);

        let mut tree_update = BPlusTreeUpdate::<u32, u32>::try_new(&filepath)?;

        // Insert 5000 items.
        // 5000 items ensures at least Root -> Internal -> Leaf split (Height 2 or 3).

        let count = 5000;
        let mut updates = Vec::with_capacity(count);
        for i in 0..count {
            let val = u32::try_from(i).unwrap();
            updates.push((val, val)); // value matches key
        }

        // Split into batches to test multiple batch ops
        for chunk in updates.chunks(1000) {
            let chunk_refs: Vec<(&u32, &u32)> = chunk.iter().map(|(k, v)| (k, v)).collect();
            tree_update.upsert_batch(&chunk_refs)?;
        }
        drop(tree_update);

        // Validation
        let mut query = BPlusTreeQuery::<u32, u32>::try_new(&filepath)?;
        for i in 0..count {
            let k = u32::try_from(i).unwrap();
            let val = query.query(&k).unwrap();
            assert_eq!(val, Some(k));
        }
        Ok(())
    }

    #[test]
    fn test_upsert_batch_overwrites() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_upsert_overwrite.bin");
        let mut tree = BPlusTree::<u32, String>::new();
        tree.store(&filepath)?;
        drop(tree);

        let mut tree_update = BPlusTreeUpdate::<u32, String>::try_new(&filepath)?;

        // Batch contains same key multiple times
        let updates = vec![
            (1, "First".to_string()),
            (1, "Second".to_string()),
            (2, "Two".to_string()),
            (1, "Third".to_string()),
        ];

        let update_refs: Vec<(&u32, &String)> = updates.iter().map(|(k, v)| (k, v)).collect();
        tree_update.upsert_batch(&update_refs)?;
        drop(tree_update);

        let mut query = BPlusTreeQuery::<u32, String>::try_new(&filepath)?;
        assert_eq!(query.query(&1).unwrap(), Some("Third".to_string()));
        assert_eq!(query.query(&2).unwrap(), Some("Two".to_string()));
        Ok(())
    }

    #[test]
    fn test_compaction_packing_limits() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_compact_pack.bin");
        let mut tree = BPlusTree::<u32, String>::new();
        tree.store(&filepath)?;
        drop(tree);

        let mut tree_update = BPlusTreeUpdate::<u32, String>::try_new(&filepath)?;

        // Insert 200 items of 100 bytes.
        // Upsert creates Single blocks blocks block-aligned.
        // File size > 200 * 4096 = 800KB.

        let val = "x".repeat(100);
        let count = 200;
        let mut updates = Vec::new();
        for i in 0..count {
            updates.push((i, val.clone()));
        }
        // Use individual upsert calls to create fragmentation (one path write per update)
        for (k, v) in updates {
            let refs = [(&k, &v)];
            tree_update.upsert_batch(&refs)?;
        }

        let size_before = std::fs::metadata(&filepath)?.len();
        // Each individual update writes a full path, increasing file size significantly.
        assert!(size_before > count as u64 * 4000);

        // Now Compact
        tree_update.compact(&filepath)?;
        drop(tree_update);

        let size_after = std::fs::metadata(&filepath)?.len();
        // 200 items * 100 bytes = 20KB payload.
        // Should pack into ~5-6 blocks (4KB each).

        println!("Size before: {}, Size after: {}", size_before, size_after);
        assert!(size_after < size_before / 10, "Compaction should pack values");
        assert!(size_after < 100 * 1024, "File should be small"); // < 100KB

        // Verify data
        let mut query = BPlusTreeQuery::<u32, String>::try_new(&filepath)?;
        for i in 0..count {
            assert_eq!(query.query(&i).unwrap(), Some(val.clone()));
        }

        Ok(())
    }

    #[test]
    fn test_large_keys_multiblock_node() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_multiblock.bin");

        let mut tree = BPlusTree::<String, u32>::new();

        // 5 keys of 2000 bytes each. Total ~10KB keys.
        // Should span ~3 blocks (4KB each).
        for i in 0..5 {
            let key = format!("{:04}{}", i, "a".repeat(2000));
            tree.insert(key, i);
        }

        tree.store(&filepath)?;
        drop(tree);

        let loaded = BPlusTree::<String, u32>::load(&filepath)?;
        for i in 0..5 {
            let key = format!("{:04}{}", i, "a".repeat(2000));
            assert_eq!(loaded.query(&key), Some(i).as_ref());
        }
        Ok(())
    }

    #[test]
    fn test_upsert_multiblock_node() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_multiblock_upsert.bin");

        let mut tree = BPlusTree::<String, u32>::new();
        tree.store(&filepath)?;
        drop(tree);

        let mut updater = BPlusTreeUpdate::<String, u32>::try_new(&filepath)?;

        // Upsert large keys
        let mut batch = Vec::new();
        let keys: Vec<String> = (0..5).map(|i| format!("{:04}{}", i, "b".repeat(2000))).collect();
        let vals: Vec<u32> = (0..5).collect();

        for i in 0..5 {
            batch.push((&keys[i], &vals[i]));
        }

        updater.upsert_batch(&batch)?;
        drop(updater);

        let mut query = BPlusTreeQuery::<String, u32>::try_new(&filepath)?;
        for i in 0..5 {
            let val = query.query(&keys[i]).unwrap();
            assert_eq!(val, Some(vals[i]));
        }
        Ok(())
    }

    #[test]
    fn test_node_serialization_overhead() -> io::Result<()> {
        use crate::utils::binary_serialize;
        use crate::repository::bplustree::{ValueInfo, ValueStorageMode, PAGE_SIZE_USIZE};

        // Simulate a leaf node with u32 keys and ValueInfo
        let key_counts = [10, 30, 50, 80, 100];

        for count in key_counts {
            let keys: Vec<u32> = (0..count).collect();
            let value_info: Vec<ValueInfo> = (0..count)
                .map(|i| ValueInfo {
                    mode: ValueStorageMode::Packed(i as u64 * 4096, (i % 16) as u16),
                    length: 100,
                    compressed_cache: None,
                })
                .collect();

            let keys_serialized = binary_serialize(&keys)?;
            let info_serialized = binary_serialize(&value_info)?;

            // Total content: flag(1) + keys_len(4) + keys + info_len(4) + info
            let total = 1 + 4 + keys_serialized.len() + 4 + info_serialized.len();
            let fits_in_block = total <= PAGE_SIZE_USIZE;

            println!(
                "Keys={}: keys_bytes={}, info_bytes={}, total={}, fits_in_block={}",
                count, keys_serialized.len(), info_serialized.len(), total, fits_in_block
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod page_tests {
    use super::*;

    #[test]
    fn test_page_initialization() {
        let mut data = [0u8; PAGE_SIZE_USIZE];
        let page = SlottedPage::new(&mut data, PageType::Leaf).expect("Init failed");
        assert_eq!(page.header.page_type, PageType::Leaf);
        assert_eq!(page.header.cell_count, 0);
        assert_eq!(page.header.free_start, PAGE_HEADER_SIZE as u16);
        assert_eq!(page.header.free_end, PAGE_SIZE as u16);
        assert_eq!(page.free_space(), PAGE_SIZE_USIZE - PAGE_HEADER_SIZE_USIZE);
    }

    #[test]
    fn test_insert_get() {
        let mut data = [0u8; PAGE_SIZE_USIZE];
        let mut page = SlottedPage::new(&mut data, PageType::Leaf).expect("Init failed");

        let val1 = b"hello";
        let val2 = b"world";

        // Insert length-prefixed for test realism
        let mut cell1 = Vec::new();
        cell1.extend_from_slice(&(val1.len() as u32).to_le_bytes());
        cell1.extend_from_slice(val1);

        let mut cell2 = Vec::new();
        cell2.extend_from_slice(&(val2.len() as u32).to_le_bytes());
        cell2.extend_from_slice(val2);

        page.insert_at_index(0, &cell1).unwrap();
        page.insert_at_index(1, &cell2).unwrap();

        assert_eq!(page.header.cell_count, 2);

        let read1 = page.get_cell(0).expect("Get cell 0");
        assert_eq!(&read1[4..], val1);

        let read2 = page.get_cell(1).expect("Get cell 1");
        assert_eq!(&read2[4..], val2);
    }

    #[test]
    fn test_split_off() {
        let mut data = [0u8; PAGE_SIZE_USIZE];
        let mut page = SlottedPage::new(&mut data, PageType::Leaf).expect("Init failed");

        let payload = vec![0xAAu8; 500];
        let mut cell = Vec::new();
        cell.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        cell.extend_from_slice(&payload);

        for i in 0..6 {
            page.insert_at_index(i, &cell).unwrap();
        }

        assert_eq!(page.header.cell_count, 6);

        let new_page_bytes = page.split_off().expect("Split failed").expect("Should have split");

        // Check original page
        assert_eq!(page.header.cell_count, 3);

        // Check new page
        let header = PageHeader::deserialize(&new_page_bytes[..PAGE_HEADER_SIZE_USIZE]).expect("Deserialize failed");
        assert_eq!(header.cell_count, 3);
    }

    #[test]
    fn test_split_off_edge_cases() {
        let mut data = [0u8; PAGE_SIZE_USIZE];
        let mut page = SlottedPage::new(&mut data, PageType::Leaf).expect("Init failed");

        // Case 0: Split empty page -> Should Error
        let res = page.split_off();
        assert!(matches!(res, Err(PageError::InvalidIndex)));

        // Case 1: Split single item page -> Should return None (no-op)
        let val = b"item";
        let mut cell = Vec::new();
        cell.extend_from_slice(&(val.len() as u32).to_le_bytes());
        cell.extend_from_slice(val);
        page.insert_at_index(0, &cell).unwrap();

        let res = page.split_off();
        match res {
            Ok(None) => {
                assert_eq!(page.header.cell_count, 1); // Original page untouched
            }
            Ok(Some(_)) => panic!("Split of single item should result in None"),
            Err(e) => panic!("Split of single item should result in no-op, not error: {:?}", e),
        }
    }
}


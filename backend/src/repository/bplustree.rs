use crate::{
    repository::storage::get_file_path_for_db_index,
    utils,
    utils::{binary_deserialize, binary_serialize, binary_serialize_into},
};
use fs2::FileExt as _;
use indexmap::IndexMap;
use log::error;
use memmap2::Mmap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use shared::error::{string_to_io_error, to_io_error};
#[cfg(unix)]
use std::os::unix::fs::{FileExt, MetadataExt};
use std::{
    ffi::OsString,
    fs::{File, Metadata, OpenOptions},
    io::{self, BufReader, Read, Seek, SeekFrom, Write},
    marker::PhantomData,
    mem::size_of,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::{Duration, Instant, UNIX_EPOCH},
};
use tempfile::NamedTempFile;
#[cfg(windows)]
use windows_sys::Win32::Foundation::CloseHandle;
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

// Constants (Restored)
const PAGE_SIZE: u16 = 4096;
pub const PAGE_SIZE_USIZE: usize = PAGE_SIZE as usize;
const LEN_SIZE: usize = 4;
const FLAG_SIZE: usize = 1;
pub(crate) const MAGIC: &[u8; 4] = b"BTRE";
pub(crate) const STORAGE_VERSION: u32 = 2;
const HEADER_SIZE: u64 = PAGE_SIZE as u64;
const ROOT_OFFSET_POS: u64 = 8;
const METADATA_OFFSET_POS: u64 = 16;
const METADATA_DATA_START_POS: usize = 20;
// Reserve space for metadata (e.g. 4096 - 16 = 4080 bytes max, but let's be safe)
const METADATA_MAX_SIZE: usize = 4000;
const HEADER_FLAG_HAS_METADATA_FLAGS: u32 = 1 << 31;
const HEADER_FLAG_HAS_TOMBSTONES: u32 = 1 << 30;
const HEADER_METADATA_LEN_MASK: u32 = !(HEADER_FLAG_HAS_METADATA_FLAGS | HEADER_FLAG_HAS_TOMBSTONES);

#[inline]
const fn encode_metadata_len_with_flags(metadata_len: u32, has_tombstones: bool) -> u32 {
    let mut encoded = metadata_len | HEADER_FLAG_HAS_METADATA_FLAGS;
    if has_tombstones {
        encoded |= HEADER_FLAG_HAS_TOMBSTONES;
    }
    encoded
}

#[inline]
const fn decode_metadata_len_and_flags(raw: u32) -> (u32, bool) {
    let metadata_len = raw & HEADER_METADATA_LEN_MASK;
    let has_metadata_flags = (raw & HEADER_FLAG_HAS_METADATA_FLAGS) != 0;
    let has_tombstones = if has_metadata_flags {
        (raw & HEADER_FLAG_HAS_TOMBSTONES) != 0
    } else {
        // Legacy v2 files (without header flags) are treated conservatively:
        // assume tombstones may exist until a rewrite/compact writes proper flags.
        true
    };
    (metadata_len, has_tombstones)
}

const POINTER_SIZE: usize = 8;
const INFO_SIZE: usize = 12; // (u64, u32)

// Maximum number of blocks to cache in memory (~4MB at 4KB per block)
const CACHE_CAPACITY: usize = 1024;

// MessagePack overhead estimation - increased to handle binary types like UUIDType
// which use bin8/bin16/bin32 encoding with additional header bytes
const MSGPACK_OVERHEAD_PER_ENTRY: usize = 8;
// Additional array overhead for the entire keys vector serialization
const MSGPACK_ARRAY_OVERHEAD: usize = 5;
// Safety factor to ensure we never exceed page size (25% margin)
const ORDER_SAFETY_FACTOR: usize = 75; // Use only 75% of theoretical capacity

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
    B+Tree File Layout
    ==================

    ┌─────────────────────────────────────────────────────────────┐
    │ File Header (PAGE_SIZE bytes, currently 4096)               │
    ├─────────────────────────────────────────────────────────────┤
    │ MAGIC [4B: "BTRE"]                                          │
    │ VERSION [4B: u32]                                           │
    │ ROOT_OFFSET [8B: u64]                                       │
    │ METADATA_LEN_FLAGS [4B: u32]                                │
    │   bit31: metadata flags initialized                         │
    │   bit30: has_tombstones                                     │
    │   bits0..29: metadata length                                │
    │ METADATA [variable, up to 4000B]                            │
    │ [padding to PAGE_SIZE]                                      │
    └─────────────────────────────────────────────────────────────┘

    Leaf Node Layout (single or multi-block)
    ┌─────────────────────────────────────────────────────────────┐
    │ IS_LEAF [1B: 0x01]                                          │
    │ KEYS_LEN [4B: u32]                                          │
    │ KEYS [MessagePack serialized Vec<K>]                        │
    │ VALUE_INFO_LEN [4B: u32]                                    │
    │ VALUE_INFO [MessagePack serialized Vec<ValueInfo>]          │
    │ [padding to block boundary]                                 │
    └─────────────────────────────────────────────────────────────┘

    Internal Node Layout (supports multi-block when content exceeds PAGE_SIZE)
    ┌─────────────────────────────────────────────────────────────┐
    │ IS_LEAF [1B: 0x00]                                          │
    │ KEYS_LEN [4B: u32]                                          │
    │ KEYS [MessagePack serialized Vec<K>]                        │
    │ POINTERS_LEN [4B: u32]                                      │
    │ POINTERS [MessagePack serialized Vec<u64>]                  │
    │ [padding to block boundary]                                 │
    └─────────────────────────────────────────────────────────────┘

    Note: Internal nodes can span multiple PAGE_SIZE blocks when
    keys + pointers exceed a single page. The order calculation
    uses a 75% safety factor to minimize multi-block nodes.

    Value Storage Modes:
    - Single: Large values stored at [offset] with optional LZ4 compression
      Format: [FLAG:1B][payload...] where FLAG = 0x00 (raw) or 0x01 (LZ4)
    - Packed: Small values (≤256B) packed into PAGE_SIZE blocks
      Format: [COUNT:4B][LEN:4B][data...][LEN:4B][data...]...
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

        Ok(Self { page_type, cell_count, free_start, free_end, right_sibling, checksum: 0 })
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
    fn from(err: io::Error) -> Self { PageError::Io(err) }
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
    fn from(err: io::Error) -> Self { BPlusTreeError::Io(err) }
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
        if slot_pos + 2 > self.data.len() {
            return None;
        }
        let offset = u16::from_le_bytes(self.data[slot_pos..slot_pos + 2].try_into().ok()?);

        // Bounds check for length header
        if (offset as usize) + 4 > self.data.len() {
            return None;
        }
        let len = u32::from_le_bytes(self.data[offset as usize..offset as usize + 4].try_into().ok()?) as usize;

        if (offset as usize) + 4 + len > self.data.len() {
            return None;
        }
        Some(&self.data[offset as usize..offset as usize + 4 + len])
    }

    pub fn get_cell_offset(&self, index: usize) -> Option<u16> {
        let slot_pos = PAGE_HEADER_SIZE_USIZE + (index * SLOT_SIZE);
        if slot_pos + 2 > self.data.len() {
            return None;
        }
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

        if split_idx >= count {
            split_idx = count.saturating_sub(1);
        }
        if split_idx == 0 && count > 1 {
            split_idx = 1;
        }

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
fn u32_from_bytes(bytes: &[u8]) -> io::Result<u32> { Ok(u32::from_le_bytes(bytes.try_into().map_err(to_io_error)?)) }

#[inline]
fn u64_from_bytes(bytes: &[u8]) -> io::Result<u64> { Ok(u64::from_le_bytes(bytes.try_into().map_err(to_io_error)?)) }

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

// Provide zero-copy scanning of MessagePack-encoded keys in B+tree internal nodes.
// This avoids deserializing the entire keys vector (which allocates for each key in the node)
// by scanning the raw bytes directly to find the correct child pointer.

/// Result of parsing a `MessagePack` array header
struct MsgPackArrayHeader {
    /// Number of elements in the array
    count: usize,
    /// Number of bytes consumed by the header
    header_size: usize,
}

/// Parse `MessagePack` array header to get element count
/// Returns (count, `header_bytes_consumed`)
#[inline]
fn parse_msgpack_array_header(bytes: &[u8]) -> Option<MsgPackArrayHeader> {
    if bytes.is_empty() {
        return None;
    }
    let first = bytes[0];

    // fixarray (0x90-0x9f): count in low 4 bits
    if (0x90..=0x9f).contains(&first) {
        return Some(MsgPackArrayHeader { count: (first & 0x0f) as usize, header_size: 1 });
    }

    // array16 (0xdc): next 2 bytes are big-endian count
    if first == 0xdc && bytes.len() >= 3 {
        let count = u16::from_be_bytes([bytes[1], bytes[2]]) as usize;
        return Some(MsgPackArrayHeader { count, header_size: 3 });
    }

    // array32 (0xdd): next 4 bytes are big-endian count
    if first == 0xdd && bytes.len() >= 5 {
        let count = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
        return Some(MsgPackArrayHeader { count, header_size: 5 });
    }

    None
}

/// Trait for types that can be scanned directly from `MessagePack` bytes without allocation.
///
/// This enables zero-copy key scanning in B+tree internal nodes, avoiding the need
/// to deserialize the entire keys vector just to find which child pointer to follow.
pub trait MsgPackScannable: Ord + Sized {
    /// Compare self with a key encoded at the given position in the byte slice.
    /// Returns:
    /// - `Some((ordering, bytes_consumed))` if successfully parsed
    /// - `None` if parsing failed
    fn compare_at_position(&self, bytes: &[u8]) -> Option<(std::cmp::Ordering, usize)>;

    /// Skip over a key at the given position without comparing.
    /// Returns the number of bytes consumed, or None if parsing failed.
    fn skip_at_position(bytes: &[u8]) -> Option<usize>;
}

impl MsgPackScannable for u32 {
    #[inline]
    fn compare_at_position(&self, bytes: &[u8]) -> Option<(std::cmp::Ordering, usize)> {
        if bytes.is_empty() {
            return None;
        }

        let first = bytes[0];

        // Positive fixint (0x00-0x7f): value is the byte itself
        if first <= 0x7f {
            let value = u32::from(first);
            return Some((self.cmp(&value), 1));
        }

        // uint8 (0xcc): next byte is the value
        if first == 0xcc && bytes.len() >= 2 {
            let value = u32::from(bytes[1]);
            return Some((self.cmp(&value), 2));
        }

        // uint16 (0xcd): next 2 bytes are big-endian value
        if first == 0xcd && bytes.len() >= 3 {
            let value = u32::from(u16::from_be_bytes([bytes[1], bytes[2]]));
            return Some((self.cmp(&value), 3));
        }

        // uint32 (0xce): next 4 bytes are big-endian value
        if first == 0xce && bytes.len() >= 5 {
            let value = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
            return Some((self.cmp(&value), 5));
        }

        None
    }

    #[inline]
    fn skip_at_position(bytes: &[u8]) -> Option<usize> {
        if bytes.is_empty() {
            return None;
        }

        let first = bytes[0];

        if first <= 0x7f {
            return Some(1);
        } // fixint
        if first == 0xcc {
            return Some(2);
        } // uint8
        if first == 0xcd {
            return Some(3);
        } // uint16
        if first == 0xce {
            return Some(5);
        } // uint32
        if first == 0xcf {
            return Some(9);
        } // uint64

        // Negative fixint (0xe0-0xff)
        if first >= 0xe0 {
            return Some(1);
        }

        // int8 (0xd0)
        if first == 0xd0 {
            return Some(2);
        }
        // int16 (0xd1)
        if first == 0xd1 {
            return Some(3);
        }
        // int32 (0xd2)
        if first == 0xd2 {
            return Some(5);
        }
        // int64 (0xd3)
        if first == 0xd3 {
            return Some(9);
        }

        None
    }
}

impl MsgPackScannable for String {
    #[inline]
    fn compare_at_position(&self, bytes: &[u8]) -> Option<(std::cmp::Ordering, usize)> {
        if bytes.is_empty() {
            return None;
        }

        let first = bytes[0];

        // fixstr (0xa0-0xbf): length in low 5 bits
        if (0xa0..=0xbf).contains(&first) {
            let len = (first & 0x1f) as usize;
            if bytes.len() > len {
                let str_bytes = &bytes[1..=len];
                // Compare as bytes (valid UTF-8 has same ordering as String)
                let ordering = self.as_bytes().cmp(str_bytes);
                return Some((ordering, 1 + len));
            }
            return None;
        }

        // str8 (0xd9): 1 byte length
        if first == 0xd9 && bytes.len() >= 2 {
            let len = bytes[1] as usize;
            if bytes.len() >= 2 + len {
                let str_bytes = &bytes[2..2 + len];
                let ordering = self.as_bytes().cmp(str_bytes);
                return Some((ordering, 2 + len));
            }
            return None;
        }

        // str16 (0xda): 2 byte length (big-endian)
        if first == 0xda && bytes.len() >= 3 {
            let len = u16::from_be_bytes([bytes[1], bytes[2]]) as usize;
            if bytes.len() >= 3 + len {
                let str_bytes = &bytes[3..3 + len];
                let ordering = self.as_bytes().cmp(str_bytes);
                return Some((ordering, 3 + len));
            }
            return None;
        }

        // str32 (0xdb): 4 byte length (big-endian)
        if first == 0xdb && bytes.len() >= 5 {
            let len = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
            if bytes.len() >= 5 + len {
                let str_bytes = &bytes[5..5 + len];
                let ordering = self.as_bytes().cmp(str_bytes);
                return Some((ordering, 5 + len));
            }
            return None;
        }

        None
    }

    #[inline]
    fn skip_at_position(bytes: &[u8]) -> Option<usize> {
        if bytes.is_empty() {
            return None;
        }

        let first = bytes[0];

        // fixstr (0xa0-0xbf)
        if (0xa0..=0xbf).contains(&first) {
            let len = (first & 0x1f) as usize;
            return Some(1 + len);
        }

        // str8 (0xd9)
        if first == 0xd9 && bytes.len() >= 2 {
            let len = bytes[1] as usize;
            return Some(2 + len);
        }

        // str16 (0xda)
        if first == 0xda && bytes.len() >= 3 {
            let len = u16::from_be_bytes([bytes[1], bytes[2]]) as usize;
            return Some(3 + len);
        }

        // str32 (0xdb)
        if first == 0xdb && bytes.len() >= 5 {
            let len = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
            return Some(5 + len);
        }

        None
    }
}

/// Find the child index for internal node traversal using zero-copy key scanning.
///
/// This performs a linear scan through the MessagePack-encoded keys array,
/// comparing each key without deserializing the entire vector.
///
/// Returns the index of the child pointer to follow (upper bound).
///
/// # Arguments
/// * `keys_bytes` - The raw `MessagePack` bytes of the keys array
/// * `search_key` - The key we're searching for
///
/// # Returns
/// * `Some(index)` - The child index to follow
/// * `None` - If parsing failed (caller should fall back to full deserialization)
#[inline]
fn find_child_index_zero_copy<K: MsgPackScannable>(keys_bytes: &[u8], search_key: &K) -> Option<usize> {
    let header = parse_msgpack_array_header(keys_bytes)?;
    let mut pos = header.header_size;

    // Linear scan through keys, finding upper bound
    for i in 0..header.count {
        if pos >= keys_bytes.len() {
            return None;
        }

        let (ordering, consumed) = search_key.compare_at_position(&keys_bytes[pos..])?;

        // Upper bound: first key > search_key
        if ordering == std::cmp::Ordering::Less {
            return Some(i);
        }

        pos += consumed;
    }

    // All keys are <= search_key, return count (rightmost child)
    Some(header.count)
}

/// Zero-copy scan result for internal nodes
struct ZeroCopyScanResult {
    /// Whether this is a leaf node
    is_leaf: bool,
    /// Child index to follow (for internal nodes)
    child_idx: usize,
    /// Pointers array bytes start position (offset from node start)
    pointers_start: usize,
}

/// Scan an internal node to find the correct child without full deserialization.
///
/// Node layout:
/// - `is_leaf`: 1 byte (`FLAG_SIZE`)
/// - `keys_len`: 4 bytes (`LEN_SIZE`)
/// - keys: `keys_len` bytes (`MessagePack` array)
/// - `pointers_len`: 4 bytes (`LEN_SIZE`)
/// - pointers: `pointers_len` bytes (`MessagePack` array of u64)
#[inline]
fn scan_internal_node_zero_copy<K: MsgPackScannable>(node_bytes: &[u8], search_key: &K) -> Option<ZeroCopyScanResult> {
    if node_bytes.len() < FLAG_SIZE + LEN_SIZE {
        return None;
    }

    let is_leaf = node_bytes[0] == 1;

    // Read keys_len
    let keys_len_end = FLAG_SIZE + LEN_SIZE;
    let keys_len = u32::from_le_bytes(node_bytes[FLAG_SIZE..keys_len_end].try_into().ok()?) as usize;

    let keys_start = FLAG_SIZE + LEN_SIZE;
    let keys_end = keys_start + keys_len;

    if keys_end + LEN_SIZE > node_bytes.len() {
        return None;
    }

    if is_leaf {
        // For leaf nodes, we can't use zero-copy for the full query
        // (need to return keys for binary search). Just return that it's a leaf.
        return Some(ZeroCopyScanResult { is_leaf: true, child_idx: 0, pointers_start: 0 });
    }

    // Scan keys to find child index
    let keys_bytes = &node_bytes[keys_start..keys_end];
    let child_idx = find_child_index_zero_copy(keys_bytes, search_key)?;

    // Read pointers_len
    let _pointers_len = u32::from_le_bytes(node_bytes[keys_end..keys_end + LEN_SIZE].try_into().ok()?) as usize;

    let pointers_start = keys_end + LEN_SIZE;

    // Parse pointer count from MessagePack array header

    Some(ZeroCopyScanResult { is_leaf: false, child_idx, pointers_start })
}

/// Read a specific child pointer from the pointers array without full deserialization.
#[inline]
fn read_pointer_at_index(pointers_bytes: &[u8], index: usize) -> Option<u64> {
    let header = parse_msgpack_array_header(pointers_bytes)?;

    if index >= header.count {
        return None;
    }

    let mut pos = header.header_size;

    // Skip to the target pointer
    for _ in 0..index {
        if pos >= pointers_bytes.len() {
            return None;
        }
        pos += skip_msgpack_u64(&pointers_bytes[pos..])?;
    }

    // Read the target pointer
    read_msgpack_u64(&pointers_bytes[pos..])
}

/// Skip a MessagePack-encoded u64
#[inline]
fn skip_msgpack_u64(bytes: &[u8]) -> Option<usize> {
    if bytes.is_empty() {
        return None;
    }

    let first = bytes[0];

    if first <= 0x7f {
        return Some(1);
    } // fixint
    if first == 0xcc {
        return Some(2);
    } // uint8
    if first == 0xcd {
        return Some(3);
    } // uint16
    if first == 0xce {
        return Some(5);
    } // uint32
    if first == 0xcf {
        return Some(9);
    } // uint64

    None
}

/// Read a MessagePack-encoded u64
#[inline]
fn read_msgpack_u64(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() {
        return None;
    }

    let first = bytes[0];

    // Positive fixint (0x00-0x7f)
    if first <= 0x7f {
        return Some(u64::from(first));
    }

    // uint8 (0xcc)
    if first == 0xcc && bytes.len() >= 2 {
        return Some(u64::from(bytes[1]));
    }

    // uint16 (0xcd)
    if first == 0xcd && bytes.len() >= 3 {
        return Some(u64::from(u16::from_be_bytes([bytes[1], bytes[2]])));
    }

    // uint32 (0xce)
    if first == 0xce && bytes.len() >= 5 {
        return Some(u64::from(u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]])));
    }

    // uint64 (0xcf)
    if first == 0xcf && bytes.len() >= 9 {
        return Some(u64::from_be_bytes([
            bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7], bytes[8],
        ]));
    }

    None
}

#[inline]
const fn msgpack_array_header_len(count: usize) -> usize {
    if count <= 0x0f {
        1
    } else if count <= u16::MAX as usize {
        3
    } else {
        5
    }
}

#[inline]
const fn msgpack_u64_array_upper_bound_len(count: usize) -> usize {
    // Worst-case per u64: marker + 8 bytes payload.
    msgpack_array_header_len(count) + count.saturating_mul(9)
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

    /// Entry is logically deleted.
    Tombstone,
}

#[derive(Debug, Clone)]
enum CacheData {
    Compressed(u8, Vec<u8>),
    PackedOffset(u16),
}

/// Extended value info that includes storage mode and length
#[derive(Debug, Serialize, Deserialize)]
struct ValueInfo {
    mode: ValueStorageMode,
    length: u32,
    #[serde(skip, default)]
    cache: Mutex<Option<CacheData>>,
}

impl ValueInfo {
    #[inline]
    const fn tombstone() -> Self { Self { mode: ValueStorageMode::Tombstone, length: 0, cache: Mutex::new(None) } }

    #[inline]
    const fn is_tombstone(&self) -> bool { matches!(self.mode, ValueStorageMode::Tombstone) }
}

impl Clone for ValueInfo {
    fn clone(&self) -> Self {
        Self {
            mode: self.mode,
            length: self.length,
            cache: Mutex::new(None), // Don't clone cache
        }
    }
}

/// Result of attempting an in-place value update
enum InPlaceUpdateResult {
    /// Update succeeded in-place, no node rewrite needed
    Success,
    /// Packed value was promoted to Single storage mode (node rewrite needed with new info)
    PromotedToSingle(ValueInfo),
    /// Value doesn't fit in existing space, need full COW
    NeedsCow,
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
        Self { is_leaf, keys: vec![], children: vec![], value_info: vec![], values: vec![] }
    }

    #[inline]
    fn is_overflow(&self, order: usize) -> bool { self.keys.len() > order }

    #[inline]
    const fn get_median_index(order: usize) -> usize { order >> 1 }

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

    fn get_entry_index_upper_bound(&self, key: &K) -> usize { get_entry_index_upper_bound::<K>(&self.keys, key) }

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
    fn calculate_serialized_size(&self, serial_buf: &mut Vec<u8>) -> io::Result<u64> {
        serial_buf.clear();

        // Header: is_leaf flag
        let mut size = FLAG_SIZE;

        // Keys: length + serialized data
        binary_serialize_into(&mut *serial_buf, &self.keys)?;
        size += LEN_SIZE + serial_buf.len();

        if self.is_leaf {
            // Leaf nodes now store value_info instead of values
            // value_info: length + Vec<(u64, u32)>
            // Reuse buf
            serial_buf.clear();
            binary_serialize_into(&mut *serial_buf, &self.value_info)?;
            size += LEN_SIZE + serial_buf.len();
        } else {
            // Internal node: pointer length + pointers
            // Pointer encoding is variable-length. Using small placeholder values
            // can underestimate node size and cause offset overlap.
            size += LEN_SIZE + msgpack_u64_array_upper_bound_len(self.children.len());
        }

        // Round up to block size
        let blocks = size.div_ceil(PAGE_SIZE_USIZE);
        Ok((blocks * PAGE_SIZE_USIZE) as u64)
    }

    fn serialize_to_block<W: Write + Seek>(
        &self,
        file: &mut W,
        buffer: &mut Vec<u8>,
        serial_buf: &mut Vec<u8>,
        offset: u64,
    ) -> io::Result<u64> {
        serial_buf.clear();
        binary_serialize_into(&mut *serial_buf, &self.keys)?;
        let keys_len = u32::try_from(serial_buf.len()).map_err(to_io_error)?;

        if self.is_leaf {
            let keys_end = serial_buf.len();
            // Append info_encoded to serial_buf to avoid second allocation
            binary_serialize_into(&mut *serial_buf, &self.value_info)?;
            let info_len = u32::try_from(serial_buf.len() - keys_end).map_err(to_io_error)?;
            let info_slice = &serial_buf[keys_end..];

            let content_size = FLAG_SIZE + LEN_SIZE + keys_len as usize + LEN_SIZE + info_len as usize;
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

            buffer[pos..pos + keys_len as usize].copy_from_slice(&serial_buf[0..keys_len as usize]);
            pos += keys_len as usize;

            buffer[pos..pos + LEN_SIZE].copy_from_slice(&info_len.to_le_bytes());
            pos += LEN_SIZE;

            buffer[pos..pos + info_len as usize].copy_from_slice(info_slice);

            file.write_all(&buffer[..capacity])?;

            Ok(offset + (blocks as u64 * PAGE_SIZE_USIZE as u64))
        } else {
            let ptr_count = self.children.len();
            // Conservative upper bound for MessagePack-encoded Vec<u64>.
            // Must not underestimate, otherwise child blocks can overlap.
            let ptr_encoded_size = msgpack_u64_array_upper_bound_len(ptr_count);

            let content_size = FLAG_SIZE + LEN_SIZE + keys_len as usize + LEN_SIZE + ptr_encoded_size;
            let blocks_needed = content_size.div_ceil(PAGE_SIZE_USIZE);

            let parent_start = offset;
            let mut current_offset = parent_start + (blocks_needed as u64 * PAGE_SIZE_USIZE as u64);

            let mut pointers = Vec::with_capacity(ptr_count);
            for child in &self.children {
                pointers.push(current_offset);
                let mut child_scratch = Vec::new(); // Separate scratch for recursion to protect our serial_buf
                current_offset = child.serialize_to_block(file, buffer, &mut child_scratch, current_offset)?;
            }

            // Append pointers to serial_buf
            let keys_end = serial_buf.len();
            binary_serialize_into(&mut *serial_buf, &pointers)?;
            let pointers_len = u32::try_from(serial_buf.len() - keys_end).map_err(to_io_error)?;
            let pointers_slice = &serial_buf[keys_end..];

            file.seek(SeekFrom::Start(parent_start))?;

            let total_capacity = blocks_needed * PAGE_SIZE_USIZE;
            if buffer.len() < total_capacity {
                buffer.resize(total_capacity, 0);
            }
            buffer[..total_capacity].fill(0);

            let mut pos = 0;
            // Is_leaf=0
            buffer[pos] = 0u8;
            pos += FLAG_SIZE;

            buffer[pos..pos + LEN_SIZE].copy_from_slice(&keys_len.to_le_bytes());
            pos += LEN_SIZE;
            buffer[pos..pos + keys_len as usize].copy_from_slice(&serial_buf[0..keys_len as usize]);
            pos += keys_len as usize;

            buffer[pos..pos + LEN_SIZE].copy_from_slice(&pointers_len.to_le_bytes());
            pos += LEN_SIZE;
            buffer[pos..pos + pointers_len as usize].copy_from_slice(pointers_slice);

            file.write_all(&buffer[..total_capacity])?;

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

        let mut serial_buf = Vec::with_capacity(PAGE_SIZE_USIZE);

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
                            serial_buf.clear();
                            binary_serialize_into(&mut serial_buf, value)?;
                            serialized_values.push(serial_buf.clone());
                        }

                        // Determine the packing structure with placeholder offsets (0)
                        // Final offsets will be assigned in Pass 3
                        let mut current_pack_index: u16 = 0;
                        let mut current_pack_size = PACK_BLOCK_HEADER_SIZE;
                        let mut pack_count = 0u32;

                        for value_bytes in serialized_values {
                            let size = value_bytes.len();

                            if size <= SMALL_VALUE_THRESHOLD {
                                let entry_size = PACK_VALUE_HEADER_SIZE + size;

                                if current_pack_size + entry_size <= PAGE_SIZE_USIZE {
                                    // Add to current pack
                                    node.value_info.push(ValueInfo {
                                        mode: ValueStorageMode::Packed(u64::from(pack_count), current_pack_index),
                                        length: u32::try_from(size).map_err(to_io_error)?,
                                        cache: Mutex::new(None),
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
                                        cache: Mutex::new(None),
                                    });
                                }
                            } else {
                                // Large value - use Single storage with optional compression
                                // Pre-calculate compressed size if applicable
                                let (flag, payload) = compress_if_beneficial(&value_bytes);
                                let stored_size = 1 + payload.len();

                                let cache = if flag == COMPRESSION_FLAG_LZ4 {
                                    Some(CacheData::Compressed(flag, payload))
                                } else {
                                    None // Don't cache uncompressed data to save memory
                                };

                                node.value_info.push(ValueInfo {
                                    mode: ValueStorageMode::Single(u64::MAX),
                                    length: u32::try_from(stored_size).map_err(to_io_error)?,
                                    cache: Mutex::new(cache),
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
            current_offset += self.calculate_serialized_size(&mut serial_buf)?;

            while !current_level.is_empty() {
                let mut next_level = Vec::new();
                for node in current_level {
                    if !node.is_leaf {
                        for child in &node.children {
                            let child_ptr = std::ptr::from_ref(child);
                            node_offset_map.insert(child_ptr, current_offset);
                            current_offset += child.calculate_serialized_size(&mut serial_buf)?;
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
                                ValueStorageMode::Single(_) | ValueStorageMode::Tombstone => {}
                            }
                        }

                        // Second pass: update pack indices to actual offsets
                        for info in &mut node.value_info {
                            if let ValueStorageMode::Packed(pack_idx, index) = &mut info.mode {
                                let actual_offset = pack_block_offsets[pack_idx];
                                *pack_idx = actual_offset;
                                let _ = index;
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
                        node.serialize_to_block(file, buffer, &mut serial_buf, node_offset)?;
                    } else {
                        let child_offsets: Vec<u64> =
                            node.children.iter().map(|c| node_offset_map[&std::ptr::from_ref(c)]).collect();

                        node.serialize_internal_with_offsets(
                            file,
                            buffer,
                            &mut serial_buf,
                            node_offset,
                            &child_offsets,
                        )?;
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
                            serial_buf.clear();
                            binary_serialize_into(&mut serial_buf, value)?;

                            match info.mode {
                                ValueStorageMode::Packed(block_offset, index) => {
                                    pack_blocks.entry(block_offset).or_default().push((index, serial_buf.clone()));
                                }
                                ValueStorageMode::Single(block_offset) => {
                                    // Write single value with compression format
                                    file.seek(SeekFrom::Start(block_offset))?;

                                    // Apply adaptive compression or use cache
                                    let cache_guard = info.cache.lock();
                                    let (flag, payload_ref) = if let Some(cache_data) = cache_guard.as_ref() {
                                        if let CacheData::Compressed(c_flag, c_payload) = cache_data {
                                            (*c_flag, c_payload.as_slice())
                                        } else {
                                            (COMPRESSION_FLAG_NONE, serial_buf.as_slice())
                                        }
                                    } else {
                                        // If not cached, it means it wasn't beneficial (or we chose not to cache it)
                                        // So we write raw bytes with NONE flag
                                        (COMPRESSION_FLAG_NONE, serial_buf.as_slice())
                                    };

                                    // Write: [flag:1][payload]
                                    file.write_all(&[flag])?;
                                    file.write_all(payload_ref)?;
                                }
                                ValueStorageMode::Tombstone => {}
                            }
                        }

                        // Write packed blocks
                        for (block_offset, mut values) in pack_blocks {
                            // Sort by index to ensure correct order
                            values.sort_by_key(|(idx, _)| *idx);

                            // Convert to slice references
                            let value_refs: Vec<(u16, &[u8])> =
                                values.iter().map(|(idx, bytes)| (*idx, bytes.as_slice())).collect();

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

        // Root offset is what Pass 2 assigned to 'self'
        let root_ptr = std::ptr::from_ref(self);
        let root_offset = node_offset_map[&root_ptr];
        Ok(root_offset)
    }

    /// Serialize an internal node with pre-calculated child offsets
    /// Supports multi-block internal nodes when keys + pointers exceed a single page
    fn serialize_internal_with_offsets<W: Write + Seek>(
        &self,
        file: &mut W,
        buffer: &mut Vec<u8>,
        serial_buf: &mut Vec<u8>,
        offset: u64,
        child_offsets: &[u64],
    ) -> io::Result<u64> {
        // Similar to serialize_to_block but for internal nodes with known child offsets
        serial_buf.clear();
        binary_serialize_into(&mut *serial_buf, &self.keys)?;
        let keys_len = serial_buf.len();
        let keys_end = keys_len;

        binary_serialize_into(&mut *serial_buf, child_offsets)?;
        let pointer_len = serial_buf.len() - keys_end;

        // Calculate total content size
        let total_content_size = FLAG_SIZE + LEN_SIZE + keys_len + LEN_SIZE + pointer_len;
        let blocks_needed = total_content_size.div_ceil(PAGE_SIZE_USIZE);

        let total_buffer_size = blocks_needed * PAGE_SIZE_USIZE;
        if buffer.len() < total_buffer_size {
            buffer.resize(total_buffer_size, 0);
        }
        buffer[..total_buffer_size].fill(0);

        let mut write_pos = 0;

        // Write is_leaf flag (0 for internal node)
        buffer[write_pos] = u8::from(self.is_leaf);
        write_pos += FLAG_SIZE;

        // Write keys length and data
        buffer[write_pos..write_pos + LEN_SIZE]
            .copy_from_slice(&u32::try_from(keys_len).map_err(to_io_error)?.to_le_bytes());
        write_pos += LEN_SIZE;
        buffer[write_pos..write_pos + keys_len].copy_from_slice(&serial_buf[0..keys_end]);
        write_pos += keys_len;

        // Write pointers length and data
        buffer[write_pos..write_pos + LEN_SIZE]
            .copy_from_slice(&u32::try_from(pointer_len).map_err(to_io_error)?.to_le_bytes());
        write_pos += LEN_SIZE;
        buffer[write_pos..write_pos + pointer_len].copy_from_slice(&serial_buf[keys_end..]);

        // Write all blocks to file
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(&buffer[..total_buffer_size])?;

        Ok(offset + total_buffer_size as u64)
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
        let mut keys: Vec<K> = binary_deserialize(&buffer[read_pos..read_pos + keys_len])?;
        read_pos += keys_len;

        let payload_len = u32_from_bytes(&buffer[read_pos..read_pos + LEN_SIZE])? as usize;
        read_pos += LEN_SIZE;

        let total_required = min_required + payload_len;
        if buffer.len() < total_required {
            buffer.resize(total_required, 0);
        }

        file.read_exact(&mut buffer[min_required..total_required])?;

        let (value_info, values, children, children_pointer) = if is_leaf {
            let mut info: Vec<ValueInfo> = binary_deserialize(&buffer[read_pos..read_pos + payload_len])?;
            let vals = if nested {
                let mut filtered_keys: Vec<K> = Vec::with_capacity(keys.len());
                let mut filtered_info: Vec<ValueInfo> = Vec::with_capacity(info.len());
                let mut v = Vec::with_capacity(info.len());

                let original_keys = std::mem::take(&mut keys);
                for (entry_key, entry_info) in original_keys.into_iter().zip(info.into_iter()) {
                    if entry_info.is_tombstone() {
                        continue;
                    }
                    v.push(Self::load_value_from_info(file, &entry_info)?);
                    filtered_keys.push(entry_key);
                    filtered_info.push(entry_info);
                }

                keys = filtered_keys;
                info = filtered_info;
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
        let header_end = start
            .checked_add(FLAG_SIZE + LEN_SIZE)
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
        Self::deserialize_from_block_slice(slice, Some(mmap), file, nested)
    }

    fn deserialize_from_block_slice<R: Read + Seek>(
        slice: &[u8],
        mmap: Option<&[u8]>,
        file: &mut R,
        nested: bool,
    ) -> io::Result<(Self, Option<Vec<u64>>)> {
        // Node type
        let is_leaf = slice[0] == 1u8;
        let mut read_pos = FLAG_SIZE;

        // ---- Keys ----
        let keys_length = u32_from_bytes(&slice[read_pos..read_pos + LEN_SIZE])? as usize;
        read_pos += LEN_SIZE;
        let mut keys: Vec<K> = binary_deserialize(&slice[read_pos..read_pos + keys_length])?;
        read_pos += keys_length;

        // ---- Value info (offset, length) for leaf nodes ----
        let (value_info, values): (Vec<ValueInfo>, Vec<V>) = if is_leaf {
            // Read value_info
            let info_length = u32_from_bytes(&slice[read_pos..read_pos + LEN_SIZE])? as usize;
            read_pos += LEN_SIZE;
            let mut info: Vec<ValueInfo> = binary_deserialize(&slice[read_pos..read_pos + info_length])?;

            // Values are loaded on-demand when nested=true
            if nested {
                let mut vals = Vec::with_capacity(info.len());
                let mut filtered_keys: Vec<K> = Vec::with_capacity(keys.len());
                let mut filtered_info: Vec<ValueInfo> = Vec::with_capacity(info.len());
                let mut last_packed_block: Option<(u64, Vec<u8>)> = None;
                let original_keys = std::mem::take(&mut keys);
                for (entry_key, entry_info) in original_keys.into_iter().zip(info.into_iter()) {
                    if entry_info.is_tombstone() {
                        continue;
                    }
                    match entry_info.mode {
                        ValueStorageMode::Packed(block_offset, index) => {
                            // Packed loading optimization: reuse block if it's the same
                            if let Some((offset, ref block)) = last_packed_block {
                                if offset == block_offset {
                                    vals.push(Self::extract_value_from_packed_block(block, index, &entry_info.cache)?);
                                    filtered_keys.push(entry_key);
                                    filtered_info.push(entry_info);
                                    continue;
                                }
                            }

                            // Load new block
                            let mut block = vec![0u8; PAGE_SIZE_USIZE];
                            file.seek(SeekFrom::Start(block_offset))?;
                            file.read_exact(&mut block)?;
                            vals.push(Self::extract_value_from_packed_block(&block, index, &entry_info.cache)?);
                            last_packed_block = Some((block_offset, block));
                            filtered_keys.push(entry_key);
                            filtered_info.push(entry_info);
                        }
                        ValueStorageMode::Single(_) => {
                            last_packed_block = None;
                            vals.push(Self::load_value_from_info(file, &entry_info)?);
                            filtered_keys.push(entry_key);
                            filtered_info.push(entry_info);
                        }
                        ValueStorageMode::Tombstone => {}
                    }
                }
                keys = filtered_keys;
                info = filtered_info;
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
                    let (child, _) = if let Some(m) = mmap {
                        Self::deserialize_from_mmap(m, file, ptr, nested)?
                    } else {
                        Self::deserialize_from_block(file, &mut child_buffer, ptr, nested)?
                    };
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
        // Fast path: Check cache for Single mode
        if let ValueStorageMode::Single(_) = info.mode {
            let cache_guard = info.cache.lock();
            if let Some(CacheData::Compressed(flag, payload)) = cache_guard.as_ref() {
                if *flag == COMPRESSION_FLAG_LZ4 {
                    let decompressed = lz4_flex::decompress_size_prepended(payload).map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidData, format!("LZ4 cache decompression failed: {e}"))
                    })?;
                    return binary_deserialize(&decompressed);
                }
                return binary_deserialize(payload);
            }
        }

        match info.mode {
            ValueStorageMode::Single(offset) => {
                let stored_len = info.length as usize;
                if stored_len < 1 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid value length"));
                }

                // Read everything: flag + payload
                file.seek(SeekFrom::Start(offset))?;
                let mut buffer = vec![0u8; stored_len];
                file.read_exact(&mut buffer)?;

                let flag = buffer[0];
                // Split payload without re-allocating if possible? Vec::split_off allocates new vec for tail.
                // We want payload as Vec for cache.
                let payload = buffer[1..].to_vec();

                // Decompress for result
                let data = if flag == COMPRESSION_FLAG_LZ4 {
                    lz4_flex::decompress_size_prepended(&payload).map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidData, format!("LZ4 decompression failed: {e}"))
                    })?
                } else {
                    payload.clone()
                };

                // Update cache
                *info.cache.lock() = Some(CacheData::Compressed(flag, payload));

                binary_deserialize(&data)
            }
            ValueStorageMode::Packed(block_offset, index) => {
                Self::load_value_from_packed_block(file, block_offset, index, info.length, &info.cache)
            }
            ValueStorageMode::Tombstone => {
                Err(io::Error::new(io::ErrorKind::NotFound, "value was deleted (tombstone)"))
            }
        }
    }

    /// Load a value from a packed block
    fn load_value_from_packed_block<R: Read + Seek>(
        file: &mut R,
        block_offset: u64,
        value_index: u16,
        _expected_length: u32,
        cache: &Mutex<Option<CacheData>>,
    ) -> io::Result<V> {
        file.seek(SeekFrom::Start(block_offset))?;

        let mut block_buffer = vec![0u8; PAGE_SIZE_USIZE];
        file.read_exact(&mut block_buffer)?;

        Self::extract_value_from_packed_block(&block_buffer, value_index, cache)
    }

    /// Helper to extract value from a packed block that is already in memory
    fn extract_value_from_packed_block(
        block_buffer: &[u8],
        value_index: u16,
        cache: &Mutex<Option<CacheData>>,
    ) -> io::Result<V> {
        // Read count
        if block_buffer.len() < 4 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Packed block too small"));
        }
        let mut pos = 4;

        // Skip to target value
        for i in 0..=value_index {
            if pos + 4 > block_buffer.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Packed block corrupted: position {pos} exceeds block size"),
                ));
            }

            let len = u32::from_le_bytes(block_buffer[pos..pos + 4].try_into().map_err(to_io_error)?) as usize;
            pos += 4;

            if i == value_index {
                // Found target value
                if pos + len > block_buffer.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("Packed value corrupted: length {len} at position {pos} exceeds block size"),
                    ));
                }

                // Cache the position in the block for future in-place updates
                *cache.lock() = Some(CacheData::PackedOffset(u16::try_from(pos).map_err(to_io_error)?));

                let value_data = &block_buffer[pos..pos + len];
                return binary_deserialize(value_data);
            }

            pos += len;
        }

        Err(io::Error::new(io::ErrorKind::InvalidData, format!("Value index {value_index} not found in packed block")))
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
    //
    // Conservative calculation to prevent internal node overflow:
    // - Account for MessagePack overhead per key (binary type headers)
    // - Account for array serialization overhead
    // - Apply safety factor to use only 75% of theoretical capacity

    let base_overhead = FLAG_SIZE + LEN_SIZE + LEN_SIZE + MSGPACK_ARRAY_OVERHEAD + 32; // flag + keys_len + info_len + array overhead + safety buffer
    let key_size = size_of::<K>();

    // Per-entry overhead: key size (with msgpack overhead) + pointer/info size + msgpack overhead
    let inner_entry_size = key_size + POINTER_SIZE + MSGPACK_OVERHEAD_PER_ENTRY;
    let leaf_entry_size = key_size + INFO_SIZE + MSGPACK_OVERHEAD_PER_ENTRY;

    // Calculate raw order then apply safety factor
    let raw_inner_order = (PAGE_SIZE_USIZE - base_overhead) / inner_entry_size;
    let raw_leaf_order = (PAGE_SIZE_USIZE - base_overhead) / leaf_entry_size;

    // Apply safety factor (use only 75% of capacity to prevent overflow)
    let inner_order = (raw_inner_order * ORDER_SAFETY_FACTOR) / 100;
    let leaf_order = (raw_leaf_order * ORDER_SAFETY_FACTOR) / 100;

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
    fn default() -> Self { Self::new() }
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
    pub fn get_metadata(&self) -> &BPlusTreeMetadata { &self.metadata }

    /// Helper to set metadata
    pub fn set_metadata(&mut self, data: BPlusTreeMetadata) {
        self.metadata = data;
        self.dirty = true;
    }

    pub fn is_empty(&self) -> bool { self.root.keys.is_empty() }

    pub fn len(&self) -> usize { self.root.len() }

    pub fn insert(&mut self, key: K, value: V) {
        self.dirty = true;
        if self.root.keys.is_empty() {
            self.root.keys.push(key);
            self.root.values.push(value);
            return;
        }

        if let Some(node) = self.root.insert(key, value, self.inner_order, self.leaf_order) {
            let child_key_opt =
                if node.is_leaf { node.keys.first() } else { BPlusTreeNode::<K, V>::find_leaf_entry(&node) };

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

    pub fn query(&self, key: &K) -> Option<&V> { self.root.query(key) }

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
    pub fn store_with_index<SortKey, F>(&mut self, filepath: &Path, sort_key_extractor: F) -> io::Result<u64>
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
            entries_with_locations.into_iter().map(|(k, v, loc)| (sort_key_extractor(&v), k, loc)).collect();

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
        if meta_bytes.len() > METADATA_MAX_SIZE || METADATA_DATA_START_POS + meta_bytes.len() > PAGE_SIZE_USIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Metadata too large for header page"));
        }
        let metadata_len =
            u32::try_from(meta_bytes.len()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let metadata_len_with_flags = encode_metadata_len_with_flags(metadata_len, false);
        header[16..20].copy_from_slice(&metadata_len_with_flags.to_le_bytes());
        if !meta_bytes.is_empty() {
            header[METADATA_DATA_START_POS..METADATA_DATA_START_POS + meta_bytes.len()].copy_from_slice(&meta_bytes);
        }

        file.write_all(&header)?;

        // We need to ensure we pad to PAGE_SIZE before continuing
        file.seek(SeekFrom::Start(HEADER_SIZE))?;

        // Use breadth-first serialization for better disk locality
        match self.root.serialize_breadth_first(&mut file, &mut buffer, HEADER_SIZE) {
            Ok(root_offset) => {
                // Update root offset in header
                file.seek(SeekFrom::Start(ROOT_OFFSET_POS))?;
                file.write_all(&root_offset.to_le_bytes())?;

                file.flush()?;
                drop(file);
                if let Err(err) = utils::rename_or_copy(tempfile.path(), filepath, false) {
                    return Err(string_to_io_error(format!(
                        "Temp file rename/copy did not work {} {err}",
                        tempfile.path().to_string_lossy()
                    )));
                }
                self.dirty = false;
                Ok(root_offset)
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
        write_buffer: &mut Vec<u8>,
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
                let mut serial_buf = Vec::new();
                current_offset =
                    node.serialize_internal_with_offsets(file, write_buffer, &mut serial_buf, node_offset, &pointers)?;

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
        let file = File::open(filepath)?;
        let mmap = unsafe { Mmap::map(&file)? };

        if mmap.len() < PAGE_SIZE_USIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "File too small"));
        }

        // Verify Header
        let header = &mmap[0..PAGE_SIZE_USIZE];
        if &header[0..4] != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid magic number"));
        }
        let version = u32_from_bytes(&header[4..8])?;
        if version != STORAGE_VERSION {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Unsupported storage version: {version}")));
        }
        let root_offset = u64_from_bytes(&header[8..16])?;

        // Read metadata
        let metadata_len_raw = u32_from_bytes(&header[16..20])?;
        let (metadata_len, _) = decode_metadata_len_and_flags(metadata_len_raw);
        let metadata = if metadata_len > 0 {
            if (METADATA_DATA_START_POS + metadata_len as usize) > PAGE_SIZE_USIZE {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "Metadata length exceeds header page size"));
            }
            header[METADATA_DATA_START_POS..(METADATA_DATA_START_POS + metadata_len as usize)].to_vec()
        } else {
            Vec::new()
        };

        let mut cursor = io::Cursor::new(mmap.as_ref());
        // Start after header block, with nested=true to deserialize all nodes
        let (root, _) = BPlusTreeNode::<K, V>::deserialize_from_mmap(&mmap, &mut cursor, root_offset, true)?;

        let (inner_order, leaf_order) = calc_order::<K>();
        Ok(Self { root, inner_order, leaf_order, metadata: BPlusTreeMetadata::from_bytes(&metadata), dirty: false })
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
            let res = BPlusTreeNode::<K, V>::deserialize_from_block_slice(&data, None, file, false)
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
                Ok(idx) => match node.value_info.get(idx) {
                    Some(info) => {
                        if info.is_tombstone() {
                            return Ok(None);
                        }
                        let value = BPlusTreeNode::<K, V>::load_value_from_info(file, info)?;
                        Ok(Some(value))
                    }
                    None => Ok(None),
                },
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

fn query_tree_contains_live_key<K, V, R: Read + Seek>(
    file: &mut R,
    buffer: &mut Vec<u8>,
    cache: &mut IndexMap<u64, Vec<u8>>,
    key: &K,
    start_offset: u64,
    has_tombstones: bool,
) -> Result<bool, BPlusTreeError>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    let mut offset = start_offset;
    loop {
        let (node, pointers) = if let Some(data) = cache.shift_remove(&offset) {
            let res = BPlusTreeNode::<K, V>::deserialize_from_block_slice(&data, None, file, false)
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
            return Ok(match node.keys.binary_search(key) {
                Ok(idx) => {
                    if has_tombstones {
                        node.value_info.get(idx).is_some_and(|info| !info.is_tombstone())
                    } else {
                        node.value_info.get(idx).is_some()
                    }
                }
                Err(_) => false,
            });
        }

        let child_idx = get_entry_index_upper_bound::<K>(&node.keys, key);
        if let Some(child_offsets) = pointers {
            if let Some(child_offset) = child_offsets.get(child_idx) {
                offset = *child_offset;
            } else {
                return Ok(false);
            }
        } else {
            return Ok(false);
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
    #[inline]
    fn fallback_scan<K, V, R: Read + Seek>(
        file: &mut R,
        buffer: &mut Vec<u8>,
        key: &K,
        start_offset: u64,
    ) -> Result<Option<V>, BPlusTreeError>
    where
        K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
        V: Serialize + for<'de> Deserialize<'de> + Clone,
    {
        let mut stack = vec![start_offset];
        let mut last: Option<V> = None;

        while let Some(offset) = stack.pop() {
            let (node, pointers) = BPlusTreeNode::<K, V>::deserialize_from_block(file, buffer, offset, false)?;
            if node.is_leaf {
                for (leaf_key, info) in node.keys.iter().zip(node.value_info.iter()) {
                    if leaf_key <= key && !info.is_tombstone() {
                        last = Some(BPlusTreeNode::<K, V>::load_value_from_info(file, info)?);
                    }
                }
            } else if let Some(ptrs) = pointers {
                for ptr in ptrs.into_iter().rev() {
                    stack.push(ptr);
                }
            }
        }

        Ok(last)
    }

    let mut offset = start_offset;
    loop {
        // Try Cache First
        let (node, pointers) = if let Some(data) = cache.shift_remove(&offset) {
            // LRU
            let res = BPlusTreeNode::<K, V>::deserialize_from_block_slice(&data, None, file, false)
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
            let mut idx = get_entry_index_upper_bound::<K>(&node.keys, key);
            while idx > 0 {
                idx -= 1;
                let Some(info) = node.value_info.get(idx) else {
                    continue;
                };
                if info.is_tombstone() {
                    continue;
                }
                let value = BPlusTreeNode::<K, V>::load_value_from_info(file, info)?;
                return Ok(Some(value));
            }
            return fallback_scan(file, buffer, key, start_offset);
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
            let res = BPlusTreeNode::<K, V>::deserialize_from_block_slice(&data, None, file, false)
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
            count += node.value_info.iter().filter(|info| !info.is_tombstone()).count();
        } else if let Some(ptrs) = pointers {
            stack.extend(ptrs);
        }
    }
    Ok(count)
}

fn count_items_mmap<K, V>(mmap: &[u8], start_offset: u64) -> Result<usize, BPlusTreeError>
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
            count += node.value_info.iter().filter(|info| !info.is_tombstone()).count();
        } else if let Some(ptrs) = pointers {
            stack.extend(ptrs);
        }
    }
    Ok(count)
}

fn query_tree_mmap<K, V>(
    mmap: &[u8],
    cursor: &mut io::Cursor<&[u8]>,
    node_cache: &mut NodeCache<K, V>,
    key: &K,
    start_offset: u64,
) -> Result<Option<V>, BPlusTreeError>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    let mut offset = start_offset;
    loop {
        // Cache Logic
        if !node_cache.contains_key(&offset) {
            let (node, pointers) = BPlusTreeNode::<K, V>::deserialize_from_mmap(mmap, cursor, offset, false)?;
            if node_cache.len() >= CACHE_CAPACITY {
                node_cache.shift_remove_index(0);
            }
            node_cache.insert(offset, (node, pointers));
        }

        // Update LRU
        if let Some(idx) = node_cache.get_index_of(&offset) {
            let len = node_cache.len();
            if len > 1 {
                node_cache.move_index(idx, len - 1);
            }
        }

        let (node, pointers) = &node_cache[&offset];

        if node.is_leaf {
            return match node.keys.binary_search(key) {
                Ok(idx) => match node.value_info.get(idx) {
                    Some(info) => {
                        if info.is_tombstone() {
                            return Ok(None);
                        }
                        let value = BPlusTreeNode::<K, V>::load_value_from_info(cursor, info)?;
                        Ok(Some(value))
                    }
                    None => Ok(None),
                },
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

fn query_tree_mmap_contains_live_key<K, V>(
    mmap: &[u8],
    cursor: &mut io::Cursor<&[u8]>,
    node_cache: &mut NodeCache<K, V>,
    key: &K,
    start_offset: u64,
    has_tombstones: bool,
) -> Result<bool, BPlusTreeError>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    let mut offset = start_offset;
    loop {
        if !node_cache.contains_key(&offset) {
            let (node, pointers) = BPlusTreeNode::<K, V>::deserialize_from_mmap(mmap, cursor, offset, false)?;
            if node_cache.len() >= CACHE_CAPACITY {
                node_cache.shift_remove_index(0);
            }
            node_cache.insert(offset, (node, pointers));
        }

        if let Some(idx) = node_cache.get_index_of(&offset) {
            let len = node_cache.len();
            if len > 1 {
                node_cache.move_index(idx, len - 1);
            }
        }

        let (node, pointers) = &node_cache[&offset];

        if node.is_leaf {
            return Ok(match node.keys.binary_search(key) {
                Ok(idx) => {
                    if has_tombstones {
                        node.value_info.get(idx).is_some_and(|info| !info.is_tombstone())
                    } else {
                        node.value_info.get(idx).is_some()
                    }
                }
                Err(_) => false,
            });
        }

        let child_idx = get_entry_index_upper_bound::<K>(&node.keys, key);
        if let Some(child_offsets) = pointers {
            if let Some(child_offset) = child_offsets.get(child_idx) {
                offset = *child_offset;
            } else {
                return Ok(false);
            }
        } else {
            return Ok(false);
        }
    }
}

/// Zero-copy optimized query for mmap'd data.
///
/// This function uses zero-copy key scanning for internal nodes, avoiding
/// the need to deserialize the entire keys vector. It only falls back to
/// full deserialization for leaf nodes where we need to perform binary search
/// and access `value_info`.
///
/// Performance improvement: For trees with many internal nodes, this eliminates
/// N heap allocations per internal node (where N is the number of keys).
fn query_tree_mmap_zero_copy<K, V>(
    mmap: &[u8],
    cursor: &mut io::Cursor<&[u8]>,
    key: &K,
    start_offset: u64,
) -> Result<Option<V>, BPlusTreeError>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone + MsgPackScannable,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    let mut offset = start_offset;

    loop {
        let node_start =
            usize::try_from(offset).map_err(|e| BPlusTreeError::Corrupted(format!("Invalid offset: {e}")))?;

        if node_start >= mmap.len() {
            return Err(BPlusTreeError::Corrupted("Offset out of bounds".into()));
        }

        let node_bytes = &mmap[node_start..];

        // Try zero-copy scan first
        if let Some(scan_result) = scan_internal_node_zero_copy(node_bytes, key) {
            if scan_result.is_leaf {
                // Fall back to full deserialization for leaf nodes
                let (node, _) = BPlusTreeNode::<K, V>::deserialize_from_mmap(mmap, cursor, offset, false)?;

                return match node.keys.binary_search(key) {
                    Ok(idx) => match node.value_info.get(idx) {
                        Some(info) => {
                            if info.is_tombstone() {
                                return Ok(None);
                            }
                            let value = BPlusTreeNode::<K, V>::load_value_from_info(cursor, info)?;
                            Ok(Some(value))
                        }
                        None => Ok(None),
                    },
                    Err(_) => Ok(None),
                };
            }

            // Internal node: read the child pointer using zero-copy
            let pointers_bytes = &node_bytes[scan_result.pointers_start..];
            if let Some(child_offset) = read_pointer_at_index(pointers_bytes, scan_result.child_idx) {
                offset = child_offset;
                continue;
            }
            // Fall through to fallback if pointer read failed
        }

        // Fallback: full deserialization (handles edge cases, unsupported key types, etc.)
        let (node, pointers) = BPlusTreeNode::<K, V>::deserialize_from_mmap(mmap, cursor, offset, false)?;

        if node.is_leaf {
            return match node.keys.binary_search(key) {
                Ok(idx) => match node.value_info.get(idx) {
                    Some(info) => {
                        if info.is_tombstone() {
                            return Ok(None);
                        }
                        let value = BPlusTreeNode::<K, V>::load_value_from_info(cursor, info)?;
                        Ok(Some(value))
                    }
                    None => Ok(None),
                },
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
    #[inline]
    fn fallback_scan<K, V>(
        mmap: &[u8],
        cursor: &mut io::Cursor<&[u8]>,
        key: &K,
        start_offset: u64,
    ) -> Result<Option<V>, BPlusTreeError>
    where
        K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
        V: Serialize + for<'de> Deserialize<'de> + Clone,
    {
        let mut stack = vec![start_offset];
        let mut last: Option<V> = None;

        while let Some(offset) = stack.pop() {
            let (node, pointers) = BPlusTreeNode::<K, V>::deserialize_from_mmap(mmap, cursor, offset, false)?;
            if node.is_leaf {
                for (leaf_key, info) in node.keys.iter().zip(node.value_info.iter()) {
                    if leaf_key <= key && !info.is_tombstone() {
                        last = Some(BPlusTreeNode::<K, V>::load_value_from_info(cursor, info)?);
                    }
                }
            } else if let Some(ptrs) = pointers {
                for ptr in ptrs.into_iter().rev() {
                    stack.push(ptr);
                }
            }
        }

        Ok(last)
    }

    let mut offset = start_offset;
    loop {
        let (node, pointers) = BPlusTreeNode::<K, V>::deserialize_from_mmap(mmap, cursor, offset, false)?;

        if node.is_leaf {
            let mut idx = get_entry_index_upper_bound::<K>(&node.keys, key);
            while idx > 0 {
                idx -= 1;
                let Some(info) = node.value_info.get(idx) else {
                    continue;
                };
                if info.is_tombstone() {
                    continue;
                }
                let value = BPlusTreeNode::<K, V>::load_value_from_info(cursor, info)?;
                return Ok(Some(value));
            }
            return fallback_scan(mmap, cursor, key, start_offset);
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
type NodeCache<K, V> = IndexMap<u64, (BPlusTreeNode<K, V>, Option<Vec<u64>>)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    len: u64,
    modified_ns: u128,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

impl FileIdentity {
    fn from_metadata(metadata: &Metadata) -> Self {
        let modified_ns = metadata
            .modified()
            .or_else(|_| metadata.created())
            .ok()
            .and_then(|timestamp| timestamp.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_nanos());

        Self {
            len: metadata.len(),
            modified_ns,
            #[cfg(unix)]
            dev: metadata.dev(),
            #[cfg(unix)]
            ino: metadata.ino(),
        }
    }
}

pub struct BPlusTreeQuery<K, V> {
    file: Option<BufReader<File>>,
    mmap: Option<Mmap>,
    filepath: PathBuf,
    file_identity: Option<FileIdentity>,
    has_tombstones: bool,
    buffer: Vec<u8>,
    cache: IndexMap<u64, Vec<u8>>,
    node_cache: NodeCache<K, V>,
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
        let file_identity = Some(FileIdentity::from_metadata(&metadata));

        if file_len < HEADER_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "File too small"));
        }

        // Try Mmap
        let mmap = unsafe { Mmap::map(&file).ok() };

        // Verify Header
        let mut header = [0u8; METADATA_DATA_START_POS];
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
        let version = u32::from_le_bytes(
            header[4..8].try_into().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid version slice"))?,
        );
        if version != STORAGE_VERSION {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Unsupported storage version: {version}")));
        }
        let root_offset = u64::from_le_bytes(
            header[8..16]
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid root offset slice"))?,
        );
        let metadata_len_raw = u32::from_le_bytes(
            header[16..20]
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid metadata length slice"))?,
        );
        let (metadata_len, has_tombstones) = decode_metadata_len_and_flags(metadata_len_raw);
        if metadata_len as usize > METADATA_MAX_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Metadata too large: {metadata_len}")));
        }

        Ok(Self {
            file: if mmap.is_some() { None } else { Some(utils::file_reader(file)) },
            mmap,
            filepath: PathBuf::new(),
            file_identity,
            has_tombstones,
            buffer: vec![0u8; PAGE_SIZE_USIZE],
            cache: IndexMap::with_capacity(CACHE_CAPACITY),
            node_cache: IndexMap::with_capacity(CACHE_CAPACITY),
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

    /// Clone an existing query without re-reading the file header.
    /// This avoids synchronous disk initialization while still providing
    /// an independent reader.
    pub fn try_clone(&self) -> io::Result<Self> {
        let (file, mmap) = if self.mmap.is_some() {
            if self.filepath.as_os_str().is_empty() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "Missing filepath for mmap clone"));
            }
            let file = File::open(&self.filepath)?;
            if let Some(mapped) = unsafe { Mmap::map(&file).ok() } {
                (None, Some(mapped))
            } else {
                (Some(utils::file_reader(file)), None)
            }
        } else if let Some(file) = &self.file {
            let cloned = file.get_ref().try_clone()?;
            (Some(utils::file_reader(cloned)), None)
        } else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "No data source available to clone"));
        };

        Ok(Self {
            file,
            mmap,
            filepath: self.filepath.clone(),
            file_identity: self.file_identity,
            has_tombstones: self.has_tombstones,
            buffer: vec![0u8; PAGE_SIZE_USIZE],
            cache: IndexMap::with_capacity(CACHE_CAPACITY),
            node_cache: IndexMap::with_capacity(CACHE_CAPACITY),
            root_offset: self.root_offset,
            _marker_k: PhantomData,
            _marker_v: PhantomData,
        })
    }

    /// Returns the filepath this query was opened from.
    pub fn filepath(&self) -> &Path { &self.filepath }

    fn read_root_offset_and_tombstone_flag_from_file(file: &File) -> io::Result<(u64, bool)> {
        let mut root_and_metadata_len = [0u8; size_of::<u64>() + size_of::<u32>()];
        #[cfg(unix)]
        file.read_exact_at(&mut root_and_metadata_len, ROOT_OFFSET_POS)?;
        #[cfg(not(unix))]
        {
            let mut f = file;
            let current_pos = f.stream_position()?;
            f.seek(SeekFrom::Start(ROOT_OFFSET_POS))?;
            f.read_exact(&mut root_and_metadata_len)?;
            f.seek(SeekFrom::Start(current_pos))?;
        }
        let root_offset = u64::from_le_bytes(
            root_and_metadata_len[0..8]
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid root offset bytes"))?,
        );
        let metadata_len_raw = u32::from_le_bytes(
            root_and_metadata_len[8..12]
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid metadata length bytes"))?,
        );
        let (metadata_len, has_tombstones) = decode_metadata_len_and_flags(metadata_len_raw);
        if metadata_len as usize > METADATA_MAX_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Metadata too large: {metadata_len}")));
        }

        Ok((root_offset, has_tombstones))
    }

    fn refresh_root_offset(&mut self) -> io::Result<()> {
        let (new_root_offset, new_has_tombstones) = if self.mmap.is_some() && !self.filepath.as_os_str().is_empty() {
            let file = File::open(&self.filepath)?;
            let metadata = file.metadata()?;
            let current_identity = FileIdentity::from_metadata(&metadata);
            let remap_required = self.file_identity != Some(current_identity);
            let (root_offset, has_tombstones) = Self::read_root_offset_and_tombstone_flag_from_file(&file)?;

            if remap_required {
                if let Some(remapped) = unsafe { Mmap::map(&file).ok() } {
                    self.file = None;
                    self.mmap = Some(remapped);
                } else {
                    self.mmap = None;
                    self.file = Some(utils::file_reader(file));
                }
                self.cache.clear();
                self.node_cache.clear();
            }

            self.file_identity = Some(current_identity);
            (root_offset, has_tombstones)
        } else if let Some(mmap) = &self.mmap {
            let start = usize::try_from(ROOT_OFFSET_POS).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("Invalid root offset position: {e}"))
            })?;
            let root_end = start
                .checked_add(size_of::<u64>())
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Invalid root offset range"))?;
            let root_bytes = mmap
                .get(start..root_end)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Header too small"))?;
            let root_offset_bytes: [u8; size_of::<u64>()] = root_bytes
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid root offset bytes"))?;
            let metadata_len_start = usize::try_from(METADATA_OFFSET_POS)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("Invalid metadata position: {e}")))?;
            let metadata_len_end = metadata_len_start
                .checked_add(size_of::<u32>())
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Invalid metadata length range"))?;
            let metadata_len_bytes = mmap
                .get(metadata_len_start..metadata_len_end)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Header too small for metadata length"))?;
            let metadata_len_raw = u32::from_le_bytes(
                metadata_len_bytes
                    .try_into()
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid metadata length bytes"))?,
            );
            let (metadata_len, has_tombstones) = decode_metadata_len_and_flags(metadata_len_raw);
            if metadata_len as usize > METADATA_MAX_SIZE {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Metadata too large: {metadata_len}")));
            }
            (u64::from_le_bytes(root_offset_bytes), has_tombstones)
        } else if let Some(file) = &mut self.file {
            Self::read_root_offset_and_tombstone_flag_from_file(file.get_ref())?
        } else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "No data source available"));
        };

        if new_root_offset != self.root_offset || new_has_tombstones != self.has_tombstones {
            self.root_offset = new_root_offset;
            self.has_tombstones = new_has_tombstones;
            self.cache.clear();
            self.node_cache.clear();
        }

        Ok(())
    }

    pub fn query(&mut self, key: &K) -> Result<Option<V>, BPlusTreeError> {
        self.refresh_root_offset().map_err(BPlusTreeError::Io)?;
        if let Some(mmap) = &self.mmap {
            let mut cursor = io::Cursor::new(mmap.as_ref());
            query_tree_mmap(mmap, &mut cursor, &mut self.node_cache, key, self.root_offset)
        } else if let Some(file) = &mut self.file {
            query_tree(file, &mut self.buffer, &mut self.cache, key, self.root_offset)
        } else {
            Err(BPlusTreeError::InvalidStructure("No data source available".into()))
        }
    }

    pub const fn has_tombstones(&self) -> bool { self.has_tombstones }

    pub fn contains_live_key(&mut self, key: &K) -> Result<bool, BPlusTreeError> {
        self.refresh_root_offset().map_err(BPlusTreeError::Io)?;
        if let Some(mmap) = &self.mmap {
            let mut cursor = io::Cursor::new(mmap.as_ref());
            query_tree_mmap_contains_live_key(
                mmap,
                &mut cursor,
                &mut self.node_cache,
                key,
                self.root_offset,
                self.has_tombstones,
            )
        } else if let Some(file) = &mut self.file {
            query_tree_contains_live_key::<K, V, _>(
                file,
                &mut self.buffer,
                &mut self.cache,
                key,
                self.root_offset,
                self.has_tombstones,
            )
        } else {
            Err(BPlusTreeError::InvalidStructure("No data source available".into()))
        }
    }
}

/// Additional methods for key types that support zero-copy scanning.
///
/// These methods provide optimized query performance by avoiding heap allocations
/// when traversing internal nodes. For trees with many levels, this can significantly
/// reduce memory pressure and improve query latency.
impl<K, V> BPlusTreeQuery<K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone + MsgPackScannable,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    /// Zero-copy optimized query that avoids deserializing internal node keys.
    ///
    /// This method scans through MessagePack-encoded keys directly, comparing
    /// without allocating. It's significantly faster for trees with many internal
    /// nodes, especially when keys are `u32` or `String`.
    ///
    /// # Example
    /// ```ignore
    /// let mut query = BPlusTreeQuery::<u32, MyValue>::try_new(&path)?;
    /// let value = query.query_zero_copy(&42)?;
    /// ```
    pub fn query_zero_copy(&mut self, key: &K) -> Result<Option<V>, BPlusTreeError> {
        self.refresh_root_offset().map_err(BPlusTreeError::Io)?;
        if let Some(mmap) = &self.mmap {
            let mut cursor = io::Cursor::new(mmap.as_ref());
            query_tree_mmap_zero_copy(mmap, &mut cursor, key, self.root_offset)
        } else if let Some(file) = &mut self.file {
            // For file-based queries, fall back to regular query
            // (zero-copy would require reading node data into a buffer first)
            query_tree(file, &mut self.buffer, &mut self.cache, key, self.root_offset)
        } else {
            Err(BPlusTreeError::InvalidStructure("No data source available".into()))
        }
    }
}

impl<K, V> BPlusTreeQuery<K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub fn query_le(&mut self, key: &K) -> Result<Option<V>, BPlusTreeError> {
        self.refresh_root_offset().map_err(BPlusTreeError::Io)?;
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
        self.refresh_root_offset().map_err(BPlusTreeError::Io)?;
        if let Some(mmap) = &self.mmap {
            count_items_mmap::<K, V>(mmap, self.root_offset)
        } else if let Some(file) = &mut self.file {
            count_items::<K, V, _>(file, &mut self.buffer, &mut self.cache, self.root_offset)
        } else {
            Err(BPlusTreeError::InvalidStructure("No data source available".into()))
        }
    }

    pub fn is_empty(&mut self) -> Result<bool, BPlusTreeError> {
        self.refresh_root_offset().map_err(BPlusTreeError::Io)?;
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
    pub fn iter(&mut self) -> BPlusTreeDiskIterator<'_, K, V> { BPlusTreeDiskIterator::new(self) }

    pub(crate) fn into_sorted_parts(self) -> (PathBuf, Option<BufReader<File>>, Option<Mmap>) {
        (self.filepath, self.file, self.mmap)
    }

    /// Owned iterator that traverses the tree in order defined by a secondary sorted index.
    ///
    /// The index path is automatically derived from the tree filepath by changing
    /// the extension to `.idx`. For example, if the tree is at `/data/items.bin`,
    /// the index is expected at `/data/items.idx`.
    ///
    /// This iterator reads values directly from stored offsets in O(1) time,
    /// avoiding O(log n) tree traversal per item.
    pub fn disk_iter_sorted<SortKey>(
        self,
    ) -> io::Result<super::sorted_index::BPlusTreeSortedIteratorOwned<K, V, SortKey>>
    where
        SortKey: for<'de> Deserialize<'de>,
    {
        super::sorted_index::BPlusTreeSortedIteratorOwned::<K, V, SortKey>::new_hybrid(
            self.filepath.clone(),
            self.file,
            self.mmap,
        )
    }

    /// Owned iterator with explicit index path.
    pub fn disk_iter_sorted_with_path<SortKey>(
        self,
        index_path: &Path,
    ) -> io::Result<super::sorted_index::BPlusTreeSortedIteratorOwned<K, V, SortKey>>
    where
        SortKey: for<'de> Deserialize<'de>,
    {
        super::sorted_index::BPlusTreeSortedIteratorOwned::<K, V, SortKey>::with_index_path_hybrid(
            self.filepath.clone(),
            self.file,
            self.mmap,
            index_path,
        )
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
    #[allow(clippy::too_many_lines)]
    pub fn collect_with_locations(&mut self) -> io::Result<Vec<(K, V, super::sorted_index::ValueLocation)>> {
        let mut result = Vec::new();
        let mut stack = vec![self.root_offset];
        let has_tombstones = self.has_tombstones;

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
                    if has_tombstones {
                        for (key, info) in node.keys.into_iter().zip(node.value_info.iter()) {
                            if info.is_tombstone() {
                                continue;
                            }
                            let value = BPlusTreeNode::<K, V>::load_value_from_info(&mut cursor, info)?;
                            let location = match info.mode {
                                ValueStorageMode::Single(offset) => {
                                    super::sorted_index::ValueLocation::Single { offset, length: info.length }
                                }
                                ValueStorageMode::Packed(block_offset, index) => {
                                    super::sorted_index::ValueLocation::Packed {
                                        block_offset,
                                        index,
                                        length: info.length,
                                    }
                                }
                                ValueStorageMode::Tombstone => continue,
                            };
                            result.push((key, value, location));
                        }
                    } else {
                        for (key, info) in node.keys.into_iter().zip(node.value_info.iter()) {
                            let value = BPlusTreeNode::<K, V>::load_value_from_info(&mut cursor, info)?;
                            let location = match info.mode {
                                ValueStorageMode::Single(offset) => {
                                    super::sorted_index::ValueLocation::Single { offset, length: info.length }
                                }
                                ValueStorageMode::Packed(block_offset, index) => {
                                    super::sorted_index::ValueLocation::Packed {
                                        block_offset,
                                        index,
                                        length: info.length,
                                    }
                                }
                                ValueStorageMode::Tombstone => continue,
                            };
                            result.push((key, value, location));
                        }
                    }
                } else if let Some(file) = &mut self.file {
                    if has_tombstones {
                        for (key, info) in node.keys.into_iter().zip(node.value_info.iter()) {
                            if info.is_tombstone() {
                                continue;
                            }
                            let value = BPlusTreeNode::<K, V>::load_value_from_info(file, info)?;
                            let location = match info.mode {
                                ValueStorageMode::Single(offset) => {
                                    super::sorted_index::ValueLocation::Single { offset, length: info.length }
                                }
                                ValueStorageMode::Packed(block_offset, index) => {
                                    super::sorted_index::ValueLocation::Packed {
                                        block_offset,
                                        index,
                                        length: info.length,
                                    }
                                }
                                ValueStorageMode::Tombstone => continue,
                            };
                            result.push((key, value, location));
                        }
                    } else {
                        for (key, info) in node.keys.into_iter().zip(node.value_info.iter()) {
                            let value = BPlusTreeNode::<K, V>::load_value_from_info(file, info)?;
                            let location = match info.mode {
                                ValueStorageMode::Single(offset) => {
                                    super::sorted_index::ValueLocation::Single { offset, length: info.length }
                                }
                                ValueStorageMode::Packed(block_offset, index) => {
                                    super::sorted_index::ValueLocation::Packed {
                                        block_offset,
                                        index,
                                        length: info.length,
                                    }
                                }
                                ValueStorageMode::Tombstone => continue,
                            };
                            result.push((key, value, location));
                        }
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
    pub fn disk_iter(self) -> BPlusTreeDiskIteratorOwned<K, V> { BPlusTreeDiskIteratorOwned::new(self) }
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
        Self { query, stack: vec![(root_offset, 0)], leaf_keys: Vec::new(), leaf_values: Vec::new(), leaf_idx: 0 }
    }

    pub fn is_empty(&self) -> bool { self.stack.is_empty() && self.leaf_idx >= self.leaf_keys.len() }

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
                let mut keys = Vec::with_capacity(node.keys.len());
                let mut vals = Vec::with_capacity(node.value_info.len());
                let has_tombstones = self.query.has_tombstones;
                if let Some(mmap) = &self.query.mmap {
                    let mut cursor = io::Cursor::new(mmap.as_ref());
                    if has_tombstones {
                        for (key, value_info) in node.keys.iter().zip(node.value_info.iter()) {
                            if value_info.is_tombstone() {
                                continue;
                            }
                            let v = BPlusTreeNode::<K, V>::load_value_from_info(&mut cursor, value_info)?;
                            keys.push(key.clone());
                            vals.push(v);
                        }
                    } else {
                        for (key, value_info) in node.keys.iter().zip(node.value_info.iter()) {
                            let v = BPlusTreeNode::<K, V>::load_value_from_info(&mut cursor, value_info)?;
                            keys.push(key.clone());
                            vals.push(v);
                        }
                    }
                } else if let Some(file) = &mut self.query.file {
                    if has_tombstones {
                        for (key, value_info) in node.keys.iter().zip(node.value_info.iter()) {
                            if value_info.is_tombstone() {
                                continue;
                            }
                            let v = BPlusTreeNode::<K, V>::load_value_from_info(file, value_info)?;
                            keys.push(key.clone());
                            vals.push(v);
                        }
                    } else {
                        for (key, value_info) in node.keys.iter().zip(node.value_info.iter()) {
                            let v = BPlusTreeNode::<K, V>::load_value_from_info(file, value_info)?;
                            keys.push(key.clone());
                            vals.push(v);
                        }
                    }
                }
                return Ok(Some((keys, vals)));
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
pub enum PlaylistIteratorReader<K, V, SortKey> {
    Sorted(super::sorted_index::BPlusTreeSortedIteratorOwned<K, V, SortKey>),
    Unsorted(BPlusTreeDiskIteratorOwned<K, V>),
}

impl<K, V, SortKey> Iterator for PlaylistIteratorReader<K, V, SortKey>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
    SortKey: for<'de> Deserialize<'de>,
{
    type Item = io::Result<(K, V)>;

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
        Self { query, stack: vec![(root_offset, 0)], leaf_keys: Vec::new(), leaf_values: Vec::new(), leaf_idx: 0 }
    }

    pub fn is_empty(&self) -> bool { self.stack.is_empty() && self.leaf_idx >= self.leaf_keys.len() }

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
                let mut keys = Vec::with_capacity(node.keys.len());
                let mut vals = Vec::with_capacity(node.value_info.len());
                let has_tombstones = self.query.has_tombstones;
                if let Some(mmap) = &self.query.mmap {
                    let mut cursor = io::Cursor::new(mmap.as_ref());
                    if has_tombstones {
                        for (key, value_info) in node.keys.iter().zip(node.value_info.iter()) {
                            if value_info.is_tombstone() {
                                continue;
                            }
                            let v = BPlusTreeNode::<K, V>::load_value_from_info(&mut cursor, value_info)?;
                            keys.push(key.clone());
                            vals.push(v);
                        }
                    } else {
                        for (key, value_info) in node.keys.iter().zip(node.value_info.iter()) {
                            let v = BPlusTreeNode::<K, V>::load_value_from_info(&mut cursor, value_info)?;
                            keys.push(key.clone());
                            vals.push(v);
                        }
                    }
                } else if let Some(file) = &mut self.query.file {
                    if has_tombstones {
                        for (key, value_info) in node.keys.iter().zip(node.value_info.iter()) {
                            if value_info.is_tombstone() {
                                continue;
                            }
                            let v = BPlusTreeNode::<K, V>::load_value_from_info(file, value_info)?;
                            keys.push(key.clone());
                            vals.push(v);
                        }
                    } else {
                        for (key, value_info) in node.keys.iter().zip(node.value_info.iter()) {
                            let v = BPlusTreeNode::<K, V>::load_value_from_info(file, value_info)?;
                            keys.push(key.clone());
                            vals.push(v);
                        }
                    }
                }
                return Ok(Some((keys, vals)));
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushPolicy {
    /// Flush and `sync_all` after each write operation (safest, slowest).
    Immediate,
    /// Flush after writes; call `commit()` (or a background commit loop) to `sync_all`.
    Batch,
    /// Flush only. Never call `sync_all` automatically.
    None,
}

#[derive(Debug, Clone, Copy)]
struct BatchRollbackState {
    file_len: u64,
    root_offset: u64,
}

pub struct BPlusTreeUpdate<K, V> {
    file: BufReader<File>,
    read_buffer: Vec<u8>,
    write_buffer: Vec<u8>,
    serial_buffer: Vec<u8>, // Reusable buffer for serialization
    cache: IndexMap<u64, Vec<u8>>,
    root_offset: u64,
    has_tombstones: bool,
    inner_order: usize,
    leaf_order: usize,
    flush_policy: FlushPolicy,
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
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("File not found {}", filepath.to_str().unwrap_or("?")),
            ));
        }
        // Acquire lock first
        let lock = FileLock::try_lock(filepath)?;

        let f = utils::open_read_write_file(filepath)?;

        // Verify Header
        let mut header = [0u8; METADATA_DATA_START_POS];
        #[cfg(unix)]
        f.read_exact_at(&mut header, 0)?;
        #[cfg(not(unix))]
        {
            let mut tmp_f = &f;
            tmp_f.seek(SeekFrom::Start(0))?;
            tmp_f.read_exact(&mut header)?;
        }

        if &header[0..4] != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid magic number"));
        }
        let version = u32::from_le_bytes(
            header[4..8].try_into().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid version slice"))?,
        );
        if version != STORAGE_VERSION {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Unsupported storage version: {version}")));
        }
        let root_offset = u64::from_le_bytes(
            header[8..16]
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid root offset slice"))?,
        );
        let metadata_len_raw = u32::from_le_bytes(
            header[16..20]
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid metadata length slice"))?,
        );
        let (metadata_len, has_tombstones) = decode_metadata_len_and_flags(metadata_len_raw);
        if metadata_len as usize > METADATA_MAX_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Metadata too large: {metadata_len}")));
        }
        let (inner_order, leaf_order) = calc_order::<K>();

        let file = utils::file_reader(f);

        Ok(Self {
            file,
            read_buffer: vec![0u8; PAGE_SIZE_USIZE],
            write_buffer: vec![0u8; PAGE_SIZE_USIZE],
            serial_buffer: Vec::with_capacity(PAGE_SIZE_USIZE),
            cache: IndexMap::with_capacity(CACHE_CAPACITY),
            root_offset,
            has_tombstones,
            inner_order,
            leaf_order,
            flush_policy: FlushPolicy::Immediate,
            lock,
            _marker_k: PhantomData,
            _marker_v: PhantomData,
        })
    }

    /// Opens an update handle with lock-acquisition backoff.
    ///
    /// This function is **blocking** and should not run directly on a Tokio worker
    /// thread. Internally it delegates to [`Self::try_new_with_backoff_stats`],
    /// which uses `std::thread::sleep` while waiting for the file lock.
    ///
    /// In async contexts (Axum/Tokio), call this inside
    /// `tokio::task::spawn_blocking(...)`.
    pub fn try_new_with_backoff(filepath: &Path) -> io::Result<Self> {
        Self::try_new_with_backoff_stats(filepath).map(|(tree, _)| tree)
    }

    /// Opens an update handle with lock-acquisition backoff and returns
    /// `(tree, retry_attempts)`.
    ///
    /// This function is **blocking**. It performs lock retries with
    /// `std::thread::sleep`, so running it on a Tokio worker thread can stall
    /// other async tasks scheduled on that thread.
    ///
    /// Use `tokio::task::spawn_blocking(...)` when calling from async code.
    pub fn try_new_with_backoff_stats(filepath: &Path) -> io::Result<(Self, u64)> {
        let mut attempts = 0u64;
        let mut backoff = Duration::from_micros(10);
        let max_backoff = Duration::from_millis(10);
        let started_at = Instant::now();
        let timeout = Duration::from_secs(30);

        loop {
            match Self::try_new(filepath) {
                Ok(tree) => return Ok((tree, attempts)),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    if started_at.elapsed() >= timeout {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "timeout acquiring lock"));
                    }
                    attempts = attempts.saturating_add(1);
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(max_backoff);
                }
                Err(err) => return Err(err),
            }
        }
    }

    pub fn set_flush_policy(&mut self, policy: FlushPolicy) { self.flush_policy = policy; }

    #[inline]
    const fn should_sync_on_write(&self) -> bool { matches!(self.flush_policy, FlushPolicy::Immediate) }

    #[inline]
    const fn should_sync_on_commit(&self) -> bool { !matches!(self.flush_policy, FlushPolicy::None) }

    /// Prepares a batch by cloning keys and serializing values eagerly.
    ///
    /// Keys are cloned because the returned prepared payload must outlive the
    /// borrowed input slice. For large batches or expensive key types, this
    /// can be a notable allocation/cloning cost.
    pub fn prepare_upsert_batch(items: &[(&K, &V)]) -> io::Result<Vec<(K, Vec<u8>)>> {
        let mut prepared = Vec::with_capacity(items.len());
        for (key, value) in items {
            prepared.push(((*key).clone(), binary_serialize(*value)?));
        }
        Ok(prepared)
    }

    pub fn upsert_batch_prepared_with_backoff(filepath: &Path, items: &[(&K, &V)]) -> io::Result<u64> {
        let prepared = Self::prepare_upsert_batch(items)?;
        let mut updater = Self::try_new_with_backoff(filepath)?;
        updater.upsert_batch_encoded(prepared)
    }

    fn capture_batch_rollback_state(&self) -> io::Result<BatchRollbackState> {
        Ok(BatchRollbackState { file_len: self.file.get_ref().metadata()?.len(), root_offset: self.root_offset })
    }

    fn rollback_batch_state(&mut self, state: BatchRollbackState) -> io::Result<()> {
        let should_sync = self.should_sync_on_commit();
        {
            let file = self.file.get_mut();
            file.seek(SeekFrom::Start(ROOT_OFFSET_POS))?;
            file.write_all(&state.root_offset.to_le_bytes())?;
            file.flush()?;
            file.set_len(state.file_len)?;
            if should_sync {
                file.sync_all()?;
            }
        }

        // Seek via BufReader to invalidate any buffered state.
        self.file.seek(SeekFrom::Start(state.file_len))?;
        self.cache.clear();
        self.root_offset = state.root_offset;
        Ok(())
    }

    fn read_metadata_len_from_header(&mut self) -> io::Result<u32> {
        self.file.seek(SeekFrom::Start(METADATA_OFFSET_POS))?;
        let mut len_buf = [0u8; 4];
        self.file.read_exact(&mut len_buf)?;
        let (metadata_len, _) = decode_metadata_len_and_flags(u32::from_le_bytes(len_buf));
        if metadata_len as usize > METADATA_MAX_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Metadata too large: {metadata_len}")));
        }
        Ok(metadata_len)
    }

    fn write_metadata_len_with_current_flags(&mut self, metadata_len: u32) -> io::Result<()> {
        if metadata_len as usize > METADATA_MAX_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("Metadata too large: {metadata_len}")));
        }

        let metadata_len_with_flags = encode_metadata_len_with_flags(metadata_len, self.has_tombstones);
        self.file.seek(SeekFrom::Start(METADATA_OFFSET_POS))?;
        self.file.get_mut().write_all(&metadata_len_with_flags.to_le_bytes())?;
        self.file.seek_relative(0)?;
        Ok(())
    }

    /// Helper to access metadata
    pub fn get_metadata(&mut self) -> io::Result<BPlusTreeMetadata> {
        let Ok(len) = self.read_metadata_len_from_header() else {
            return Ok(BPlusTreeMetadata::Empty);
        };
        if len == 0 {
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
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Metadata too large: {} > {}", bytes.len(), METADATA_MAX_SIZE),
            ));
        }

        self.file.seek(SeekFrom::Start(METADATA_OFFSET_POS))?;
        {
            let file = self.file.get_mut();
            let metadata_len =
                u32::try_from(bytes.len()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            let metadata_len_with_flags = encode_metadata_len_with_flags(metadata_len, self.has_tombstones);
            file.write_all(&metadata_len_with_flags.to_le_bytes())?;
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
        let (node, _) =
            BPlusTreeNode::<K, V>::deserialize_from_block(&mut reader, &mut self.read_buffer, self.root_offset, false)?;
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
        let new_root_offset = self.update_batch_recursive(self.root_offset, &refs, true)?;

        // Only update header if root offset actually changed
        // (if in-place update succeeded, offset stays the same)
        if new_root_offset != self.root_offset {
            // Atomic Header Swap
            self.file.seek(SeekFrom::Start(ROOT_OFFSET_POS))?;
            self.file.get_mut().write_all(&new_root_offset.to_le_bytes()).map_err(BPlusTreeError::Io)?;
            self.root_offset = new_root_offset;
        }

        self.file.get_mut().flush().map_err(BPlusTreeError::Io)?;
        if self.should_sync_on_write() {
            self.file.get_mut().sync_all().map_err(BPlusTreeError::Io)?;
        }

        Ok(new_root_offset)
    }

    /// Update multiple items in batch. This is more efficient than calling `update()` multiple times
    /// as it performs all updates and then commits the final root offset once.
    ///
    /// For rollback safety in `update_batch`, callers may disable in-place updates and force
    /// copy-on-write mode for all touched values.
    ///
    /// returns The final root offset after all updates, or an error if any update fails.
    /// Returns the original offset if all updates were performed in-place.
    fn update_batch_recursive(
        &mut self,
        offset: u64,
        items: &[(&K, &V)],
        allow_in_place: bool,
    ) -> Result<u64, BPlusTreeError> {
        let (mut node, pointers_opt) =
            BPlusTreeNode::<K, V>::deserialize_from_block(&mut self.file, &mut self.read_buffer, offset, false)?;

        if node.is_leaf {
            // Track which items need the traditional COW approach (value grew beyond allocated space)
            let mut needs_cow: Vec<(usize, &K, &V)> = Vec::new();
            // Track promoted packed values (need node rewrite but value already written)
            let mut promoted: Vec<(usize, ValueInfo)> = Vec::new();

            for (key, value) in items {
                match node.keys.binary_search(key) {
                    Ok(idx) => {
                        if allow_in_place {
                            // Try in-place update first
                            let result = self
                                .try_update_value_in_place(value, &node.value_info[idx])
                                .map_err(BPlusTreeError::Io)?;

                            match result {
                                InPlaceUpdateResult::Success => {
                                    // In-place succeeded, value_info stays the same
                                }
                                InPlaceUpdateResult::PromotedToSingle(new_info) => {
                                    // Packed value promoted to Single, need to update node
                                    promoted.push((idx, new_info));
                                }
                                InPlaceUpdateResult::NeedsCow => {
                                    // Value doesn't fit in existing space, need full COW
                                    needs_cow.push((idx, *key, *value));
                                }
                            }
                        } else {
                            // Batch mode uses pure COW so rollback can safely truncate/revert root.
                            needs_cow.push((idx, *key, *value));
                        }
                    }
                    Err(_) => return Err(BPlusTreeError::KeyNotFound),
                }
            }

            // If all updates were in-place (no promotions, no COW), no node rewrite needed!
            if needs_cow.is_empty() && promoted.is_empty() {
                // Flush to ensure in-place writes hit disk
                self.file.get_mut().flush().map_err(BPlusTreeError::Io)?;
                return Ok(offset); // Return original offset - no node changes
            }

            // Apply promoted value_info updates
            for (idx, new_info) in promoted {
                node.value_info[idx] = new_info;
            }

            // Handle COW items - need to write new value blocks
            for (idx, _key, value) in needs_cow {
                let (val_off, val_len) = self.insert_value_to_disk(value).map_err(BPlusTreeError::Io)?;
                node.value_info[idx] =
                    ValueInfo { mode: ValueStorageMode::Single(val_off), length: val_len, cache: Mutex::new(None) };
            }

            let new_offset = self.write_node(&node).map_err(BPlusTreeError::Io)?;
            Ok(new_offset)
        } else {
            let mut pointers = pointers_opt
                .ok_or_else(|| BPlusTreeError::InvalidStructure("Internal node missing pointers".into()))?;
            let mut any_child_changed = false;

            let mut current_idx = 0;
            while current_idx < items.len() {
                let first_key_in_group = items[current_idx].0;
                let child_idx = get_entry_index_upper_bound::<K>(&node.keys, first_key_in_group);

                let mut group_end = current_idx + 1;
                while group_end < items.len()
                    && get_entry_index_upper_bound::<K>(&node.keys, items[group_end].0) == child_idx
                {
                    group_end += 1;
                }

                let sub_items = &items[current_idx..group_end];
                let original_child_offset = pointers[child_idx];
                let new_child_offset = self.update_batch_recursive(original_child_offset, sub_items, allow_in_place)?;

                if new_child_offset != original_child_offset {
                    pointers[child_idx] = new_child_offset;
                    any_child_changed = true;
                }

                current_idx = group_end;
            }

            // If no child offsets changed, no need to rewrite this internal node
            if !any_child_changed {
                return Ok(offset);
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

        let rollback_state = self.capture_batch_rollback_state().map_err(BPlusTreeError::Io)?;
        let result = (|| -> Result<u64, BPlusTreeError> {
            let mut sorted_items = items.to_vec();
            sorted_items.sort_by(|a, b| a.0.cmp(b.0));

            // Disable in-place updates for batch rollback safety.
            let new_root_offset = self.update_batch_recursive(self.root_offset, &sorted_items, false)?;

            // Only update header if root offset actually changed
            // (if all in-place updates succeeded, offset stays the same)
            if new_root_offset != self.root_offset {
                // Atomic Header Swap - only once at the end
                self.file.get_mut().seek(SeekFrom::Start(ROOT_OFFSET_POS)).map_err(BPlusTreeError::Io)?;
                self.file.get_mut().write_all(&new_root_offset.to_le_bytes()).map_err(BPlusTreeError::Io)?;
                self.root_offset = new_root_offset;
            }

            self.file.get_mut().flush().map_err(BPlusTreeError::Io)?;
            if self.should_sync_on_write() {
                self.file.get_mut().sync_all().map_err(BPlusTreeError::Io)?;
            }
            Ok(new_root_offset)
        })();

        match result {
            Ok(value) => Ok(value),
            Err(err) => {
                if let Err(rollback_err) = self.rollback_batch_state(rollback_state) {
                    return Err(BPlusTreeError::Io(io::Error::other(format!(
                        "update_batch failed: {err}; rollback failed: {rollback_err}"
                    ))));
                }
                Err(err)
            }
        }
    }

    fn delete_batch_recursive(&mut self, offset: u64, keys: &[&K]) -> io::Result<(u64, usize)> {
        let (mut node, pointers_opt) =
            BPlusTreeNode::<K, V>::deserialize_from_block(&mut self.file, &mut self.read_buffer, offset, false)?;

        if node.is_leaf {
            let mut deleted = 0usize;
            for key in keys {
                if let Ok(idx) = node.keys.binary_search(key) {
                    let already_tombstoned = node.value_info.get(idx).is_some_and(ValueInfo::is_tombstone);
                    if !already_tombstoned {
                        node.value_info[idx] = ValueInfo::tombstone();
                        deleted += 1;
                    }
                }
            }

            if deleted == 0 {
                return Ok((offset, 0));
            }

            let new_offset = self.write_node(&node)?;
            return Ok((new_offset, deleted));
        }

        let mut pointers = pointers_opt.ok_or_else(|| io::Error::other("Internal node missing pointers"))?;
        let mut total_deleted = 0usize;
        let mut any_child_changed = false;
        let mut current_idx = 0usize;

        while current_idx < keys.len() {
            let first_key_in_group = keys[current_idx];
            let child_idx = get_entry_index_upper_bound::<K>(&node.keys, first_key_in_group);

            let mut group_end = current_idx + 1;
            while group_end < keys.len() && get_entry_index_upper_bound::<K>(&node.keys, keys[group_end]) == child_idx {
                group_end += 1;
            }

            let sub_keys = &keys[current_idx..group_end];
            let original_child_offset = pointers[child_idx];
            let (new_child_offset, deleted) = self.delete_batch_recursive(original_child_offset, sub_keys)?;
            total_deleted += deleted;

            if new_child_offset != original_child_offset {
                pointers[child_idx] = new_child_offset;
                any_child_changed = true;
            }

            current_idx = group_end;
        }

        if !any_child_changed {
            return Ok((offset, total_deleted));
        }

        let new_offset = self.write_internal_node(&node, &pointers)?;
        Ok((new_offset, total_deleted))
    }

    pub fn delete(&mut self, key: &K) -> io::Result<bool> {
        let deleted = self.delete_batch(&[key])?;
        Ok(deleted > 0)
    }

    pub fn delete_batch(&mut self, keys: &[&K]) -> io::Result<usize> {
        if keys.is_empty() {
            return Ok(0);
        }

        let rollback_state = self.capture_batch_rollback_state()?;
        let result = (|| -> io::Result<usize> {
            let mut sorted_keys = keys.to_vec();
            sorted_keys.sort();
            sorted_keys.dedup_by(|a, b| *a == *b);

            let (new_root_offset, deleted) = self.delete_batch_recursive(self.root_offset, &sorted_keys)?;

            if new_root_offset != self.root_offset {
                self.file.get_mut().seek(SeekFrom::Start(ROOT_OFFSET_POS))?;
                self.file.get_mut().write_all(&new_root_offset.to_le_bytes())?;
                self.root_offset = new_root_offset;
            }

            if deleted > 0 {
                self.has_tombstones = true;
                let metadata_len = self.read_metadata_len_from_header()?;
                self.write_metadata_len_with_current_flags(metadata_len)?;
            }

            self.file.get_mut().flush()?;
            if self.should_sync_on_write() {
                self.file.get_mut().sync_all()?;
            }

            Ok(deleted)
        })();

        match result {
            Ok(value) => Ok(value),
            Err(err) => {
                if let Err(rollback_err) = self.rollback_batch_state(rollback_state) {
                    return Err(io::Error::other(format!(
                        "delete_batch failed: {err}; rollback failed: {rollback_err}"
                    )));
                }
                Err(err)
            }
        }
    }

    /// Try to update a value in-place if possible.
    /// Returns:
    /// - `Ok(Success)` if in-place update succeeded (no node rewrite needed)
    /// - `Ok(PromotedToSingle(info))` if packed value was promoted to Single (node rewrite needed)
    /// - `Ok(NeedsCow)` if new value doesn't fit (caller should use full COW)
    /// - `Err` on I/O error
    ///
    /// For Single storage: updates in-place if new value fits in existing space.
    /// For Packed storage:
    ///   - If new serialized size equals old size exactly: updates in-place within the packed block
    ///   - Otherwise: promotes to Single storage mode (writes at EOF, returns new `ValueInfo`)
    fn try_update_value_in_place(&mut self, value: &V, existing_info: &ValueInfo) -> io::Result<InPlaceUpdateResult> {
        // Serialize the new value first (needed for all paths)
        let raw_bytes = binary_serialize(value)?;

        match existing_info.mode {
            ValueStorageMode::Single(existing_offset) => {
                // Single storage: compress and check if it fits in existing space
                let (flag, payload) = compress_if_beneficial(&raw_bytes);
                let new_stored_len = 1 + payload.len(); // flag + payload
                let existing_len = existing_info.length as usize;

                if new_stored_len > existing_len {
                    return Ok(InPlaceUpdateResult::NeedsCow); // Doesn't fit
                }

                // Write in-place: [flag:1][payload][zero-padding to existing_len]
                self.file.seek(SeekFrom::Start(existing_offset))?;
                self.file.get_mut().write_all(&[flag])?;
                self.file.get_mut().write_all(&payload)?;

                // Zero-pad remaining space
                let padding_len = existing_len - new_stored_len;
                if padding_len > 0 {
                    let zeros = vec![0u8; padding_len];
                    self.file.get_mut().write_all(&zeros)?;
                }

                // Invalidate BufReader cache since we bypassed it to write
                self.file.stream_position()?; // alias for seek(SeekFrom::Current(0))

                Ok(InPlaceUpdateResult::Success)
            }
            ValueStorageMode::Packed(block_offset, value_index) => {
                // Packed storage: raw_bytes is the MessagePack-serialized value (no compression for packed)
                let new_len = raw_bytes.len();
                let existing_len = existing_info.length as usize;

                if new_len == existing_len {
                    // Same size: can update in-place within the packed block
                    self.update_packed_value_in_place(block_offset, value_index, &raw_bytes, &existing_info.cache)?;
                    Ok(InPlaceUpdateResult::Success)
                } else {
                    // Different size: promote to Single storage mode
                    // Write value at EOF as Single (with compression)
                    let (val_offset, val_len) = self.insert_value_to_disk(value)?;
                    let new_info = ValueInfo {
                        mode: ValueStorageMode::Single(val_offset),
                        length: val_len,
                        cache: Mutex::new(None),
                    };
                    Ok(InPlaceUpdateResult::PromotedToSingle(new_info))
                }
            }
            ValueStorageMode::Tombstone => Ok(InPlaceUpdateResult::NeedsCow),
        }
    }

    /// Update a value in-place within a packed block.
    /// The new value must have the exact same serialized size as the existing value.
    fn update_packed_value_in_place(
        &mut self,
        block_offset: u64,
        value_index: u16,
        new_value_bytes: &[u8],
        cache: &Mutex<Option<CacheData>>,
    ) -> io::Result<()> {
        // Optimization: Use cached offset if available to skip read and scan
        let cached_pos = {
            let guard = cache.lock();
            if let Some(CacheData::PackedOffset(pos)) = guard.as_ref() {
                Some(*pos)
            } else {
                None
            }
        };

        if let Some(pos) = cached_pos {
            // Jump directly to the offset (avoid reading entire block and linear scan)
            self.file.seek(SeekFrom::Start(block_offset + u64::from(pos)))?;
            self.file.get_mut().write_all(new_value_bytes)?;
            self.file.stream_position()?;
            return Ok(());
        }

        // Read the entire packed block
        self.file.seek(SeekFrom::Start(block_offset))?;
        let mut block_buffer = vec![0u8; PAGE_SIZE_USIZE];
        self.file.read_exact(&mut block_buffer)?;

        // Navigate to the target value's position
        // Format: [COUNT:4B][LEN:4B][data...][LEN:4B][data...]...
        let mut pos = 4; // Skip count

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
                // Found target value - verify size matches
                if len != new_value_bytes.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("Size mismatch in packed update: expected {len}, got {}", new_value_bytes.len()),
                    ));
                }

                // Update the value data in the buffer
                block_buffer[pos..pos + len].copy_from_slice(new_value_bytes);

                // Write the entire block back (optimization: we could write just the slice,
                // but we already have the whole block in-memory here and write_all(4KB) is fast)
                self.file.seek(SeekFrom::Start(block_offset))?;
                self.file.get_mut().write_all(&block_buffer)?;

                // Cache the offset for future updates
                *cache.lock() = Some(CacheData::PackedOffset(u16::try_from(pos).map_err(to_io_error)?));

                // Invalidate BufReader cache
                self.file.stream_position()?; // alias for seek(SeekFrom::Current(0))

                return Ok(());
            }

            pos += len;
        }

        Err(io::Error::new(io::ErrorKind::InvalidData, format!("Value index {value_index} not found in packed block")))
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
        self.serial_buffer.clear();
        node.serialize_to_block(self.file.get_mut(), &mut self.write_buffer, &mut self.serial_buffer, offset)?;
        Ok(offset)
    }

    fn write_internal_node(&mut self, node: &BPlusTreeNode<K, V>, pointers: &[u64]) -> io::Result<u64> {
        self.file.get_mut().seek(SeekFrom::End(0))?;
        let offset = self.file.get_mut().stream_position()?;
        self.serial_buffer.clear();
        node.serialize_internal_with_offsets(
            self.file.get_mut(),
            &mut self.write_buffer,
            &mut self.serial_buffer,
            offset,
            pointers,
        )?;
        Ok(offset)
    }

    fn upsert_batch_recursive(&mut self, offset: u64, items: &[(&K, &V)]) -> io::Result<(u64, Vec<(K, u64)>)> {
        let (mut node, pointers_opt) = BPlusTreeNode::<K, V>::deserialize_from_block(
            &mut self.file,
            &mut self.read_buffer,
            offset,
            false, // shallow
        )?;

        if node.is_leaf {
            for (key, value) in items {
                let (val_off, val_len) = self.insert_value_to_disk(value)?;
                let new_info =
                    ValueInfo { mode: ValueStorageMode::Single(val_off), length: val_len, cache: Mutex::new(None) };

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
                while group_end < items.len()
                    && get_entry_index_upper_bound::<K>(&node.keys, items[group_end].0) == child_idx
                {
                    group_end += 1;
                }

                let sub_items = &items[current_idx..group_end];
                let (new_child_offset, child_promotions) =
                    self.upsert_batch_recursive(pointers[child_idx], sub_items)?;
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

        let rollback_state = self.capture_batch_rollback_state()?;
        let result = (|| -> io::Result<u64> {
            let mut sorted_items = items.to_vec();
            sorted_items.sort_by(|a, b| a.0.cmp(b.0));

            let (mut current_root, promotions) = self.upsert_batch_recursive(self.root_offset, &sorted_items)?;

            // Handle promotions (splits) using balanced approach
            current_root = self.build_higher_levels(current_root, promotions)?;

            self.file.get_mut().seek(SeekFrom::Start(ROOT_OFFSET_POS))?;
            self.file.get_mut().write_all(&current_root.to_le_bytes())?;
            self.file.get_mut().flush()?;
            if self.should_sync_on_write() {
                self.file.get_mut().sync_all()?;
            }

            self.root_offset = current_root;
            Ok(current_root)
        })();

        match result {
            Ok(value) => Ok(value),
            Err(err) => {
                if let Err(rollback_err) = self.rollback_batch_state(rollback_state) {
                    return Err(io::Error::other(format!(
                        "upsert_batch failed: {err}; rollback failed: {rollback_err}"
                    )));
                }
                Err(err)
            }
        }
    }

    pub fn upsert_batch_encoded(&mut self, items: Vec<(K, Vec<u8>)>) -> io::Result<u64> {
        self.upsert_batch_preserialized(items)
    }

    /// Upsert multiple items using pre-serialized key-value data.
    ///
    /// This method is designed for use with `spawn_blocking` where you want to:
    /// 1. Serialize values in the async context (before `spawn_blocking`)
    /// 2. Pass only `Vec<u8>` bytes into the blocking context (avoiding clones)
    /// 3. Perform all I/O in a single blocking call
    ///
    /// The key type K still follows the tree bounds (Ord + Serialize + Deserialize),
    /// but values are written as raw bytes without re-serialization.
    ///
    /// # Arguments
    /// * `items` - (`key`, `value_bytes`) pairs where `value_bytes` is MessagePack-encoded.
    ///   Keys are already typed and used directly for tree traversal.
    ///
    /// # Returns
    /// The final root offset after all upserts, or an error if any operation fails
    pub fn upsert_batch_preserialized(&mut self, items: Vec<(K, Vec<u8>)>) -> io::Result<u64> {
        if items.is_empty() {
            return Ok(self.root_offset);
        }

        let rollback_state = self.capture_batch_rollback_state()?;
        let result = (|| -> io::Result<u64> {
            // // Sort by key for efficient batch insertion
            let mut sorted_items = items;
            sorted_items.sort_by(|a, b| a.0.cmp(&b.0));

            let (mut current_root, promotions) =
                self.upsert_batch_preserialized_recursive(self.root_offset, &sorted_items)?;

            // Handle promotions (splits) using a balanced approach
            current_root = self.build_higher_levels(current_root, promotions)?;

            self.file.get_mut().seek(SeekFrom::Start(ROOT_OFFSET_POS))?;
            self.file.get_mut().write_all(&current_root.to_le_bytes())?;
            self.file.get_mut().flush()?;
            if self.should_sync_on_write() {
                self.file.get_mut().sync_all()?;
            }

            self.root_offset = current_root;
            Ok(current_root)
        })();

        match result {
            Ok(value) => Ok(value),
            Err(err) => {
                if let Err(rollback_err) = self.rollback_batch_state(rollback_state) {
                    return Err(io::Error::other(format!(
                        "upsert_batch_preserialized failed: {err}; rollback failed: {rollback_err}"
                    )));
                }
                Err(err)
            }
        }
    }

    pub fn commit(&mut self) -> io::Result<()> {
        self.file.get_mut().flush()?;
        if self.should_sync_on_commit() {
            self.file.get_mut().sync_all()?;
        }
        Ok(())
    }

    /// Recursive helper for `upsert_batch_preserialized`.
    /// Items is a slice of (key, pre-serialized value bytes).
    fn upsert_batch_preserialized_recursive(
        &mut self,
        offset: u64,
        items: &[(K, Vec<u8>)],
    ) -> io::Result<(u64, Vec<(K, u64)>)> {
        let (mut node, pointers_opt) = BPlusTreeNode::<K, V>::deserialize_from_block(
            &mut self.file,
            &mut self.read_buffer,
            offset,
            false, // shallow
        )?;

        if node.is_leaf {
            for (key, value_bytes) in items {
                // Write pre-serialized value to disk with compression
                let (val_off, val_len) = self.insert_preserialized_value_to_disk(value_bytes)?;
                let new_info =
                    ValueInfo { mode: ValueStorageMode::Single(val_off), length: val_len, cache: Mutex::new(None) };

                match node.keys.binary_search(key) {
                    Ok(idx) => {
                        node.value_info[idx] = new_info;
                    }
                    Err(idx) => {
                        node.keys.insert(idx, key.clone());
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
                let first_key_in_group = &items[current_idx].0;
                let child_idx = get_entry_index_upper_bound::<K>(&node.keys, first_key_in_group);

                let mut group_end = current_idx + 1;
                while group_end < items.len()
                    && get_entry_index_upper_bound::<K>(&node.keys, &items[group_end].0) == child_idx
                {
                    group_end += 1;
                }

                let sub_items = &items[current_idx..group_end];
                let (new_child_offset, child_promotions) =
                    self.upsert_batch_preserialized_recursive(pointers[child_idx], sub_items)?;
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

    /// Insert a pre-serialized value (already `MessagePack` encoded) to disk.
    /// Applies compression if beneficial.
    fn insert_preserialized_value_to_disk(&mut self, value_bytes: &[u8]) -> io::Result<(u64, u32)> {
        // Apply compression if beneficial
        let (flag, payload) = compress_if_beneficial(value_bytes);

        self.file.get_mut().seek(SeekFrom::End(0))?;
        let offset = self.file.get_mut().stream_position()?;

        // Write: [flag:1][payload]
        self.file.get_mut().write_all(&[flag])?;
        self.file.get_mut().write_all(&payload)?;

        let stored_len = 1 + payload.len();
        Ok((offset, u32::try_from(stored_len).map_err(to_io_error)?))
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
                        let (l, _) = decode_metadata_len_and_flags(u32::from_le_bytes(lbuf));
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
        let metadata_len = u32::try_from(metadata.len()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let metadata_len_with_flags = encode_metadata_len_with_flags(metadata_len, false);
        header[16..20].copy_from_slice(&metadata_len_with_flags.to_le_bytes());
        if !metadata.is_empty() {
            header[METADATA_DATA_START_POS..METADATA_DATA_START_POS + metadata.len()].copy_from_slice(&metadata);
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
                    cache: Mutex::new(None),
                });

                if current_leaf.keys.len() >= self.leaf_order {
                    let first_key = current_leaf.keys[0].clone();
                    let node_offset = current_offset;
                    // Created temporary scratch buffer for serialization
                    let mut serial_buf = Vec::new();
                    current_offset = current_leaf.serialize_to_block(
                        &mut write_buffer,
                        &mut node_buffer,
                        &mut serial_buf,
                        node_offset,
                    )?;
                    leaf_pointers.push((first_key, node_offset));
                    current_leaf = BPlusTreeNode::new(true);
                }
            }

            // Handle trailing leaf
            if !current_leaf.keys.is_empty() {
                let first_key = current_leaf.keys[0].clone();
                let node_offset = current_offset;
                let mut serial_buf = Vec::new(); // Recyle? No loop here.
                current_offset = current_leaf.serialize_to_block(
                    &mut write_buffer,
                    &mut node_buffer,
                    &mut serial_buf,
                    node_offset,
                )?;
                leaf_pointers.push((first_key, node_offset));
            }
            write_buffer.flush()?;
        }

        // 3. Build Internal Levels
        let tree = BPlusTree::<K, V>::new();
        let mut node_buffer = vec![0u8; PAGE_SIZE_USIZE];
        let root_offset =
            tree.build_levels_from_pointers(&mut temp_file, leaf_pointers, current_offset, &mut node_buffer)?;

        // 4. Update Header
        temp_file.seek(SeekFrom::Start(ROOT_OFFSET_POS))?;
        temp_file.write_all(&root_offset.to_le_bytes())?;

        temp_file.flush()?;
        // temp_file.as_file().sync_all()?; // Removed as requested; relying on persisted or OS flush policy

        // 5. Atomic Replace
        temp_file.persist(filepath).map_err(to_io_error)?;

        // 6. Refresh state
        self.root_offset = root_offset;
        self.has_tombstones = false;
        let file = utils::open_read_write_file(filepath)?;
        self.file = utils::file_reader(file);
        self.cache.clear();

        Ok(())
    }
}

/// Single-writer wrapper that serializes prepared batch writes through one updater instance.
///
/// This wrapper does not implement write-ahead logging (WAL) semantics or
/// crash-recovery replay. It only serializes access through a shared updater.
pub struct BPlusTreeSerialWriter<K, V> {
    updater: Arc<Mutex<BPlusTreeUpdate<K, V>>>,
    flush_policy: FlushPolicy,
    dirty: Arc<AtomicBool>,
    background_commit_shutdown: Arc<AtomicBool>,
    background_commit_handle: Mutex<Option<JoinHandle<()>>>,
}

impl<K, V> BPlusTreeSerialWriter<K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone + Send + 'static,
    V: Serialize + for<'de> Deserialize<'de> + Clone + Send + 'static,
{
    pub fn new(filepath: &Path, flush_policy: FlushPolicy) -> io::Result<Self> {
        let mut updater = BPlusTreeUpdate::<K, V>::try_new_with_backoff(filepath)?;
        updater.set_flush_policy(flush_policy);
        Ok(Self {
            updater: Arc::new(Mutex::new(updater)),
            flush_policy,
            dirty: Arc::new(AtomicBool::new(false)),
            background_commit_shutdown: Arc::new(AtomicBool::new(false)),
            background_commit_handle: Mutex::new(None),
        })
    }

    pub fn upsert_prepared(&self, items: Vec<(K, Vec<u8>)>) -> io::Result<u64> {
        let result = self.updater.lock().upsert_batch_encoded(items);
        if result.is_ok() {
            self.mark_dirty_after_write();
        }
        result
    }

    pub fn upsert(&self, items: &[(&K, &V)]) -> io::Result<u64> {
        let prepared = BPlusTreeUpdate::<K, V>::prepare_upsert_batch(items)?;
        self.upsert_prepared(prepared)
    }

    #[inline]
    fn mark_dirty_after_write(&self) {
        match self.flush_policy {
            FlushPolicy::Batch => self.dirty.store(true, Ordering::Release),
            FlushPolicy::Immediate | FlushPolicy::None => self.dirty.store(false, Ordering::Release),
        }
    }

    pub fn start_background_commit(&self, interval: Duration) -> io::Result<()> {
        if self.flush_policy != FlushPolicy::Batch {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "background commit requires FlushPolicy::Batch"));
        }
        if interval.is_zero() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "background commit interval must be > 0"));
        }

        let mut handle_slot = self.background_commit_handle.lock();
        if handle_slot.is_some() {
            return Ok(());
        }

        self.background_commit_shutdown.store(false, Ordering::Release);
        let updater = Arc::clone(&self.updater);
        let dirty = Arc::clone(&self.dirty);
        let shutdown = Arc::clone(&self.background_commit_shutdown);

        let handle = std::thread::Builder::new()
            .name("bplustree-commit".to_string())
            .spawn(move || {
                while !shutdown.load(Ordering::Acquire) {
                    std::thread::park_timeout(interval);
                    if shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    if !dirty.swap(false, Ordering::AcqRel) {
                        continue;
                    }
                    if let Err(err) = updater.lock().commit() {
                        error!("Background B+Tree commit failed: {err}");
                        dirty.store(true, Ordering::Release);
                    }
                }

                if dirty.swap(false, Ordering::AcqRel) {
                    if let Err(err) = updater.lock().commit() {
                        error!("Final background B+Tree commit failed during shutdown: {err}");
                    }
                }
            })
            .map_err(io::Error::other)?;
        *handle_slot = Some(handle);
        Ok(())
    }

    pub fn stop_background_commit(&self) -> io::Result<()> {
        self.background_commit_shutdown.store(true, Ordering::Release);
        if let Some(handle) = self.background_commit_handle.lock().take() {
            handle.thread().unpark();
            handle.join().map_err(|_| io::Error::other("background B+Tree commit thread panicked"))?;
        }
        Ok(())
    }

    /// Explicit durability barrier.
    pub fn flush_now(&self) -> io::Result<()> { self.commit() }

    pub fn commit(&self) -> io::Result<()> {
        let result = self.updater.lock().commit();
        if result.is_ok() {
            self.dirty.store(false, Ordering::Release);
        }
        result
    }

    /// Alias for `commit()`.
    pub fn shutdown(&self) -> io::Result<()> {
        self.stop_background_commit()?;
        self.commit()
    }
}

impl<K, V> Drop for BPlusTreeSerialWriter<K, V> {
    fn drop(&mut self) {
        self.background_commit_shutdown.store(true, Ordering::Release);
        if let Some(handle) = self.background_commit_handle.lock().take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}

#[deprecated(note = "Not a real WAL implementation; use BPlusTreeSerialWriter instead")]
pub type BPlusTreeWalWriter<K, V> = BPlusTreeSerialWriter<K, V>;

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
        Self { stack, current_keys: None, current_values: None, index: 0 }
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
    pub fn iter(&self) -> BPlusTreeIterator<'_, K, V> { BPlusTreeIterator::new(self) }
}

impl<'a, K, V> IntoIterator for &'a BPlusTree<K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = (&'a K, &'a V);
    type IntoIter = BPlusTreeIterator<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter { self.iter() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::bplustree::{BPlusTree, BPlusTreeQuery, BPlusTreeUpdate};
    use parking_lot::Mutex;
    use serde::{de::Deserializer, ser::Error as SerError, Deserialize, Serialize, Serializer};
    use shared::utils::generate_random_string;
    use std::{collections::HashSet, io};
    use tempfile::tempdir;

    #[cfg(unix)]
    #[allow(dead_code)]
    fn process_is_alive(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }

        #[cfg(target_os = "linux")]
        {
            // Fast path for Linux/musl.
            if Path::new("/proc").join(pid.to_string()).exists() {
                return true;
            }
        }

        let pid_raw: libc::pid_t = match pid.try_into() {
            Ok(value) => value,
            Err(_) => return false,
        };

        // kill(pid, 0) probes process existence without sending a signal.
        let rc = unsafe { libc::kill(pid_raw, 0) };
        if rc == 0 {
            return true;
        }

        match io::Error::last_os_error().raw_os_error() {
            Some(code) if code == libc::ESRCH => false,
            Some(code) if code == libc::EPERM => true,
            _ => false,
        }
    }

    #[cfg(windows)]
    #[allow(dead_code)]
    fn process_is_alive(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }

        // PROCESS_QUERY_LIMITED_INFORMATION is sufficient to check process existence.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle == 0 {
            return false;
        }

        unsafe {
            CloseHandle(handle);
        }
        true
    }

    // Example usage with a simple struct
    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    struct Record {
        id: u32,
        data: String,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct FailingValue {
        payload: String,
        fail_serialize: bool,
    }

    impl FailingValue {
        fn ok(payload: impl Into<String>) -> Self { Self { payload: payload.into(), fail_serialize: false } }

        fn failing(payload: impl Into<String>) -> Self { Self { payload: payload.into(), fail_serialize: true } }
    }

    impl Serialize for FailingValue {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            if self.fail_serialize {
                return Err(S::Error::custom("intentional serialize failure"));
            }
            self.payload.serialize(serializer)
        }
    }

    impl<'de> Deserialize<'de> for FailingValue {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            let payload = String::deserialize(deserializer)?;
            Ok(Self::ok(payload))
        }
    }

    #[test]
    fn test_process_is_alive_current_process() {
        let current_pid = std::process::id();
        assert!(process_is_alive(current_pid));
        assert!(!process_is_alive(u32::MAX));
    }

    #[test]
    fn insert_test() -> io::Result<()> {
        let test_size = 500;
        let content = generate_random_string(1024);
        let mut tree = BPlusTree::<u32, Record>::new();
        for i in 0u32..=test_size {
            tree.insert(i, Record { id: i, data: format!("{content} {i}") });
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
            assert!(found.unwrap().eq(&Record { id: i, data: format!("{content} {i}") }), "{content} {i} not found");
        }

        let mut tree_query: BPlusTreeQuery<u32, Record> = BPlusTreeQuery::try_new(&filepath)?;
        for i in 0u32..=test_size {
            let found = tree_query.query(&i).expect("Query failed");
            assert!(found.is_some(), "{content} {i} not found");
            let entry = found.unwrap();
            assert!(entry.eq(&Record { id: i, data: format!("{content} {i}") }), "{content} {i} not found");
        }

        let mut tree_update: BPlusTreeUpdate<u32, Record> = BPlusTreeUpdate::try_new(&filepath)?;

        for i in 0u32..=test_size {
            if let Ok(Some(record)) = tree_update.query(&i) {
                let new_record = Record { id: record.id, data: format!("{content} {}", record.id + 9000) };
                tree_update.update(&i, new_record).map_err(BPlusTreeError::to_io)?;
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
            let expected = Record { id: i, data: format!("{content} {}", i + 9000) };
            assert!(entry.eq(&expected), "Entry not equal {entry:?} != {expected:?}");
        }

        Ok(())
    }

    #[test]
    fn insert_duplicate_test() {
        let content = "Entry";
        let mut tree = BPlusTree::<u32, Record>::new();
        for i in 0u32..=500 {
            tree.insert(i, Record { id: i, data: format!("{content} {i}") });
        }
        for i in 0u32..=500 {
            tree.insert(i, Record { id: i, data: format!("{content} {}", i + 1) });
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

        update.upsert_batch(&[(&1, &r1_new), (&3, &r3_new)])?;

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
            tree.insert(i, Record { id: i, data: format!("data {i}") });
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
        update.update(&1, Record { id: 1, data: "updated".to_string() }).map_err(BPlusTreeError::to_io)?;
        assert_eq!(update.len().expect("Update len failed after update"), test_size as usize);

        // Insert new key - length should increase
        update.upsert_batch(&[(&(test_size + 1), &Record { id: test_size + 1, data: "new".to_string() })])?;
        assert_eq!(update.len().expect("Update len failed after insert"), (test_size + 1) as usize);

        Ok(())
    }

    #[test]
    fn iterator_test() -> io::Result<()> {
        let mut tree = BPlusTree::<u32, Record>::new();
        let mut entry_set = HashSet::new();
        for i in 0u32..=500 {
            tree.insert(i, Record { id: i, data: format!("Entry {i}") });
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
                tree_update
                    .update(&i, Record { id: i, data: format!("UpdatedContent {i}") })
                    .map_err(BPlusTreeError::to_io)?;
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
                assert_eq!(
                    value.data,
                    format!("UpdatedContent {key}"),
                    "Iterator returned wrong value for updated key {key}"
                );
            } else {
                assert_eq!(
                    value.data,
                    format!("{content} {key}"),
                    "Iterator returned wrong value for original key {key}"
                );
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

        // Use incompressible data > SMALL_VALUE_THRESHOLD (256 bytes) to ensure Single storage.
        // Repetitive strings like "x".repeat(300) compress too well with LZ4 and end up
        // smaller than 256 bytes, causing them to be stored as Packed (not Single).
        // Random-looking strings don't compress well and stay above the threshold.
        let padding: String = generate_random_string(400);
        for i in 0u32..10 {
            // Each record has unique data to prevent compression
            tree.insert(i, Record { id: i, data: format!("{padding}{i}") });
        }
        tree.store(&filepath)?;

        let initial_size = std::fs::metadata(&filepath)?.len();

        // Update with same-length incompressible data - should be in-place, no file growth
        let mut tree_update = BPlusTreeUpdate::<u32, Record>::try_new(&filepath)?;
        let same_size_padding: String = generate_random_string(400);
        for i in 0u32..10 {
            tree_update
                .update(&i, Record { id: i, data: format!("{same_size_padding}{i}") })
                .map_err(BPlusTreeError::to_io)?;
        }

        let size_after_same_update = std::fs::metadata(&filepath)?.len();
        // With in-place update optimization, same-size updates should NOT grow the file
        assert_eq!(
            size_after_same_update, initial_size,
            "Same-size updates should happen in-place without file growth"
        );
        drop(tree_update);

        // Reload and verify the in-place updates worked
        let mut tree_query = BPlusTreeQuery::<u32, Record>::try_new(&filepath)?;
        for i in 0u32..10 {
            let val = tree_query.query(&i).expect("Query failed").expect("Should find key");
            assert!(val.data.starts_with(&same_size_padding), "Updated value should contain new padding");
        }
        drop(tree_query);

        // Update with smaller size data - should also be in-place, no file growth
        // Using 200-char random string (smaller than 400 but > threshold for incompressibility)
        let mut tree_update = BPlusTreeUpdate::<u32, Record>::try_new(&filepath)?;
        let smaller_padding: String = generate_random_string(200);
        for i in 0u32..10 {
            tree_update
                .update(&i, Record { id: i, data: format!("{smaller_padding}{i}") })
                .map_err(BPlusTreeError::to_io)?;
        }

        let size_after_smaller_update = std::fs::metadata(&filepath)?.len();
        assert_eq!(
            size_after_smaller_update, initial_size,
            "Smaller updates should happen in-place without file growth"
        );

        // Update with larger size data - should trigger COW and file growth
        // 5000 chars is much larger than the original ~400 byte allocation
        let larger_padding: String = generate_random_string(5000);
        for i in 0u32..1 {
            tree_update
                .update(&i, Record { id: i, data: format!("{larger_padding}{i}") })
                .map_err(BPlusTreeError::to_io)?;
        }

        let size_after_larger_update = std::fs::metadata(&filepath)?.len();
        assert!(size_after_larger_update > initial_size, "Larger updates should trigger COW and grow the file");

        // Final verification: Compact should shrink the file
        tree_update.compact(&filepath)?;
        let size_after_compact = std::fs::metadata(&filepath)?.len();
        assert!(size_after_compact < size_after_larger_update, "Compaction should reduce file size");
        drop(tree_update);

        // Final data check after compact
        let mut final_query = BPlusTreeQuery::<u32, Record>::try_new(&filepath)?;
        // Key 0 was updated with larger padding
        assert!(final_query.query(&0).unwrap().unwrap().data.starts_with(&larger_padding));
        // Keys 1-9 were updated with smaller padding
        for i in 1u32..10 {
            assert!(final_query.query(&i).unwrap().unwrap().data.starts_with(&smaller_padding));
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
            tree_update
                .update(&i, Record { id: i, data: format!("UpdatedContent {i}") })
                .map_err(BPlusTreeError::to_io)?;
        }

        // 3. Verify Query Integrity (Must return NEW values)
        for i in (0..test_size).step_by(10) {
            let val = tree_update.query(&i).expect("Query failed").expect("Should find updated key");
            assert_eq!(val.data, format!("UpdatedContent {i}"));
        }

        // 4. Verify Query Integrity for non-updated keys (Must return OLD values)
        for i in (1..test_size).step_by(11) {
            if i % 10 == 0 {
                continue;
            } // skip updated ones
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
        tree_update.update(&10, Record { id: 10, data: "NewVal".to_string() }).map_err(BPlusTreeError::to_io)?;

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
            tree.insert(i, Record { id: i, data: format!("Initial {i}") });
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

        let update_refs: Vec<(&u32, &Record)> = updates.iter().map(|(k, v)| (k, v)).collect();

        tree_update.update_batch(&update_refs).map_err(BPlusTreeError::to_io)?;
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
            tree.insert(i, Record { id: i, data: format!("Initial {i}") });
        }
        tree.store(&filepath)?;
        drop(tree);

        let mut tree_update = BPlusTreeUpdate::<u32, Record>::try_new(&filepath)?;
        let initial_root = tree_update.root_offset;

        // Test empty batch - should be no-op
        let empty_batch: Vec<(&u32, &Record)> = vec![];
        let result = tree_update.update_batch(&empty_batch).map_err(BPlusTreeError::to_io)?;

        assert_eq!(result, initial_root, "Empty batch should not change root offset");

        // Verify data unchanged
        for i in 0u32..10 {
            let val = tree_update.query(&i).expect("Query failed").expect("Should find key");
            assert_eq!(val.data, format!("Initial {i}"));
        }

        Ok(())
    }

    #[test]
    fn update_batch_error_rolls_back_file_and_data() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_update_batch_rollback.bin");
        let mut tree = BPlusTree::<u32, String>::new();
        let count = 2_000u32;
        for i in 0..count {
            tree.insert(i, format!("v{i}"));
        }
        tree.store(&filepath)?;
        drop(tree);

        let size_before = std::fs::metadata(&filepath)?.len();
        let mut updater = BPlusTreeUpdate::<u32, String>::try_new(&filepath)?;
        let batch_updates = [(0u32, "X".repeat(6_000)), (u32::MAX, "missing".to_string())];
        let refs: Vec<(&u32, &String)> = batch_updates.iter().map(|(k, v)| (k, v)).collect();
        let result = updater.update_batch(&refs);
        assert!(matches!(result, Err(BPlusTreeError::KeyNotFound)));
        drop(updater);

        let size_after = std::fs::metadata(&filepath)?.len();
        assert_eq!(size_after, size_before, "failed batch must roll back file growth");

        // Ensure header/root offset remains valid for update/load paths.
        let mut reopened_updater = BPlusTreeUpdate::<u32, String>::try_new(&filepath)?;
        assert_eq!(reopened_updater.query(&0).map_err(BPlusTreeError::to_io)?, Some("v0".to_string()));
        drop(reopened_updater);
        let loaded_tree = BPlusTree::<u32, String>::load(&filepath)?;
        assert_eq!(loaded_tree.query(&0).cloned(), Some("v0".to_string()));

        let mut query = BPlusTreeQuery::<u32, String>::try_new(&filepath)?;
        assert_eq!(query.query(&0).map_err(BPlusTreeError::to_io)?, Some("v0".to_string()));
        assert!(query.query(&u32::MAX).map_err(BPlusTreeError::to_io)?.is_none());

        Ok(())
    }

    #[test]
    fn update_batch_error_rolls_back_in_place_candidate_write() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_update_batch_in_place_rollback.bin");
        let original = "A".repeat(500);
        let updated = "B".repeat(500);

        let mut tree = BPlusTree::<u32, String>::new();
        tree.insert(1, original.clone());
        tree.store(&filepath)?;
        drop(tree);

        let mut batch_updater = BPlusTreeUpdate::<u32, String>::try_new(&filepath)?;
        let update_items = [(1u32, updated.clone()), (u32::MAX, "missing".to_string())];
        let refs: Vec<(&u32, &String)> = update_items.iter().map(|(k, v)| (k, v)).collect();

        let result = batch_updater.update_batch(&refs);
        assert!(matches!(result, Err(BPlusTreeError::KeyNotFound)));
        drop(batch_updater);

        // On failed batch, key 1 must still have original value.
        let mut query = BPlusTreeQuery::<u32, String>::try_new(&filepath)?;
        assert_eq!(query.query(&1).map_err(BPlusTreeError::to_io)?, Some(original));
        assert_ne!(query.query(&1).map_err(BPlusTreeError::to_io)?, Some(updated));

        Ok(())
    }

    #[test]
    fn upsert_batch_error_rolls_back_file_and_data() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_upsert_batch_rollback.bin");
        let mut tree = BPlusTree::<u32, FailingValue>::new();
        tree.insert(1, FailingValue::ok("initial"));
        tree.store(&filepath)?;
        drop(tree);

        let size_before = std::fs::metadata(&filepath)?.len();
        let mut updater = BPlusTreeUpdate::<u32, FailingValue>::try_new(&filepath)?;
        let batch = [(1u32, FailingValue::ok("updated")), (2u32, FailingValue::failing("boom"))];
        let refs: Vec<(&u32, &FailingValue)> = batch.iter().map(|(k, v)| (k, v)).collect();
        assert!(updater.upsert_batch(&refs).is_err());
        drop(updater);

        let size_after = std::fs::metadata(&filepath)?.len();
        assert_eq!(size_after, size_before, "failed upsert batch must roll back file growth");

        let mut query = BPlusTreeQuery::<u32, FailingValue>::try_new(&filepath)?;
        let existing = query.query(&1).map_err(BPlusTreeError::to_io)?;
        assert_eq!(existing.map(|v| v.payload), Some("initial".to_string()));
        assert!(query.query(&2).map_err(BPlusTreeError::to_io)?.is_none());

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
            tree.insert(i, Record { id: i, data: format!("Initial {i}") });
        }
        tree.store(&filepath)?;
        drop(tree);

        let mut tree_update = BPlusTreeUpdate::<u32, Record>::try_new(&filepath)?;

        // Prepare large batch update (every other item)
        let updates: Vec<(u32, Record)> = (0..test_size)
            .filter(|i| i % 2 == 0)
            .map(|i| (i, Record { id: i, data: format!("BatchUpdated {i}") }))
            .collect();

        let update_refs: Vec<(&u32, &Record)> = updates.iter().map(|(k, v)| (k, v)).collect();

        // Perform batch update
        tree_update.update_batch(&update_refs).map_err(BPlusTreeError::to_io)?;
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
            tree.insert(i, Record { id: i, data: large_data.clone() });
        }
        tree.store(&filepath)?;
        drop(tree);

        let mut tree_update = BPlusTreeUpdate::<u32, Record>::try_new(&filepath)?;

        // Batch update with smaller data
        let small_data = "y".repeat(50);
        let updates: Vec<(u32, Record)> =
            (0u32..100).map(|i| (i, Record { id: i, data: small_data.clone() })).collect();

        let update_refs: Vec<(&u32, &Record)> = updates.iter().map(|(k, v)| (k, v)).collect();

        tree_update.update_batch(&update_refs).map_err(BPlusTreeError::to_io)?;

        let size_before_compact = std::fs::metadata(&filepath)?.len();

        // Compact to reclaim space
        tree_update.compact(&filepath)?;

        let size_after_compact = std::fs::metadata(&filepath)?.len();
        assert!(size_after_compact < size_before_compact, "Compaction should reduce file size after batch update");

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

        assert!(tree_check.query(&1).map_err(BPlusTreeError::to_io)?.is_some(), "Should have key 1");
        assert!(tree_check.query(&2).map_err(BPlusTreeError::to_io)?.is_some(), "Should have key 2");
        assert!(
            tree_check.query(&3).map_err(BPlusTreeError::to_io)?.is_some(),
            "Should have key 3 - if missing, file handle wasn't updated"
        );

        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn query_remaps_after_atomic_replace_with_same_length() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_query_replace_target.bin");
        let replacement_path = tempdir.path().join("tree_query_replace_source.bin");

        let mut tree = BPlusTree::<u32, Record>::new();
        tree.insert(1, Record { id: 1, data: "aaaa".to_string() });
        tree.insert(2, Record { id: 2, data: "bbbb".to_string() });
        tree.store(&filepath)?;

        let mut query = BPlusTreeQuery::<u32, Record>::try_new(&filepath)?;
        assert!(query.mmap.is_some(), "Test requires mmap-backed query");
        let initial = query.query(&1).map_err(BPlusTreeError::to_io)?.expect("missing initial key");
        assert_eq!(initial.data, "aaaa");

        let mut replacement = BPlusTree::<u32, Record>::new();
        replacement.insert(1, Record { id: 1, data: "cccc".to_string() });
        replacement.insert(2, Record { id: 2, data: "dddd".to_string() });
        replacement.store(&replacement_path)?;

        let old_len = std::fs::metadata(&filepath)?.len();
        let replacement_len = std::fs::metadata(&replacement_path)?.len();
        assert_eq!(old_len, replacement_len, "test requires same-length replacement file");

        std::fs::rename(&replacement_path, &filepath)?;

        let refreshed = query.query(&1).map_err(BPlusTreeError::to_io)?.expect("missing replaced key");
        assert_eq!(refreshed.data, "cccc");

        Ok(())
    }

    #[test]
    fn upsert_batch_mixed_test() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_upsert_batch_mixed.bin");
        let mut tree = BPlusTree::<u32, Record>::new();

        // Create initial tree with keys 0-49
        for i in 0u32..50 {
            tree.insert(i, Record { id: i, data: format!("Initial {i}") });
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

        let update_refs: Vec<(&u32, &Record)> = updates.iter().map(|(k, v)| (k, v)).collect();

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
            tree.insert(i, Record { id: i, data: format!("Initial {i}") });
        }
        tree.store(&filepath)?;
        drop(tree);

        let mut tree_update = BPlusTreeUpdate::<u32, Record>::try_new(&filepath)?;

        // Upsert all new keys (100-149)
        let updates: Vec<(u32, Record)> =
            (100u32..150).map(|i| (i, Record { id: i, data: format!("New {i}") })).collect();

        let update_refs: Vec<(&u32, &Record)> = updates.iter().map(|(k, v)| (k, v)).collect();

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
            tree.insert(i, Record { id: i, data: format!("Initial {i}") });
        }
        tree.store(&filepath)?;
        drop(tree);

        let mut tree_update = BPlusTreeUpdate::<u32, Record>::try_new(&filepath)?;

        // Upsert all existing keys (should behave like update)
        let updates: Vec<(u32, Record)> =
            (0u32..100).map(|i| (i, Record { id: i, data: format!("Updated {i}") })).collect();

        let update_refs: Vec<(&u32, &Record)> = updates.iter().map(|(k, v)| (k, v)).collect();

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
        let unpacked_size_estimate = u64::from(count) * u64::try_from(super::PAGE_SIZE_USIZE).unwrap();

        println!("File size with packing: {file_size} bytes");
        println!("Estimated unpacked size: {unpacked_size_estimate} bytes");

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

        let updates = [(1, val1.clone()), (2, val2.clone())];

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
        let updates =
            [(1, "First".to_string()), (1, "Second".to_string()), (2, "Two".to_string()), (1, "Third".to_string())];

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
        assert!(size_before > u64::from(count) * 4000);

        // Now Compact
        tree_update.compact(&filepath)?;
        drop(tree_update);

        let size_after = std::fs::metadata(&filepath)?.len();
        // 200 items * 100 bytes = 20KB payload.
        // Should pack into ~5-6 blocks (4KB each).

        println!("Size before: {size_before}, Size after: {size_after}");
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
    fn test_store_and_query_many_variable_string_keys() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_many_variable_string_keys.bin");

        let count = 40_000usize;
        let mut tree = BPlusTree::<String, String>::new();
        for i in 0..count {
            let key_suffix_len = 10 + (i % 97);
            let key = format!("k{i:06}_{}", "x".repeat(key_suffix_len));
            tree.insert(key, format!("v{i:06}"));
        }

        tree.store(&filepath)?;
        drop(tree);

        let mut query = BPlusTreeQuery::<String, String>::try_new(&filepath)?;
        for i in (0..count).step_by(17) {
            let key_suffix_len = 10 + (i % 97);
            let key = format!("k{i:06}_{}", "x".repeat(key_suffix_len));
            assert_eq!(query.query(&key).map_err(super::BPlusTreeError::to_io)?, Some(format!("v{i:06}")));
        }

        Ok(())
    }

    #[test]
    fn test_node_serialization_overhead() -> io::Result<()> {
        use crate::{
            repository::bplustree::{ValueInfo, ValueStorageMode, PAGE_SIZE_USIZE},
            utils::binary_serialize,
        };

        // Simulate a leaf node with u32 keys and ValueInfo
        let key_counts = [10, 30, 50, 80, 100];

        for count in key_counts {
            let keys: Vec<u32> = (0..count).collect();
            let value_info: Vec<ValueInfo> = (0..count)
                .map(|i| ValueInfo {
                    mode: ValueStorageMode::Packed(u64::from(i) * 4096, (i % 16) as u16),
                    length: 100,
                    cache: Mutex::new(None),
                })
                .collect();

            let keys_serialized = binary_serialize(&keys)?;
            let info_serialized = binary_serialize(&value_info)?;

            // Total content: flag(1) + keys_len(4) + keys + info_len(4) + info
            let total = 1 + 4 + keys_serialized.len() + 4 + info_serialized.len();
            let fits_in_block = total <= PAGE_SIZE_USIZE;

            println!(
                "Keys={}: keys_bytes={}, info_bytes={}, total={}, fits_in_block={}",
                count,
                keys_serialized.len(),
                info_serialized.len(),
                total,
                fits_in_block
            );
        }
        Ok(())
    }

    #[test]
    fn test_internal_node_size_estimate_never_underestimates_msgpack_pointers() -> io::Result<()> {
        // Many children force pointer encoding overhead to dominate.
        // This guards against size underestimation in internal-node layout.
        let key_count = 599usize;
        let mut node = BPlusTreeNode::<String, u32>::new(false);
        node.keys = (0..key_count).map(|i| format!("k{i:04}")).collect();
        node.children = (0..=key_count).map(|_| BPlusTreeNode::<String, u32>::new(true)).collect();

        let mut serial_buf = Vec::new();
        let estimated_size = node.calculate_serialized_size(&mut serial_buf)?;

        let mut file = tempfile::tempfile()?;
        let mut buffer = Vec::new();
        let base = u64::from(u32::MAX) + 1_000_000;
        let child_offsets: Vec<u64> = (0..node.children.len()).map(|i| base + (i as u64) * 10_000).collect();
        let actual_size =
            node.serialize_internal_with_offsets(&mut file, &mut buffer, &mut serial_buf, 0, &child_offsets)?;

        assert!(
            estimated_size >= actual_size,
            "internal node size estimate underflowed: estimated={estimated_size}, actual={actual_size}"
        );
        Ok(())
    }

    /// Test that packed value updates work correctly:
    /// - Same-size updates happen in-place within the packed block
    /// - Different-size updates promote the value to Single storage mode
    #[test]
    fn test_packed_value_update() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_packed_update.bin");

        // Create and store tree with small values that will be packed
        let mut tree = BPlusTree::<u32, String>::new();

        // Insert 50 small values (< 256 bytes) that will use packed storage
        let small_val = "x".repeat(50); // 50 bytes, well under SMALL_VALUE_THRESHOLD
        for i in 0..50 {
            tree.insert(i, small_val.clone());
        }

        tree.store(&filepath)?;
        drop(tree);

        // Get initial file size
        let size_initial = std::fs::metadata(&filepath)?.len();

        // Open for update
        let mut tree_update = BPlusTreeUpdate::<u32, String>::try_new(&filepath)?;

        // Test 1: Same-size update (should update in-place within packed block)
        let same_size_val = "y".repeat(50); // Same size as original
        let refs1 = [(&5u32, &same_size_val)];
        tree_update.update_batch(&refs1).map_err(super::BPlusTreeError::to_io)?;

        // Test 2: Different-size update (should promote to Single storage)
        let larger_val = "z".repeat(100); // Larger than original
        let refs2 = [(&10u32, &larger_val)];
        tree_update.update_batch(&refs2).map_err(super::BPlusTreeError::to_io)?;

        // Test 3: Smaller-size update (should promote to Single storage due to size mismatch)
        let smaller_val = "w".repeat(30); // Smaller than original
        let refs3 = [(&15u32, &smaller_val)];
        tree_update.update_batch(&refs3).map_err(super::BPlusTreeError::to_io)?;

        drop(tree_update);

        // Get file size after updates
        let size_after = std::fs::metadata(&filepath)?.len();

        // File size should have grown slightly (promoted values written at EOF)
        // but not dramatically since most values are still packed
        println!("Size initial: {size_initial}, Size after: {size_after}");
        assert!(size_after >= size_initial, "File should not shrink");

        // Verify all data is correct
        let mut query = BPlusTreeQuery::<u32, String>::try_new(&filepath)?;

        // Updated values
        assert_eq!(query.query(&5).unwrap(), Some(same_size_val.clone()), "Same-size update failed");
        assert_eq!(query.query(&10).unwrap(), Some(larger_val.clone()), "Larger-size update failed");
        assert_eq!(query.query(&15).unwrap(), Some(smaller_val.clone()), "Smaller-size update failed");

        // Unchanged values
        assert_eq!(query.query(&0).unwrap(), Some(small_val.clone()), "Unchanged value 0 incorrect");
        assert_eq!(query.query(&20).unwrap(), Some(small_val.clone()), "Unchanged value 20 incorrect");
        assert_eq!(query.query(&49).unwrap(), Some(small_val.clone()), "Unchanged value 49 incorrect");

        Ok(())
    }

    #[test]
    fn test_flush_policy() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_flush.bin");

        let mut tree = BPlusTree::<u32, String>::new();
        for i in 0..10 {
            tree.insert(i, format!("value_{i}"));
        }
        tree.store(&filepath)?;

        let mut update = BPlusTreeUpdate::<u32, String>::try_new(&filepath)?;

        // Test None policy - should not error
        update.flush_policy = super::FlushPolicy::None;
        for i in 0..5 {
            update.update(&i, format!("new_{i}")).map_err(super::BPlusTreeError::to_io)?;
        }

        // Verify values within same session
        for i in 0..5 {
            assert_eq!(update.query(&i).map_err(super::BPlusTreeError::to_io)?.unwrap(), format!("new_{i}"));
        }

        // Test Batch policy
        update.flush_policy = super::FlushPolicy::Batch;
        let batch = [(&5u32, &"batch_5".to_string()), (&6u32, &"batch_6".to_string())];
        update.update_batch(&batch).map_err(super::BPlusTreeError::to_io)?;

        assert_eq!(update.query(&5).map_err(super::BPlusTreeError::to_io)?.unwrap(), "batch_5");

        Ok(())
    }

    #[test]
    fn serial_writer_batch_marks_dirty_and_commit_clears_dirty() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("serial_writer_dirty.bin");

        let mut tree = BPlusTree::<u32, String>::new();
        tree.store(&filepath)?;

        let writer = BPlusTreeSerialWriter::<u32, String>::new(&filepath, FlushPolicy::Batch)?;
        let key = 7u32;
        let value = "value_7".to_string();
        writer.upsert(&[(&key, &value)])?;
        assert!(writer.dirty.load(Ordering::Acquire));

        writer.commit()?;
        assert!(!writer.dirty.load(Ordering::Acquire));

        writer.shutdown()?;
        Ok(())
    }

    #[test]
    fn serial_writer_background_commit_requires_batch_policy() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("serial_writer_bg_policy.bin");

        let mut tree = BPlusTree::<u32, String>::new();
        tree.store(&filepath)?;

        let writer = BPlusTreeSerialWriter::<u32, String>::new(&filepath, FlushPolicy::Immediate)?;
        let result = writer.start_background_commit(Duration::from_millis(10));
        assert!(result.is_err());

        Ok(())
    }

    #[test]
    fn serial_writer_background_commit_flushes_batch_writes() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("serial_writer_bg_flush.bin");

        let mut tree = BPlusTree::<u32, String>::new();
        tree.store(&filepath)?;

        let writer = BPlusTreeSerialWriter::<u32, String>::new(&filepath, FlushPolicy::Batch)?;
        writer.start_background_commit(Duration::from_millis(10))?;

        let key = 3u32;
        let value = "batch_value".to_string();
        writer.upsert(&[(&key, &value)])?;
        assert!(writer.dirty.load(Ordering::Acquire));

        for _ in 0..40 {
            if !writer.dirty.load(Ordering::Acquire) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!writer.dirty.load(Ordering::Acquire));

        writer.shutdown()?;
        Ok(())
    }

    #[test]
    fn test_cache_population_and_reuse() -> io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_cache.bin");

        // Create tree with mixed storage
        let mut tree = BPlusTree::<u32, String>::new();
        // 1. Single storage (large-ish value)
        let large_val = "A".repeat(500);
        tree.insert(1, large_val.clone());
        // 2. Packed storage (small value)
        let small_val = "B".repeat(20);
        tree.insert(2, small_val.clone());

        tree.store(&filepath)?;

        let mut update = BPlusTreeUpdate::<u32, String>::try_new(&filepath)?;

        // --- Single Storage Cache ---
        // First read populates cache
        let val1 = update.query(&1).map_err(super::BPlusTreeError::to_io)?.unwrap();
        assert_eq!(val1, large_val);

        // Subsequent update of DIFFERENT key should not affect key 1's cache
        let _ = update.update(&10, "unrelated".into()); // Might error if 10 not found, but we check cache reuse

        // Update key 1 with SAME SIZE (should use/populate cache)
        let large_val_2 = "C".repeat(500);
        update.update(&1, large_val_2.clone()).map_err(super::BPlusTreeError::to_io)?;
        assert_eq!(update.query(&1).map_err(super::BPlusTreeError::to_io)?.unwrap(), large_val_2);

        // --- Packed Storage Cache ---
        // First read of key 2 populates PackedOffset cache
        let _ = update.query(&2).map_err(super::BPlusTreeError::to_io)?;

        // Update key 2 with SAME SIZE (should use Cached Offset)
        let small_val_2 = "D".repeat(20);
        update.update(&2, small_val_2.clone()).map_err(super::BPlusTreeError::to_io)?;

        // Verify update
        assert_eq!(update.query(&2).map_err(super::BPlusTreeError::to_io)?.unwrap(), small_val_2);

        Ok(())
    }

    #[test]
    fn test_concurrent_cache_access() -> io::Result<()> {
        use std::{sync::Arc, thread};

        let tempdir = tempfile::tempdir()?;
        let filepath = tempdir.path().join("tree_concurrent.bin");

        let mut tree = BPlusTree::<u32, String>::new();
        let val = "concurrent_test_value".to_string();
        for i in 0..100 {
            tree.insert(i, val.clone());
        }
        tree.store(&filepath)?;

        // We test concurrent READS on BPlusTreeQuery.
        // Note: BPlusTreeQuery::query requires &mut self for buffer management,
        // so we use a Mutex to protect it. While this serializes the query() calls,
        // it still tests thread-safety of the shared ValueInfo Mutexes internally
        // if they were somehow shared (though they currently aren't).
        let query = Arc::new(parking_lot::Mutex::new(super::BPlusTreeQuery::<u32, String>::try_new(&filepath)?));

        let mut handles = Vec::new();
        for _t in 0..10 {
            let q = Arc::clone(&query);
            let v = val.clone();
            let handle = thread::spawn(move || {
                for i in 0..100 {
                    let mut guard = q.lock();
                    let res = guard.query(&i).expect("Query failed");
                    assert_eq!(res, Some(v.clone()));
                }
            });
            handles.push(handle);
        }

        for h in handles {
            h.join().expect("Thread panicked");
        }

        Ok(())
    }

    #[test]
    fn test_page_initialization() {
        let mut data = [0u8; PAGE_SIZE_USIZE];
        let page = SlottedPage::new(&mut data, PageType::Leaf).expect("Init failed");
        assert_eq!(page.header.page_type, PageType::Leaf);
        assert_eq!(page.header.cell_count, 0);
        assert_eq!(page.header.free_start, PAGE_HEADER_SIZE);
        assert_eq!(page.header.free_end, PAGE_SIZE);
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
        cell1.extend_from_slice(&u32::try_from(val1.len()).unwrap().to_le_bytes());
        cell1.extend_from_slice(val1);

        let mut cell2 = Vec::new();
        cell2.extend_from_slice(&u32::try_from(val2.len()).unwrap().to_le_bytes());
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
        cell.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
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
        cell.extend_from_slice(&u32::try_from(val.len()).unwrap().to_le_bytes());
        cell.extend_from_slice(val);
        page.insert_at_index(0, &cell).unwrap();

        let res = page.split_off();
        match res {
            Ok(None) => {
                assert_eq!(page.header.cell_count, 1); // Original page untouched
            }
            Ok(Some(_)) => panic!("Split of single item should result in None"),
            Err(e) => panic!("Split of single item should result in no-op, not error: {e:?}"),
        }
    }

    #[test]
    fn small_tree_test() -> io::Result<()> {
        let tempdir = tempdir()?;
        let filepath = tempdir.path().join("small_tree.bin");

        let mut tree = BPlusTree::<String, String>::new();
        tree.insert("key1".to_string(), "val1".to_string());
        tree.insert("key2".to_string(), "val2".to_string());

        assert_eq!(tree.len(), 2);

        tree.store(&filepath)?;

        let mut update = BPlusTreeUpdate::<String, String>::try_new(&filepath)?;
        assert_eq!(update.len().unwrap(), 2);

        let res1 = update.query(&"key1".to_string()).unwrap();
        assert_eq!(res1, Some("val1".to_string()));

        let res2 = update.query(&"key2".to_string()).unwrap();
        assert_eq!(res2, Some("val2".to_string()));

        Ok(())
    }

    #[test]
    fn test_metadata() -> io::Result<()> {
        let tempdir = tempdir()?;
        let filepath = tempdir.path().join("metadata_test.bin");

        // 1. Test in-memory BPlusTree metadata
        let mut tree = BPlusTree::<u32, String>::new();
        assert!(matches!(tree.get_metadata(), BPlusTreeMetadata::Empty));

        let meta = BPlusTreeMetadata::TargetIdMapping(12345);
        tree.set_metadata(meta.clone());
        assert_eq!(tree.get_metadata(), &meta);

        // 2. Persist and check reloaded metadata
        tree.store(&filepath)?;
        let loaded = BPlusTree::<u32, String>::load(&filepath)?;
        assert_eq!(loaded.get_metadata(), &meta);
        drop(loaded);

        // 3. Test BPlusTreeUpdate metadata
        let mut update = BPlusTreeUpdate::<u32, String>::try_new(&filepath)?;
        assert_eq!(update.get_metadata()?, meta);

        let new_meta = BPlusTreeMetadata::TargetIdMapping(67890);
        update.set_metadata(&new_meta)?;
        assert_eq!(update.get_metadata()?, new_meta);
        drop(update);

        // Reload and verify again
        let loaded2 = BPlusTree::<u32, String>::load(&filepath)?;
        assert_eq!(loaded2.get_metadata(), &new_meta);

        Ok(())
    }

    #[test]
    fn test_query_zero_copy() -> io::Result<()> {
        let tempdir = tempdir()?;
        let filepath = tempdir.path().join("zero_copy_test.bin");

        let mut tree = BPlusTree::<u32, String>::new();
        for i in 0..100 {
            tree.insert(i, format!("value_{i}"));
        }
        tree.store(&filepath)?;

        let mut query = BPlusTreeQuery::<u32, String>::try_new(&filepath)?;
        for i in 0..100 {
            let res = query.query_zero_copy(&i).expect("Zero copy query failed");
            assert_eq!(res, Some(format!("value_{i}")));
        }

        // Test key not found
        assert_eq!(query.query_zero_copy(&101).unwrap(), None);

        Ok(())
    }

    #[test]
    fn test_query_refreshes_root_offset_after_external_upsert() -> io::Result<()> {
        let tempdir = tempdir()?;
        let filepath = tempdir.path().join("query_refresh_root.bin");

        let mut tree = BPlusTree::<u32, String>::new();
        for i in 0..64 {
            tree.insert(i, format!("value_{i}"));
        }
        tree.store(&filepath)?;
        drop(tree);

        let mut query = BPlusTreeQuery::<u32, String>::try_new(&filepath)?;
        assert_eq!(query.query(&999).map_err(BPlusTreeError::to_io)?, None);

        let mut updater = BPlusTreeUpdate::<u32, String>::try_new(&filepath)?;
        let inserted = "fresh_value".to_string();
        updater.upsert_batch(&[(&999u32, &inserted)])?;
        drop(updater);

        // Same query instance should observe the updated root/header.
        assert_eq!(query.query(&999).map_err(BPlusTreeError::to_io)?, Some(inserted.clone()));
        assert_eq!(query.query_zero_copy(&999).map_err(BPlusTreeError::to_io)?, Some(inserted));

        Ok(())
    }

    #[test]
    fn test_query_le_query_struct() -> io::Result<()> {
        let tempdir = tempdir()?;
        let filepath = tempdir.path().join("query_le_test.bin");

        let mut tree = BPlusTree::<u32, String>::new();
        for i in (0..100).step_by(10) {
            tree.insert(i, format!("val_{i}"));
        }
        tree.store(&filepath)?;

        let mut query = BPlusTreeQuery::<u32, String>::try_new(&filepath)?;

        // Exact match
        assert_eq!(query.query_le(&20).unwrap().unwrap(), "val_20");

        // In gap
        assert_eq!(query.query_le(&25).unwrap().unwrap(), "val_20");

        // Before all
        assert_eq!(query.query_le(&0).unwrap().unwrap(), "val_0");

        // After all
        assert_eq!(query.query_le(&1000).unwrap().unwrap(), "val_90");

        Ok(())
    }

    #[test]
    fn test_delete_batch_hides_entries_from_query_iter_and_len() -> io::Result<()> {
        let tempdir = tempdir()?;
        let filepath = tempdir.path().join("delete_tombstone_visibility.bin");

        let mut tree = BPlusTree::<u32, String>::new();
        for i in 0..240u32 {
            tree.insert(i, format!("val_{i}"));
        }
        tree.store(&filepath)?;
        drop(tree);

        let mut updater = BPlusTreeUpdate::<u32, String>::try_new(&filepath)?;
        let deleted_keys: Vec<u32> = (0..240u32).filter(|k| k % 3 == 0).collect();
        let delete_refs: Vec<&u32> = deleted_keys.iter().collect();
        let deleted = updater.delete_batch(&delete_refs)?;
        assert_eq!(deleted, deleted_keys.len());
        assert_eq!(updater.len().map_err(BPlusTreeError::to_io)?, 240usize - deleted_keys.len());
        assert_eq!(updater.query(&0).map_err(BPlusTreeError::to_io)?, None);
        assert_eq!(updater.query(&1).map_err(BPlusTreeError::to_io)?, Some("val_1".to_string()));
        drop(updater);

        let mut query = BPlusTreeQuery::<u32, String>::try_new(&filepath)?;
        assert_eq!(query.len().map_err(BPlusTreeError::to_io)?, 240usize - deleted_keys.len());

        let iter_keys: Vec<u32> = query.iter().map(|(k, _)| k).collect();
        assert_eq!(iter_keys.len(), 240usize - deleted_keys.len());
        for key in iter_keys {
            assert_ne!(key % 3, 0);
        }

        Ok(())
    }

    #[test]
    fn test_sorted_index_iterator_skips_tombstoned_entries_without_rebuild() -> io::Result<()> {
        let tempdir = tempdir()?;
        let filepath = tempdir.path().join("sorted_index_tombstone_skip.bin");

        let mut tree = BPlusTree::<u32, Record>::new();
        for i in 0..120u32 {
            tree.insert(i, Record { id: i, data: format!("val_{i}") });
        }
        tree.store_with_index(&filepath, |record| record.id)?;
        drop(tree);

        let deleted_keys: Vec<u32> = (0..120u32).filter(|key| key % 4 == 0).collect();
        let delete_refs: Vec<&u32> = deleted_keys.iter().collect();
        let mut updater = BPlusTreeUpdate::<u32, Record>::try_new(&filepath)?;
        let deleted = updater.delete_batch(&delete_refs)?;
        assert_eq!(deleted, deleted_keys.len());
        drop(updater);

        let query = BPlusTreeQuery::<u32, Record>::try_new(&filepath)?;
        let sorted_iter = query.disk_iter_sorted::<u32>()?;
        let sorted_keys: Vec<u32> =
            sorted_iter.map(|entry| entry.map(|(key, _)| key)).collect::<io::Result<Vec<u32>>>()?;

        assert_eq!(sorted_keys.len(), 120usize - deleted_keys.len());
        for key in &sorted_keys {
            assert_ne!(key % 4, 0, "tombstoned key must be skipped in sorted iterator");
        }

        Ok(())
    }

    #[test]
    fn test_tombstone_header_flag_transitions_on_delete_and_compact() -> io::Result<()> {
        let tempdir = tempdir()?;
        let filepath = tempdir.path().join("tombstone_header_flag.bin");

        let mut tree = BPlusTree::<u32, String>::new();
        for i in 0..64u32 {
            tree.insert(i, format!("val_{i}"));
        }
        tree.store(&filepath)?;
        drop(tree);

        let query_before = BPlusTreeQuery::<u32, String>::try_new(&filepath)?;
        assert!(!query_before.has_tombstones());

        let mut updater = BPlusTreeUpdate::<u32, String>::try_new(&filepath)?;
        let delete_key = 3u32;
        let deleted = updater.delete(&delete_key)?;
        assert!(deleted);
        drop(updater);

        let query_after_delete = BPlusTreeQuery::<u32, String>::try_new(&filepath)?;
        assert!(query_after_delete.has_tombstones());

        let mut updater_for_compact = BPlusTreeUpdate::<u32, String>::try_new(&filepath)?;
        updater_for_compact.compact(&filepath)?;
        drop(updater_for_compact);

        let query_after_compact = BPlusTreeQuery::<u32, String>::try_new(&filepath)?;
        assert!(!query_after_compact.has_tombstones());

        Ok(())
    }

    #[test]
    fn test_query_le_skips_tombstones_across_leaves() -> io::Result<()> {
        let tempdir = tempdir()?;
        let filepath = tempdir.path().join("query_le_tombstone.bin");

        let mut tree = BPlusTree::<u32, String>::new();
        for i in 0..640u32 {
            tree.insert(i, format!("val_{i}"));
        }
        tree.store(&filepath)?;
        drop(tree);

        let mut updater = BPlusTreeUpdate::<u32, String>::try_new(&filepath)?;
        let deleted_keys: Vec<u32> = (0..640u32).filter(|k| k % 5 == 0).collect();
        let delete_refs: Vec<&u32> = deleted_keys.iter().collect();
        let deleted = updater.delete_batch(&delete_refs)?;
        assert_eq!(deleted, deleted_keys.len());
        drop(updater);

        let mut query = BPlusTreeQuery::<u32, String>::try_new(&filepath)?;
        for q in 0..640u32 {
            let mut expected_key: Option<u32> = None;
            for candidate in (0..=q).rev() {
                if candidate % 5 != 0 {
                    expected_key = Some(candidate);
                    break;
                }
            }
            let expected_value = expected_key.map(|candidate| format!("val_{candidate}"));
            let actual = query.query_le(&q).map_err(BPlusTreeError::to_io)?;
            assert_eq!(actual, expected_value, "query_le mismatch for key {q}");
        }

        assert_eq!(query.query_le(&639).map_err(BPlusTreeError::to_io)?, Some("val_639".to_string()));
        assert_eq!(query.query_le(&640).map_err(BPlusTreeError::to_io)?, Some("val_639".to_string()));
        assert_eq!(query.query_le(&0).map_err(BPlusTreeError::to_io)?, None);

        Ok(())
    }

    #[test]
    fn test_load_skips_tombstones_without_error() -> io::Result<()> {
        let tempdir = tempdir()?;
        let filepath = tempdir.path().join("load_tombstones.bin");

        let mut tree = BPlusTree::<u32, String>::new();
        for i in 0..512u32 {
            tree.insert(i, format!("val_{i}"));
        }
        tree.store(&filepath)?;
        drop(tree);

        let mut updater = BPlusTreeUpdate::<u32, String>::try_new(&filepath)?;
        let deleted_keys: Vec<u32> = (0..512u32).filter(|k| k % 4 == 0).collect();
        let delete_refs: Vec<&u32> = deleted_keys.iter().collect();
        let deleted = updater.delete_batch(&delete_refs)?;
        assert_eq!(deleted, deleted_keys.len());
        drop(updater);

        let loaded = BPlusTree::<u32, String>::load(&filepath)?;
        let expected_live = 512usize - deleted_keys.len();
        assert_eq!(loaded.len(), expected_live);

        assert!(loaded.query(&0).is_none());
        assert_eq!(loaded.query(&1).cloned(), Some("val_1".to_string()));

        let le_key = loaded.find_le(&4).map(|(k, _)| *k);
        assert_eq!(le_key, Some(3));

        let mut seen = 0usize;
        for (key, value) in &loaded {
            assert_ne!(*key % 4, 0);
            assert_eq!(value, &format!("val_{key}"));
            seen += 1;
        }
        assert_eq!(seen, expected_live);

        Ok(())
    }

    #[test]
    fn test_load_non_existent_errors() {
        let tempdir = tempdir().unwrap();
        let filepath = tempdir.path().join("non_existent_file.bin");

        // Load non-existent
        let res_load = BPlusTree::<u32, String>::load(&filepath);
        assert!(res_load.is_err());

        // Update try_new non-existent
        let res_update = BPlusTreeUpdate::<u32, String>::try_new(&filepath);
        assert!(res_update.is_err());

        // Query try_new non-existent
        let res_query = BPlusTreeQuery::<u32, String>::try_new(&filepath);
        assert!(res_query.is_err());
    }

    #[test]
    fn test_update_key_not_found_error() -> io::Result<()> {
        let tempdir = tempdir()?;
        let filepath = tempdir.path().join("not_found_err.bin");

        let mut tree = BPlusTree::<u32, String>::new();
        tree.insert(1, "one".into());
        tree.store(&filepath)?;

        let mut update = BPlusTreeUpdate::<u32, String>::try_new(&filepath)?;
        let res = update.update(&2, "two".into());
        assert!(matches!(res, Err(BPlusTreeError::KeyNotFound)));

        Ok(())
    }

    #[test]
    fn test_empty_tree_operations() -> io::Result<()> {
        let tempdir = tempdir()?;
        let filepath = tempdir.path().join("empty_tree_ops.bin");

        let mut tree = BPlusTree::<u32, String>::new();
        tree.store(&filepath)?;

        let mut query = BPlusTreeQuery::<u32, String>::try_new(&filepath)?;
        assert!(query.is_empty().unwrap());
        assert_eq!(query.len().unwrap(), 0);
        assert_eq!(query.query(&1).unwrap(), None);

        let mut update = BPlusTreeUpdate::<u32, String>::try_new(&filepath)?;
        assert!(update.is_empty().unwrap());
        assert_eq!(update.len().unwrap(), 0);

        Ok(())
    }

    #[test]
    fn test_query_zero_copy_string() -> io::Result<()> {
        let tempdir = tempdir()?;
        let filepath = tempdir.path().join("zero_copy_string.bin");

        let mut tree = BPlusTree::<String, u32>::new();
        for i in 0..50 {
            tree.insert(format!("key_{i:03}"), i);
        }
        tree.store(&filepath)?;

        let mut query = BPlusTreeQuery::<String, u32>::try_new(&filepath)?;
        for i in 0..50 {
            let k = format!("key_{i:03}");
            let res = query.query_zero_copy(&k).expect("Zero copy query failed");
            assert_eq!(res, Some(i));
        }

        assert_eq!(query.query_zero_copy(&"key_051".to_string()).unwrap(), None);

        Ok(())
    }

    #[test]
    fn test_slotted_page_compact_manual() {
        let mut data = [0u8; PAGE_SIZE_USIZE];
        let mut page = SlottedPage::new(&mut data, PageType::Leaf).expect("Init failed");

        let cell1 = vec![0x01; 100];
        let cell2 = vec![0x02; 100];
        let cell3 = vec![0x03; 100];

        let mut c1 = Vec::new();
        c1.extend_from_slice(&100u32.to_le_bytes());
        c1.extend_from_slice(&cell1);

        let mut c2 = Vec::new();
        c2.extend_from_slice(&100u32.to_le_bytes());
        c2.extend_from_slice(&cell2);

        let mut c3 = Vec::new();
        c3.extend_from_slice(&100u32.to_le_bytes());
        c3.extend_from_slice(&cell3);

        page.insert_at_index(0, &c1).unwrap();
        page.insert_at_index(1, &c2).unwrap();
        page.insert_at_index(2, &c3).unwrap();

        // Compacting a non-fragmented page should be fine
        page.compact().expect("Compact failed");
        assert_eq!(page.header.cell_count, 3);
        assert_eq!(&page.get_cell(0).unwrap()[4..], &cell1);
        assert_eq!(&page.get_cell(1).unwrap()[4..], &cell2);
        assert_eq!(&page.get_cell(2).unwrap()[4..], &cell3);
    }

    #[test]
    fn test_upsert_batch_preserialized() -> io::Result<()> {
        let tempdir = tempdir()?;
        let filepath = tempdir.path().join("preserialized_test.bin");

        let mut tree = BPlusTree::<u32, Record>::new();
        tree.store(&filepath)?;

        let mut update = BPlusTreeUpdate::<u32, Record>::try_new(&filepath)?;

        // Manually serialize records
        let r1 = Record { id: 1, data: "preserialized_1".to_string() };
        let r2 = Record { id: 2, data: "preserialized_2".to_string() };

        let r1_bytes = binary_serialize(&r1)?;
        let r2_bytes = binary_serialize(&r2)?;

        update.upsert_batch_preserialized(vec![(1, r1_bytes), (2, r2_bytes)])?;

        // Verify with query
        let mut query = BPlusTreeQuery::<u32, Record>::try_new(&filepath)?;
        assert_eq!(query.query(&1).unwrap(), Some(r1));
        assert_eq!(query.query(&2).unwrap(), Some(r2));

        // Test mixed: existing and new
        let r1_new = Record { id: 1, data: "updated_preserialized_1".to_string() };
        let r3 = Record { id: 3, data: "new_preserialized_3".to_string() };

        let r1_new_bytes = binary_serialize(&r1_new)?;
        let r3_bytes = binary_serialize(&r3)?;

        update.upsert_batch_preserialized(vec![(1, r1_new_bytes), (3, r3_bytes)])?;

        let mut query2 = BPlusTreeQuery::<u32, Record>::try_new(&filepath)?;
        assert_eq!(query2.query(&1).unwrap(), Some(r1_new));
        assert_eq!(query2.query(&2).unwrap(), Some(Record { id: 2, data: "preserialized_2".to_string() }));
        assert_eq!(query2.query(&3).unwrap(), Some(r3));

        Ok(())
    }
}

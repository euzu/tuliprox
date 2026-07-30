//! Legacy-compatible B+Tree storage v2.
//!
//! This module remains the active compatibility branch for existing v2 files.
//! Stabilization work must preserve `STORAGE_VERSION = 2` and avoid requiring a
//! rewrite of existing repositories. The future v3 storage line is expected to
//! live behind an explicit version boundary and a typed migration path.

use crate::{
    repository::bplustree::common::{mmap_with_advice, read_exact_at_offset, Advice},
    utils,
    utils::binary_deserialize,
};
pub(crate) use crate::repository::bplustree::common::BPlusTreeError;
#[cfg(test)]
use crate::utils::binary_serialize_into;
#[cfg(test)]
use log::error;
use memmap2::Mmap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use shared::error::to_io_error;
#[cfg(test)]
use shared::error::string_to_io_error;
use smallvec::{smallvec, SmallVec};
use std::{
    collections::HashSet,
    fs::File,
    io::{self, BufReader, Read, Seek, SeekFrom},
    marker::PhantomData,
    ops::Bound,
    path::Path,
};
#[cfg(test)]
use std::{
    borrow::Cow,
    io::Write,
};
#[cfg(test)]
use tempfile::NamedTempFile;
const PAGE_SIZE: u16 = 4096;
pub const PAGE_SIZE_USIZE: usize = PAGE_SIZE as usize;
const LEN_SIZE: usize = 4;
const FLAG_SIZE: usize = 1;
pub(crate) const MAGIC: &[u8; 4] = b"BTRE";
pub(crate) const STORAGE_VERSION: u32 = 2;
const HEADER_SIZE: u64 = PAGE_SIZE as u64;
#[cfg(test)]
const ROOT_OFFSET_POS: u64 = 8;
const METADATA_DATA_START_POS: usize = 20;
// Reserve space for metadata (e.g. 4096 - 16 = 4080 bytes max, but let's be safe)
const METADATA_MAX_SIZE: usize = 4000;
const HEADER_FLAG_HAS_METADATA_FLAGS: u32 = 1 << 31;
const HEADER_FLAG_HAS_TOMBSTONES: u32 = 1 << 30;
const HEADER_METADATA_LEN_MASK: u32 = !(HEADER_FLAG_HAS_METADATA_FLAGS | HEADER_FLAG_HAS_TOMBSTONES);

#[inline]
#[cfg(test)]
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

// v2 uses conservative runtime fanout instead of pretending that size_of::<K>()
// predicts serialized key size. Multi-block nodes keep existing files compatible.
#[cfg(test)]
const DEFAULT_INNER_ORDER: usize = 64;
#[cfg(test)]
const DEFAULT_LEAF_ORDER: usize = 64;

// Value packing configuration
#[cfg(test)]
const SMALL_VALUE_THRESHOLD: usize = 256;
#[cfg(test)]
const PACK_BLOCK_HEADER_SIZE: usize = 4;
#[cfg(test)]
const PACK_VALUE_HEADER_SIZE: usize = 4;

// LZ4 compression configuration
#[cfg(test)]
const COMPRESSION_MIN_SIZE: usize = 64;
#[cfg(test)]
const COMPRESSION_THRESHOLD_PERCENT: usize = 85;
#[cfg(test)]
const COMPRESSION_FLAG_NONE: u8 = 0x00;
pub const COMPRESSION_FLAG_LZ4: u8 = 0x01;

#[cfg(test)]
const MAGIC_METADATA_TARGET_ID_MAPPING: u8 = 0x01;

type TraversalStack = SmallVec<[(u64, usize); 8]>;

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

#[inline]
fn u32_from_bytes(bytes: &[u8]) -> io::Result<u32> { Ok(u32::from_le_bytes(bytes.try_into().map_err(to_io_error)?)) }

#[inline]
fn node_flag_to_is_leaf(flag: u8) -> io::Result<bool> {
    match flag {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, format!("Invalid B+Tree node flag: {flag}"))),
    }
}

#[inline]
fn checked_slice_range(start: usize, len: usize, total_len: usize) -> io::Result<std::ops::Range<usize>> {
    let end = start
        .checked_add(len)
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "B+Tree node slice offset overflow"))?;
    if end > total_len {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "B+Tree node slice out of bounds"));
    }
    Ok(start..end)
}

fn valid_internal_pointer_count(key_count: usize, pointer_count: usize) -> bool {
    pointer_count != 0
        && (key_count.checked_add(1) == Some(pointer_count) || (key_count != 0 && key_count == pointer_count))
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

#[inline]
#[cfg(test)]
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
#[cfg(test)]
const fn msgpack_u64_array_upper_bound_len(count: usize) -> usize {
    // Worst-case per u64: marker + 8 bytes payload.
    msgpack_array_header_len(count) + count.saturating_mul(9)
}

// Adaptively compress value bytes if beneficial.
// Returns borrowed raw bytes when compression is not useful to avoid an
// allocation on the common uncompressed path.
#[cfg(test)]
fn compress_if_beneficial(raw_bytes: &[u8]) -> (u8, Cow<'_, [u8]>) {
    if raw_bytes.len() >= COMPRESSION_MIN_SIZE {
        let compressed = lz4_flex::compress_prepend_size(raw_bytes);
        let threshold = (raw_bytes.len() * COMPRESSION_THRESHOLD_PERCENT) / 100;

        if compressed.len() < threshold {
            // Compression is effective
            (COMPRESSION_FLAG_LZ4, Cow::Owned(compressed))
        } else {
            // Compression not worth it - return borrowed raw bytes.
            (COMPRESSION_FLAG_NONE, Cow::Borrowed(raw_bytes))
        }
    } else {
        // Too small to compress - return borrowed raw bytes.
        (COMPRESSION_FLAG_NONE, Cow::Borrowed(raw_bytes))
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

#[derive(Debug, Clone)]
struct BPlusTreeNode<K, V> {
    keys: Vec<K>,
    #[cfg_attr(not(test), allow(dead_code))]
    children: Vec<BPlusTreeNode<K, V>>,
    is_leaf: bool,
    value_info: Vec<ValueInfo>,
    #[cfg_attr(not(test), allow(dead_code))]
    values: Vec<V>, // only used in leaf nodes
}

impl<K, V> BPlusTreeNode<K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    #[inline]
    #[cfg(test)]
    const fn new(is_leaf: bool) -> Self {
        Self { is_leaf, keys: vec![], children: vec![], value_info: vec![], values: vec![] }
    }

    #[inline]
    #[cfg(test)]
    fn is_overflow(&self, order: usize) -> bool { self.keys.len() > order }

    #[inline]
    #[cfg(test)]
    const fn get_median_index(order: usize) -> usize { order >> 1 }

    #[cfg(test)]
    fn find_leaf_entry(node: &Self) -> Option<&K> {
        if node.is_leaf {
            node.keys.first()
        } else if let Some(child) = node.children.first() {
            Self::find_leaf_entry(child)
        } else {
            None
        }
    }

    #[cfg(test)]
    fn get_entry_index_upper_bound(&self, key: &K) -> usize { get_entry_index_upper_bound::<K>(&self.keys, key) }

    #[cfg(test)]
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

    #[cfg(test)]
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
            // Internal keys are separators for children[1..]. The median key
            // separates the two split nodes and is represented in the parent
            // by the first leaf key of the returned right node.
            let _separator = self.keys.pop();
            node
        }
    }

    #[cfg(test)]
    fn add_historical_fence_key(&mut self) -> bool {
        if self.is_leaf {
            return false;
        }
        for index in 0..self.children.len().saturating_sub(1) {
            let Some(fence) = self.keys.get(index).cloned() else {
                continue;
            };
            let child = &mut self.children[index];
            if !child.is_leaf && child.children.len() == child.keys.len().saturating_add(1) {
                child.keys.push(fence);
                return true;
            }
        }
        self.children.iter_mut().any(Self::add_historical_fence_key)
    }

    /// Write a packed value block to disk
    #[cfg(test)]
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
    #[cfg(test)]
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

    #[cfg(test)]
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
            let capacity = blocks * PAGE_SIZE_USIZE;
            debug_assert!(
                content_size <= capacity,
                "Leaf node content ({content_size}B) exceeds allocated capacity ({capacity}B)"
            );

            file.seek(SeekFrom::Start(offset))?;

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
            let actual_content = FLAG_SIZE + LEN_SIZE + keys_len as usize + LEN_SIZE + pointers_len as usize;
            debug_assert!(
                actual_content <= total_capacity,
                "Internal node content ({actual_content}B) exceeds allocated capacity ({total_capacity}B)"
            );
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
    #[cfg(test)]
    fn serialize_breadth_first<W: Write + Seek>(
        &mut self,
        file: &mut W,
        buffer: &mut Vec<u8>,
        start_offset: u64,
    ) -> io::Result<u64> {
        let mut serial_buf = Vec::with_capacity(PAGE_SIZE_USIZE);

        self.serialize_bfs_pass1_populate_value_info(&mut serial_buf)?;
        let (node_offsets, child_ids_by_node, current_offset) =
            self.serialize_bfs_pass2_calculate_offsets(&mut serial_buf, start_offset)?;
        self.serialize_bfs_pass3_assign_value_offsets(current_offset);
        self.serialize_bfs_pass4_write_nodes(file, buffer, &mut serial_buf, &node_offsets, &child_ids_by_node)?;
        self.serialize_bfs_pass5_write_values(file, buffer, &mut serial_buf)?;

        Ok(start_offset)
    }

    #[cfg(test)]
    fn serialize_bfs_pass1_populate_value_info(&mut self, serial_buf: &mut Vec<u8>) -> io::Result<()> {
        let mut current_level_mut = vec![self];
        while !current_level_mut.is_empty() {
            let mut next_level_mut = Vec::new();
            for node in current_level_mut {
                if node.is_leaf {
                    node.value_info.clear();
                    let mut serialized_values: Vec<Vec<u8>> = Vec::new();
                    for value in &node.values {
                        serial_buf.clear();
                        binary_serialize_into(serial_buf, value)?;
                        serialized_values.push(serial_buf.clone());
                    }

                    let mut current_pack_index: u16 = 0;
                    let mut current_pack_size = PACK_BLOCK_HEADER_SIZE;
                    let mut pack_count = 0u32;

                    for value_bytes in serialized_values {
                        let size = value_bytes.len();

                        if size <= SMALL_VALUE_THRESHOLD {
                            let entry_size = PACK_VALUE_HEADER_SIZE + size;

                            if current_pack_size + entry_size <= PAGE_SIZE_USIZE {
                                node.value_info.push(ValueInfo {
                                    mode: ValueStorageMode::Packed(u64::from(pack_count), current_pack_index),
                                    length: u32::try_from(size).map_err(to_io_error)?,
                                    cache: Mutex::new(None),
                                });
                                current_pack_index += 1;
                                current_pack_size += entry_size;
                            } else {
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
                            let (flag, payload) = compress_if_beneficial(&value_bytes);
                            let stored_size = 1 + payload.len();

                            let cache = if flag == COMPRESSION_FLAG_LZ4 {
                                Some(CacheData::Compressed(flag, payload.into_owned()))
                            } else {
                                None
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
        Ok(())
    }

    #[cfg(test)]
    fn serialize_bfs_pass2_calculate_offsets(
        &self,
        serial_buf: &mut Vec<u8>,
        start_offset: u64,
    ) -> io::Result<(Vec<u64>, Vec<Vec<usize>>, u64)> {
        let mut node_refs: Vec<&BPlusTreeNode<K, V>> = vec![self];
        let mut node_offsets: Vec<u64> = vec![start_offset];
        let mut child_ids_by_node: Vec<Vec<usize>> = vec![Vec::new()];
        let mut current_offset = start_offset + self.calculate_serialized_size(serial_buf)?;
        let mut current_level = vec![0usize];

        while !current_level.is_empty() {
            let mut next_level = Vec::new();
            for node_id in current_level {
                let node = node_refs[node_id];
                if !node.is_leaf {
                    for child in &node.children {
                        let child_id = node_refs.len();
                        node_refs.push(child);
                        node_offsets.push(current_offset);
                        child_ids_by_node.push(Vec::new());
                        child_ids_by_node[node_id].push(child_id);
                        current_offset += child.calculate_serialized_size(serial_buf)?;
                        next_level.push(child_id);
                    }
                }
            }
            current_level = next_level;
        }

        Ok((node_offsets, child_ids_by_node, current_offset))
    }

    #[cfg(test)]
    fn serialize_bfs_pass3_assign_value_offsets(&mut self, mut current_offset: u64) {
        use std::collections::HashMap;
        let mut current_level_mut = vec![self];
        while !current_level_mut.is_empty() {
            let mut next_level_mut = Vec::new();
            for node in current_level_mut {
                if node.is_leaf {
                    let mut pack_block_offsets: HashMap<u64, u64> = HashMap::new();

                    for info in &mut node.value_info {
                        match &mut info.mode {
                            ValueStorageMode::Packed(pack_idx, _index) => {
                                if !pack_block_offsets.contains_key(pack_idx) {
                                    pack_block_offsets.insert(*pack_idx, current_offset);
                                    current_offset += PAGE_SIZE_USIZE as u64;
                                }
                            }
                            ValueStorageMode::Single(offset) if *offset == u64::MAX => {
                                *offset = current_offset;
                                current_offset += u64::from(info.length);
                            }
                            ValueStorageMode::Single(_) | ValueStorageMode::Tombstone => {}
                        }
                    }

                    for info in &mut node.value_info {
                        if let ValueStorageMode::Packed(pack_idx, _index) = &mut info.mode {
                            let actual_offset = pack_block_offsets[pack_idx];
                            *pack_idx = actual_offset;
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

    #[cfg(test)]
    fn serialize_bfs_pass4_write_nodes<W: Write + Seek>(
        &self,
        file: &mut W,
        buffer: &mut Vec<u8>,
        serial_buf: &mut Vec<u8>,
        node_offsets: &[u64],
        child_ids_by_node: &[Vec<usize>],
    ) -> io::Result<()> {
        let mut node_refs: Vec<&BPlusTreeNode<K, V>> = vec![self];
        let mut node_cursor = 0;
        while node_cursor < node_refs.len() {
            let node = node_refs[node_cursor];
            if !node.is_leaf {
                node_refs.extend(node.children.iter());
            }
            node_cursor += 1;
        }

        if node_refs.len() != node_offsets.len() || node_refs.len() != child_ids_by_node.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "B+Tree serialization produced inconsistent node offset table",
            ));
        }

        for (node_id, node) in node_refs.iter().enumerate() {
            let node_offset = node_offsets[node_id];

            if node.is_leaf {
                node.serialize_to_block(file, buffer, serial_buf, node_offset)?;
            } else {
                let node_child_ids = child_ids_by_node.get(node_id).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "B+Tree serialization missing child id table entry",
                    )
                })?;
                let mut child_offsets = Vec::with_capacity(node_child_ids.len());
                for child_id in node_child_ids {
                    let Some(child_offset) = node_offsets.get(*child_id) else {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "B+Tree serialization child id has no offset",
                        ));
                    };
                    child_offsets.push(*child_offset);
                }

                node.serialize_internal_with_offsets(
                    file,
                    buffer,
                    serial_buf,
                    node_offset,
                    &child_offsets,
                )?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn serialize_bfs_pass5_write_values<W: Write + Seek>(
        &self,
        file: &mut W,
        buffer: &mut [u8],
        serial_buf: &mut Vec<u8>,
    ) -> io::Result<()> {
        use std::collections::HashMap;
        let mut current_level_values = vec![self];
        while !current_level_values.is_empty() {
            let mut next_level = Vec::new();
            for node in current_level_values {
                if node.is_leaf {
                    let mut pack_blocks: HashMap<u64, Vec<(u16, Vec<u8>)>> = HashMap::new();

                    for (value, info) in node.values.iter().zip(node.value_info.iter()) {
                        serial_buf.clear();
                        binary_serialize_into(serial_buf, value)?;

                        match info.mode {
                            ValueStorageMode::Packed(block_offset, index) => {
                                pack_blocks.entry(block_offset).or_default().push((index, serial_buf.clone()));
                            }
                            ValueStorageMode::Single(block_offset) => {
                                file.seek(SeekFrom::Start(block_offset))?;

                                let cache_guard = info.cache.lock();
                                let (flag, payload_ref) =
                                    if let Some(CacheData::Compressed(c_flag, c_payload)) = cache_guard.as_ref() {
                                        (*c_flag, c_payload.as_slice())
                                    } else {
                                        (COMPRESSION_FLAG_NONE, serial_buf.as_slice())
                                    };

                                file.write_all(&[flag])?;
                                file.write_all(payload_ref)?;
                            }
                            ValueStorageMode::Tombstone => {}
                        }
                    }

                    for (block_offset, mut values) in pack_blocks {
                        values.sort_by_key(|(idx, _)| *idx);
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
        Ok(())
    }

    /// Serialize an internal node with pre-calculated child offsets
    /// Supports multi-block internal nodes when keys + pointers exceed a single page
    #[cfg(test)]
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

        let is_leaf = node_flag_to_is_leaf(buffer[0])?;
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
            if info.len() != keys.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Invalid leaf node: {} keys but {} value descriptors", keys.len(), info.len()),
                ));
            }
            let vals = if nested {
                let mut filtered_keys: Vec<K> = Vec::with_capacity(keys.len());
                let mut filtered_info: Vec<ValueInfo> = Vec::with_capacity(info.len());
                let mut v = Vec::with_capacity(info.len());

                let original_keys = std::mem::take(&mut keys);
                for (entry_key, entry_info) in original_keys.into_iter().zip(info) {
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
            if !valid_internal_pointer_count(keys.len(), pointers.len()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Invalid internal node: {} keys but {} child pointers", keys.len(), pointers.len()),
                ));
            }
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
        let header_range = checked_slice_range(0, FLAG_SIZE + LEN_SIZE, slice.len())?;
        // Node type
        let is_leaf = node_flag_to_is_leaf(slice[0])?;
        let mut read_pos = FLAG_SIZE;

        // ---- Keys ----
        let keys_length = u32_from_bytes(&slice[read_pos..header_range.end])? as usize;
        read_pos += LEN_SIZE;
        let keys_range = checked_slice_range(read_pos, keys_length, slice.len())?;
        let mut keys: Vec<K> = binary_deserialize(&slice[keys_range.clone()])?;
        read_pos = keys_range.end;

        // ---- Value info (offset, length) for leaf nodes ----
        let (value_info, values): (Vec<ValueInfo>, Vec<V>) = if is_leaf {
            // Read value_info
            let info_len_range = checked_slice_range(read_pos, LEN_SIZE, slice.len())?;
            let info_length = u32_from_bytes(&slice[info_len_range.clone()])? as usize;
            read_pos = info_len_range.end;
            let info_range = checked_slice_range(read_pos, info_length, slice.len())?;
            let mut info: Vec<ValueInfo> = binary_deserialize(&slice[info_range])?;
            if info.len() != keys.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Invalid leaf node: {} keys but {} value descriptors", keys.len(), info.len()),
                ));
            }

            // Values are loaded on-demand when nested=true
            if nested {
                let mut vals = Vec::with_capacity(info.len());
                let mut filtered_keys: Vec<K> = Vec::with_capacity(keys.len());
                let mut filtered_info: Vec<ValueInfo> = Vec::with_capacity(info.len());
                let mut last_packed_block: Option<(u64, Vec<u8>)> = None;
                let original_keys = std::mem::take(&mut keys);
                for (entry_key, entry_info) in original_keys.into_iter().zip(info) {
                    if entry_info.is_tombstone() {
                        continue;
                    }
                    match entry_info.mode {
                        ValueStorageMode::Packed(block_offset, index) => {
                            // Packed loading optimization: reuse block if it's the same
                            if let Some((offset, ref block)) = last_packed_block {
                                if offset == block_offset {
                                    vals.push(Self::extract_value_from_packed_block(block, index)?);
                                    filtered_keys.push(entry_key);
                                    filtered_info.push(entry_info);
                                    continue;
                                }
                            }

                            // Load new block
                            let mut block = vec![0u8; PAGE_SIZE_USIZE];
                            file.seek(SeekFrom::Start(block_offset))?;
                            file.read_exact(&mut block)?;
                            vals.push(Self::extract_value_from_packed_block(&block, index)?);
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
            let pointers_len_range = checked_slice_range(read_pos, LEN_SIZE, slice.len())?;
            let pointers_length = u32_from_bytes(&slice[pointers_len_range.clone()])? as usize;
            read_pos = pointers_len_range.end;
            let pointers_range = checked_slice_range(read_pos, pointers_length, slice.len())?;
            let pointers: Vec<u64> = binary_deserialize(&slice[pointers_range])?;
            if !valid_internal_pointer_count(keys.len(), pointers.len()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Invalid internal node: {} keys but {} child pointers", keys.len(), pointers.len()),
                ));
            }
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
                Self::load_value_from_packed_block(file, block_offset, index, info.length)
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
    ) -> io::Result<V> {
        file.seek(SeekFrom::Start(block_offset))?;

        let mut block_buffer = vec![0u8; PAGE_SIZE_USIZE];
        file.read_exact(&mut block_buffer)?;

        Self::extract_value_from_packed_block(&block_buffer, value_index)
    }

    /// Helper to extract value from a packed block that is already in memory
    fn extract_value_from_packed_block(block_buffer: &[u8], value_index: u16) -> io::Result<V> {
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
#[cfg(test)]
#[derive(Clone, Debug, PartialEq)]
pub enum BPlusTreeMetadata {
    Empty,
    TargetIdMapping(u32),
}

#[cfg(test)]
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

}

#[cfg(test)]
#[derive(Debug, Clone)]
pub struct BPlusTree<K, V> {
    root: BPlusTreeNode<K, V>,
    inner_order: usize,
    leaf_order: usize,
    metadata: BPlusTreeMetadata,
    dirty: bool,
}

#[cfg(test)]
const fn sanitize_order(order: usize) -> usize {
    if order < 2 {
        2
    } else {
        order
    }
}

#[cfg(test)]
const fn default_orders() -> (usize, usize) { (DEFAULT_INNER_ORDER, DEFAULT_LEAF_ORDER) }

#[cfg(test)]
impl<K, V> Default for BPlusTree<K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
impl<K, V> BPlusTree<K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub const fn new() -> Self {
        let (inner_order, leaf_order) = default_orders();
        Self::new_with_orders(inner_order, leaf_order)
    }

    /// Create a v2 tree with explicit in-memory fanout.
    ///
    /// This does not change the on-disk v2 format. Orders below 2 are clamped
    /// because B+Tree split logic requires at least two keys per node.
    pub const fn new_with_orders(inner_order: usize, leaf_order: usize) -> Self {
        Self {
            root: BPlusTreeNode::<K, V>::new(true),
            inner_order: sanitize_order(inner_order),
            leaf_order: sanitize_order(leaf_order),
            metadata: BPlusTreeMetadata::Empty,
            dirty: true, // an empty tree is stored!
        }
    }

    /// Helper to set metadata
    pub fn set_metadata(&mut self, data: BPlusTreeMetadata) {
        self.metadata = data;
        self.dirty = true;
    }

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

    pub(crate) fn add_historical_fence_key(&mut self) -> bool { self.root.add_historical_fence_key() }

    pub(crate) fn remove_last_root_child(&mut self) -> bool {
        !self.root.is_leaf && self.root.children.pop().is_some()
    }

    pub fn store(&mut self, filepath: &Path) -> io::Result<u64> {
        if self.dirty {
            self.store_internal(filepath)
        } else {
            Ok(0)
        }
    }

    /// Internal store without locking, used for compaction or initial save.
    fn store_internal(&mut self, filepath: &Path) -> io::Result<u64> {
        let tempfile = if let Some(parent_dir) = filepath.parent() {
            if let Ok(file) = NamedTempFile::new_in(parent_dir) {
                file
            } else {
                let temp_dir = tempfile::env::temp_dir();
                NamedTempFile::new_in(&temp_dir)?
            }
        } else {
            let temp_dir = tempfile::env::temp_dir();
            NamedTempFile::new_in(&temp_dir)?
        };
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

}

fn validate_legacy_tree<K, V>(
    mmap: Option<&Mmap>,
    file: &mut BufReader<File>,
    root_offset: u64,
    file_len: u64,
) -> io::Result<()>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    let mut pending = vec![(root_offset, None::<K>, None::<K>, true)];
    let mut visited = HashSet::new();
    while let Some((offset, lower, upper, is_root)) = pending.pop() {
        if offset < HEADER_SIZE || offset >= file_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("legacy B+Tree child offset {offset} is outside file length {file_len}"),
            ));
        }
        if !visited.insert(offset) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "legacy B+Tree contains a cycle or shared child"));
        }

        let (node, pointers) = if let Some(mapped) = mmap {
            let mut cursor = io::Cursor::new(mapped.as_ref());
            BPlusTreeNode::<K, V>::deserialize_from_mmap(mapped, &mut cursor, offset, false)?
        } else {
            let mut buffer = Vec::with_capacity(PAGE_SIZE_USIZE);
            BPlusTreeNode::<K, V>::deserialize_from_block(file, &mut buffer, offset, false)?
        };
        if node.keys.windows(2).any(|keys| keys[0] >= keys[1]) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "legacy B+Tree node keys are not strictly increasing"));
        }

        if node.is_leaf {
            if let Some(expected) = lower.as_ref() {
                if node.keys.first() != Some(expected) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "legacy B+Tree separator differs from child minimum",
                    ));
                }
            }
            if upper.as_ref().is_some_and(|bound| node.keys.last().is_some_and(|key| key >= bound)) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "legacy B+Tree leaf exceeds its upper bound"));
            }
            continue;
        }

        let pointers = pointers.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "internal node has no pointers"))?;
        let has_fence_key = pointers.len() == node.keys.len();
        if has_fence_key && (is_root || node.keys.last() != upper.as_ref()) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid historical internal-node fence key"));
        }

        for (index, child) in pointers.into_iter().enumerate().rev() {
            let child_lower = if index == 0 { lower.clone() } else { node.keys.get(index - 1).cloned() };
            let child_upper = node.keys.get(index).cloned().or_else(|| upper.clone());
            pending.push((child, child_lower, child_upper, false));
        }
    }
    Ok(())
}


/// `BPlusTreeQuery` performs on-disk queries without loading the entire tree into memory.
/// For frequent queries, consider using `BPlusTree::load()` instead, which loads the full tree into memory
/// at the cost of higher memory usage.
pub struct BPlusTreeQuery<K, V> {
    file: Option<BufReader<File>>,
    mmap: Option<Mmap>,
    has_tombstones: bool,
    buffer: Vec<u8>,
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
        let mmap = mmap_with_advice(&file, Advice::Normal, "B+Tree query");

        // Verify Header
        let mut header = [0u8; METADATA_DATA_START_POS];
        read_exact_at_offset(&file, &mut header, 0)?;

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

        let mut validation_file = utils::file_reader(file.try_clone()?);
        validate_legacy_tree::<K, V>(mmap.as_ref(), &mut validation_file, root_offset, file_len)?;

        Ok(Self {
            file: if mmap.is_some() { None } else { Some(utils::file_reader(file)) },
            mmap,
            has_tombstones,
            buffer: vec![0u8; PAGE_SIZE_USIZE],
            root_offset,
            _marker_k: PhantomData,
            _marker_v: PhantomData,
        })
    }

    pub fn try_new(filepath: &Path) -> io::Result<Self> {
        Self::try_from_file(File::open(filepath)?)
    }

}


impl<K, V> BPlusTreeQuery<K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    /// Iterates over key-value pairs within a given range using `right_sibling` pointers.
    ///
    /// This is more efficient than iterating the full tree and filtering when you only
    /// need a subset of keys.
    ///
    /// Tombstones are skipped automatically.
    pub fn range_iter(
        &mut self,
        start: Bound<&K>,
        end: Bound<&K>,
    ) -> impl Iterator<Item = Result<(K, V), BPlusTreeError>> + '_ {
        let start_cloned = match start {
            Bound::Included(k) => Bound::Included(k.clone()),
            Bound::Excluded(k) => Bound::Excluded(k.clone()),
            Bound::Unbounded => Bound::Unbounded,
        };
        let end_cloned = match end {
            Bound::Included(k) => Bound::Included(k.clone()),
            Bound::Excluded(k) => Bound::Excluded(k.clone()),
            Bound::Unbounded => Bound::Unbounded,
        };
        RangeLeafIterator::new(self, start_cloned, end_cloned)
    }

}

/// Range scan iterator that seeks into the tree and then walks in-order
/// without scanning from the root for every entry.
struct RangeLeafIterator<'a, K, V> {
    tree: &'a mut BPlusTreeQuery<K, V>,
    start_bound: Bound<K>,
    end_bound: Bound<K>,
    stack: TraversalStack,
    current_leaf: Option<BPlusTreeNode<K, V>>,
    leaf_idx: usize,
    initialized: bool,
    exhausted: bool,
}

/// Shared range-iterator helpers retained by the frozen v2 migration reader.
macro_rules! impl_range_leaf_common {
    ($tree_ty:ty, $lt:tt) => {
        fn new(tree: &$lt mut $tree_ty, start: Bound<K>, end: Bound<K>) -> Self {
            Self {
                tree,
                start_bound: start,
                end_bound: end,
                stack: smallvec![],
                current_leaf: None,
                leaf_idx: 0,
                initialized: false,
                exhausted: false,
            }
        }

        fn load_leaf_from_node(&mut self, node: BPlusTreeNode<K, V>, start_idx: usize) {
            self.current_leaf = Some(node);
            self.leaf_idx = start_idx;
        }

        fn key_past_end(&self, key: &K) -> bool {
            match &self.end_bound {
                Bound::Included(end) => key > end,
                Bound::Excluded(end) => key >= end,
                Bound::Unbounded => false,
            }
        }

    };
}

impl<'a, K, V> RangeLeafIterator<'a, K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    impl_range_leaf_common!(BPlusTreeQuery<K, V>, 'a);

    fn descend_to_leaf(&mut self, mut offset: u64, mut start_key: Option<&K>) -> io::Result<()> {
        loop {
            let (node, pointers) = if let Some(mmap) = &self.tree.mmap {
                let mut cursor = io::Cursor::new(mmap.as_ref());
                BPlusTreeNode::<K, V>::deserialize_from_mmap(mmap, &mut cursor, offset, false)?
            } else if let Some(file) = &mut self.tree.file {
                BPlusTreeNode::<K, V>::deserialize_from_block(file, &mut self.tree.buffer, offset, false)?
            } else {
                return Err(io::Error::other("No data source available"));
            };

            if node.is_leaf {
                let start_idx = if let Some(key) = start_key {
                    match self.start_bound {
                        Bound::Included(_) => node.keys.partition_point(|candidate| candidate < key),
                        Bound::Excluded(_) => node.keys.partition_point(|candidate| candidate <= key),
                        Bound::Unbounded => 0,
                    }
                } else {
                    0
                };
                self.load_leaf_from_node(node, start_idx);
                return Ok(());
            }

            let child_idx = if let Some(key) = start_key {
                get_entry_index_upper_bound(&node.keys, key)
            } else {
                0
            };

            let Some(ptrs) = pointers else {
                self.exhausted = true;
                return Ok(());
            };
            let Some(&next_offset) = ptrs.get(child_idx) else {
                self.exhausted = true;
                return Ok(());
            };
            self.stack.push((offset, child_idx.saturating_add(1)));
            offset = next_offset;
            start_key = None.or(start_key);
        }
    }

    fn initialize(&mut self) -> io::Result<()> {
        if self.initialized {
            return Ok(());
        }
        self.initialized = true;
        let start_key = match self.start_bound.clone() {
            Bound::Included(key) | Bound::Excluded(key) => Some(key),
            Bound::Unbounded => None,
        };
        self.descend_to_leaf(self.tree.root_offset, start_key.as_ref())
    }

    fn advance_leaf(&mut self) -> io::Result<()> {
        while let Some((offset, child_idx)) = self.stack.pop() {
            let (_node, pointers) = if let Some(mmap) = &self.tree.mmap {
                let mut cursor = io::Cursor::new(mmap.as_ref());
                BPlusTreeNode::<K, V>::deserialize_from_mmap(mmap, &mut cursor, offset, false)?
            } else if let Some(file) = &mut self.tree.file {
                BPlusTreeNode::<K, V>::deserialize_from_block(file, &mut self.tree.buffer, offset, false)?
            } else {
                return Err(io::Error::other("No data source available"));
            };

            let Some(ptrs) = pointers else {
                continue;
            };
            let Some(&next_offset) = ptrs.get(child_idx) else {
                continue;
            };
            if child_idx + 1 < ptrs.len() {
                self.stack.push((offset, child_idx + 1));
            }
            self.descend_to_leaf(next_offset, None)?;
            return Ok(());
        }

        self.exhausted = true;
        Ok(())
    }
}

impl<K, V> Iterator for RangeLeafIterator<'_, K, V>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    type Item = Result<(K, V), BPlusTreeError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.exhausted {
            return None;
        }

        // Lazy initialization
        if !self.initialized {
            if let Err(e) = self.initialize() {
                self.exhausted = true;
                return Some(Err(BPlusTreeError::Io(e)));
            }
        }

        loop {
            let Some(node) = self.current_leaf.as_ref() else {
                if self.exhausted {
                    return None;
                }
                if let Err(e) = self.advance_leaf() {
                    self.exhausted = true;
                    return Some(Err(BPlusTreeError::Io(e)));
                }
                if self.exhausted {
                    return None;
                }
                continue;
            };

            if self.leaf_idx >= node.keys.len() {
                self.current_leaf = None;
                if self.exhausted {
                    return None;
                }
                if let Err(e) = self.advance_leaf() {
                    self.exhausted = true;
                    return Some(Err(BPlusTreeError::Io(e)));
                }
                if self.exhausted {
                    return None;
                }
                continue;
            }

            let idx = self.leaf_idx;
            self.leaf_idx += 1;
            let key = node.keys[idx].clone();

            if self.key_past_end(&key) {
                self.current_leaf = None;
                self.exhausted = true;
                return None;
            }

            let info = node.value_info[idx].clone();
            if self.tree.has_tombstones && info.is_tombstone() {
                continue;
            }

            let value = if let Some(mmap) = &self.tree.mmap {
                let mut cursor = io::Cursor::new(mmap.as_ref());
                match BPlusTreeNode::<K, V>::load_value_from_info(&mut cursor, &info) {
                    Ok(value) => value,
                    Err(err) => {
                        self.exhausted = true;
                        return Some(Err(BPlusTreeError::Io(err)));
                    }
                }
            } else if let Some(file) = &mut self.tree.file {
                match BPlusTreeNode::<K, V>::load_value_from_info(file, &info) {
                    Ok(value) => value,
                    Err(err) => {
                        self.exhausted = true;
                        return Some(Err(BPlusTreeError::Io(err)));
                    }
                }
            } else {
                self.exhausted = true;
                return Some(Err(BPlusTreeError::InvalidStructure("No data source available".into())));
            };

            return Some(Ok((key, value)));
        }
    }
}

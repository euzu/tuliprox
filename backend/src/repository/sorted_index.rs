//! Sorted Index for `BPlusTree`
//!
//! This module provides a compact sorted index file that enables iteration
//! over a `BPlusTree` in order of a secondary sort key rather than the primary key.
//!
//! # File Format (v3)
//! ```text
//! [magic: 4 bytes]["SIDX"]
//! [version: u32]
//! [count: u64]
//! [entry0][entry1]...
//!
//! Entry format (with value location for O(1) access):
//! [sort_key_len: u32][sort_key_bytes][primary_key_len: u32][primary_key_bytes][value_location]
//!
//! Value location format:
//! - Single mode: [mode: u8 = 0][offset: u64][length: u32]
//! - Packed mode: [mode: u8 = 1][block_offset: u64][index: u16][length: u32]
//! ```
//!
//! # Optimization
//! By storing the value location directly in the index, iteration can
//! read values in O(1) time by seeking directly to the offset, avoiding O(log n)
//! tree traversal for each lookup.
//!
//! > **Important**: The index is tightly coupled to the B+Tree file structure.
//! > It must be rebuilt after any operation that changes value offsets (e.g., `compact()`).

use crate::utils::{binary_deserialize, binary_serialize};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use crate::repository::bplustree::{COMPRESSION_FLAG_LZ4, PAGE_SIZE_USIZE};
use indexmap::IndexMap;
use crate::repository::storage::get_file_path_for_db_index;

const MAGIC: &[u8; 4] = b"SIDX";
const VERSION: u32 = 3; // Bumped for new format with flexible value location
const HEADER_SIZE: usize = 16; // 4 (magic) + 4 (version) + 8 (count)

const MODE_SINGLE: u8 = 0;
const MODE_PACKED: u8 = 1;



/// Represents how a value can be located and read from the tree file.
#[derive(Debug, Clone, Copy)]
pub enum ValueLocation {
    /// Single value stored at a specific offset with a given length.
    Single { offset: u64, length: u32 },
    /// Value packed in a block with other values, identified by block offset and index.
    Packed { block_offset: u64, index: u16, length: u32 },
}

impl ValueLocation {
    /// Serialize the value location to bytes.
    fn to_bytes(self) -> Vec<u8> {
        match self {
            ValueLocation::Single { offset, length } => {
                let mut bytes = vec![MODE_SINGLE];
                bytes.extend_from_slice(&offset.to_le_bytes());
                bytes.extend_from_slice(&length.to_le_bytes());
                bytes
            }
            ValueLocation::Packed { block_offset, index, length } => {
                let mut bytes = vec![MODE_PACKED];
                bytes.extend_from_slice(&block_offset.to_le_bytes());
                bytes.extend_from_slice(&index.to_le_bytes());
                bytes.extend_from_slice(&length.to_le_bytes());
                bytes
            }
        }
    }

    /// Deserialize a value location from a reader.
    fn from_reader<R: Read>(reader: &mut R) -> io::Result<Self> {
        let mut mode = [0u8; 1];
        reader.read_exact(&mut mode)?;

        match mode[0] {
            MODE_SINGLE => {
                let mut offset_buf = [0u8; 8];
                let mut length_buf = [0u8; 4];
                reader.read_exact(&mut offset_buf)?;
                reader.read_exact(&mut length_buf)?;
                Ok(ValueLocation::Single {
                    offset: u64::from_le_bytes(offset_buf),
                    length: u32::from_le_bytes(length_buf),
                })
            }
            MODE_PACKED => {
                let mut block_offset_buf = [0u8; 8];
                let mut index_buf = [0u8; 2];
                let mut length_buf = [0u8; 4];
                reader.read_exact(&mut block_offset_buf)?;
                reader.read_exact(&mut index_buf)?;
                reader.read_exact(&mut length_buf)?;
                Ok(ValueLocation::Packed {
                    block_offset: u64::from_le_bytes(block_offset_buf),
                    index: u16::from_le_bytes(index_buf),
                    length: u32::from_le_bytes(length_buf),
                })
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Unknown value location mode: {}", mode[0]),
            )),
        }
    }
}

/// Entry containing value location information for direct access.
#[derive(Debug, Clone)]
pub struct IndexEntry<SortKey, K> {
    pub sort_key: SortKey,
    pub primary_key: K,
    pub location: ValueLocation,
}

/// Writer for building a sorted index file.
///
/// Entries must be pushed in sorted order. The writer buffers writes
/// and flushes on `finish()`.
pub struct SortedIndexWriter<SortKey, K> {
    writer: BufWriter<File>,
    count: u64,
    _marker: PhantomData<(SortKey, K)>,
}

impl<SortKey, K> SortedIndexWriter<SortKey, K>
where
    SortKey: Serialize,
    K: Serialize,
{
    /// Create a new index writer at the given path.
    /// Overwrites any existing file.
    pub fn new(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;

        let mut writer = BufWriter::new(file);

        // Write header (count will be updated on finish)
        writer.write_all(MAGIC)?;
        writer.write_all(&VERSION.to_le_bytes())?;
        writer.write_all(&0u64.to_le_bytes())?; // placeholder count

        Ok(Self {
            writer,
            count: 0,
            _marker: PhantomData,
        })
    }

    /// Append an entry to the index with value location for O(1) access.
    /// Caller must ensure entries are pushed in sorted order.
    pub fn push(
        &mut self,
        sort_key: &SortKey,
        primary_key: &K,
        location: ValueLocation,
    ) -> io::Result<()> {
        let sk_bytes = binary_serialize(sort_key)?;
        let pk_bytes = binary_serialize(primary_key)?;

        // Write sort key
        let sk_len = u32::try_from(sk_bytes.len())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.writer.write_all(&sk_len.to_le_bytes())?;
        self.writer.write_all(&sk_bytes)?;

        // Write primary key
        let pk_len = u32::try_from(pk_bytes.len())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.writer.write_all(&pk_len.to_le_bytes())?;
        self.writer.write_all(&pk_bytes)?;

        // Write value location
        self.writer.write_all(&location.to_bytes())?;

        self.count += 1;
        Ok(())
    }

    /// Finalize the index file by writing the count to the header.
    pub fn finish(mut self) -> io::Result<u64> {
        self.writer.flush()?;

        // Seek back to count position and write final count
        let file = self.writer.into_inner()?;
        let mut file = file;
        file.seek(SeekFrom::Start(8))?; // After magic + version
        file.write_all(&self.count.to_le_bytes())?;
        file.sync_all()?;

        Ok(self.count)
    }
}

/// Reader for iterating over a sorted index file.
pub struct SortedIndexReader<SortKey, K> {
    reader: BufReader<File>,
    remaining: u64,
    _marker: PhantomData<(SortKey, K)>,
}

impl<SortKey, K> SortedIndexReader<SortKey, K>
where
    SortKey: for<'de> Deserialize<'de>,
    K: for<'de> Deserialize<'de>,
{
    /// Open an existing index file for reading.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);

        // Verify header
        let mut header = [0u8; HEADER_SIZE];
        reader.read_exact(&mut header)?;

        if &header[0..4] != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid sorted index magic",
            ));
        }

        let version = u32::from_le_bytes(header[4..8].try_into().unwrap_or_default());
        if version != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Unsupported sorted index version: {version} (expected {VERSION})"),
            ));
        }

        let count = match header[8..16].try_into() {
            Ok(count) => u64::from_le_bytes(count),
            Err(e) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Invalid count {e}"),
                ));
            }
        };

        Ok(Self {
            reader,
            remaining: count,
            _marker: PhantomData,
        })
    }

    /// Returns the number of remaining entries to read.
    pub fn remaining(&self) -> u64 {
        self.remaining
    }

    /// Returns true if the index is empty.
    pub fn is_empty(&self) -> bool {
        self.remaining == 0
    }

    /// Read the next entry from the index.
    pub fn read_next(&mut self) -> io::Result<Option<IndexEntry<SortKey, K>>> {
        if self.remaining == 0 {
            return Ok(None);
        }

        // Read sort key
        let mut len_buf = [0u8; 4];
        self.reader.read_exact(&mut len_buf)?;
        let sk_len = u32::from_le_bytes(len_buf) as usize;

        let mut sk_bytes = vec![0u8; sk_len];
        self.reader.read_exact(&mut sk_bytes)?;
        let sort_key: SortKey = binary_deserialize(&sk_bytes)?;

        // Read primary key
        self.reader.read_exact(&mut len_buf)?;
        let pk_len = u32::from_le_bytes(len_buf) as usize;

        let mut pk_bytes = vec![0u8; pk_len];
        self.reader.read_exact(&mut pk_bytes)?;
        let primary_key: K = binary_deserialize(&pk_bytes)?;

        // Read value location
        let location = ValueLocation::from_reader(&mut self.reader)?;

        self.remaining -= 1;
        Ok(Some(IndexEntry {
            sort_key,
            primary_key,
            location,
        }))
    }
}

impl<SortKey, K> Iterator for SortedIndexReader<SortKey, K>
where
    SortKey: for<'de> Deserialize<'de>,
    K: for<'de> Deserialize<'de>,
{
    type Item = io::Result<IndexEntry<SortKey, K>>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.read_next() {
            Ok(Some(entry)) => Some(Ok(entry)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

/// Owned iterator that combines a sorted index with a primary `BPlusTree` file.
///
/// Iterates through the index in order and reads values directly from the
/// tree file using stored offsets - no tree traversal needed (O(1) per item).
pub struct BPlusTreeSortedIteratorOwned<K, V, SortKey> {
    index_reader: SortedIndexReader<SortKey, K>,
    tree_file: BufReader<File>,
    filepath: PathBuf,
    block_cache: IndexMap<u64, Vec<u8>>,
    _marker: PhantomData<V>,
}

const CACHE_CAPACITY: usize = 8;

impl<K, V, SortKey> BPlusTreeSortedIteratorOwned<K, V, SortKey>
where
    K: for<'de> Deserialize<'de>,
    V: for<'de> Deserialize<'de>,
    SortKey: for<'de> Deserialize<'de>,
{
    /// Create a new owned sorted iterator.
    ///
    /// The index path is automatically derived from the tree filepath
    /// by changing the extension to `.idx`.
    pub fn new(filepath: PathBuf, tree_file: BufReader<File>) -> io::Result<Self> {
        let index_path = get_file_path_for_db_index(&filepath);
        let index_reader = SortedIndexReader::open(&index_path)?;
        Ok(Self {
            index_reader,
            tree_file,
            filepath,
            block_cache: IndexMap::with_capacity(CACHE_CAPACITY),
            _marker: PhantomData,
        })
    }

    /// Create from an explicit index path.
    pub fn with_index_path(
        filepath: PathBuf,
        tree_file: BufReader<File>,
        index_path: &Path,
    ) -> io::Result<Self> {
        let index_reader = SortedIndexReader::open(index_path)?;
        Ok(Self {
            index_reader,
            tree_file,
            filepath,
            block_cache: IndexMap::with_capacity(CACHE_CAPACITY),
            _marker: PhantomData,
        })
    }

    /// Returns the number of remaining items.
    pub fn remaining(&self) -> u64 {
        self.index_reader.remaining
    }

    /// Returns the path to the tree file.
    pub fn filepath(&self) -> &Path {
        &self.filepath
    }

    /// Read a value from the tree file using the given `ValueLocation`.
    fn read_value(&mut self, location: ValueLocation) -> io::Result<V> {
        match location {
            ValueLocation::Single { offset, length } => {
                self.read_value_single(offset, length)
            }
            ValueLocation::Packed { block_offset, index, length: _ } => {
                self.read_value_packed(block_offset, index)
            }
        }
    }

    /// Read a single value directly from the tree file at the given offset.
    fn read_value_single(&mut self, offset: u64, length: u32) -> io::Result<V> {
        self.tree_file.seek(SeekFrom::Start(offset))?;

        // Read compression flag
        let mut flag = [0u8; 1];
        self.tree_file.read_exact(&mut flag)?;

        let data = if flag[0] == COMPRESSION_FLAG_LZ4 {
            // Compressed: [flag:1][lz4_payload_with_prepended_size]
            let compressed_len = length as usize - 1;
            let mut compressed = vec![0u8; compressed_len];
            self.tree_file.read_exact(&mut compressed)?;

            lz4_flex::decompress_size_prepended(&compressed).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("LZ4 decompression failed: {e}"),
                )
            })?
        } else {
            // Uncompressed: [flag:1][payload]
            let payload_len = length as usize - 1;
            let mut data = vec![0u8; payload_len];
            self.tree_file.read_exact(&mut data)?;
            data
        };

        crate::utils::binary_deserialize(&data)
    }

    /// Read a value from a packed block at the given index.
    fn read_value_packed(&mut self, block_offset: u64, value_index: u16) -> io::Result<V> {

        // Try cache first
        if !self.block_cache.contains_key(&block_offset) {
            // Miss - read from disk
            self.tree_file.seek(SeekFrom::Start(block_offset))?;
            let mut buf = vec![0u8; PAGE_SIZE_USIZE];
            self.tree_file.read_exact(&mut buf)?;

            // Update cache
            if self.block_cache.len() >= CACHE_CAPACITY {
                self.block_cache.shift_remove_index(0); // Remove LRU
            }
            self.block_cache.insert(block_offset, buf.clone());
        }

        let block_buffer = self.block_cache.get(&block_offset).unwrap();

        // Read count (first 4 bytes)
        let count = u32::from_le_bytes(block_buffer[0..4].try_into().map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("Invalid count: {e}"))
        })?);
        let mut pos = 4;

        // Skip to target value
        for i in 0..=value_index {
            if pos + 4 > PAGE_SIZE_USIZE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Packed block corrupted: position {pos} exceeds block size"),
                ));
            }

            let len = u32::from_le_bytes(block_buffer[pos..pos + 4].try_into().map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("Invalid length: {e}"))
            })?) as usize;
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
                return crate::utils::binary_deserialize(value_data);
            }

            pos += len;
        }

        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Value index {value_index} not found in packed block (count: {count})"),
        ))
    }
}

impl<K, V, SortKey> Iterator for BPlusTreeSortedIteratorOwned<K, V, SortKey>
where
    K: for<'de> Deserialize<'de> + Clone,
    V: for<'de> Deserialize<'de>,
    SortKey: for<'de> Deserialize<'de>,
{
    type Item = io::Result<(K, V)>;

    fn next(&mut self) -> Option<Self::Item> {
        // Get next entry from index
        let entry = match self.index_reader.read_next() {
            Ok(Some(entry)) => entry,
            Ok(None) => return None,
            Err(e) => return Some(Err(e)),
        };

        // Read value from tree file using the stored location
        match self.read_value(entry.location) {
            Ok(value) => Some(Ok((entry.primary_key, value))),
            Err(e) => Some(Err(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_sorted_index_write_read() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.idx");

        // Write entries with value locations
        let mut writer = SortedIndexWriter::<String, u32>::new(&path).unwrap();
        writer.push(
            &"apple".to_string(), 
            &1u32, 
            ValueLocation::Single { offset: 100, length: 50 }
        ).unwrap();
        writer.push(
            &"banana".to_string(), 
            &2u32, 
            ValueLocation::Packed { block_offset: 200, index: 3, length: 75 }
        ).unwrap();
        writer.push(
            &"cherry".to_string(), 
            &3u32, 
            ValueLocation::Single { offset: 300, length: 100 }
        ).unwrap();
        let count = writer.finish().unwrap();
        assert_eq!(count, 3);

        // Read entries
        let reader = SortedIndexReader::<String, u32>::open(&path).unwrap();
        let entries: Vec<_> = reader.map(|r| r.unwrap()).collect();

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].sort_key, "apple".to_string());
        assert_eq!(entries[0].primary_key, 1u32);
        match entries[0].location {
            ValueLocation::Single { offset, length } => {
                assert_eq!(offset, 100);
                assert_eq!(length, 50);
            }
            _ => panic!("Expected Single location"),
        }

        assert_eq!(entries[1].sort_key, "banana".to_string());
        assert_eq!(entries[1].primary_key, 2u32);
        match entries[1].location {
            ValueLocation::Packed { block_offset, index, length } => {
                assert_eq!(block_offset, 200);
                assert_eq!(index, 3);
                assert_eq!(length, 75);
            }
            _ => panic!("Expected Packed location"),
        }

        assert_eq!(entries[2].sort_key, "cherry".to_string());
        assert_eq!(entries[2].primary_key, 3u32);
        match entries[2].location {
            ValueLocation::Single { offset, length } => {
                assert_eq!(offset, 300);
                assert_eq!(length, 100);
            }
            _ => panic!("Expected Single location"),
        }
    }

    #[test]
    fn test_empty_index() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.idx");

        let writer = SortedIndexWriter::<String, u32>::new(&path).unwrap();
        let count = writer.finish().unwrap();
        assert_eq!(count, 0);

        let reader = SortedIndexReader::<String, u32>::open(&path).unwrap();
        assert!(reader.is_empty());
        // Note: collect() consumes the reader, so we check is_empty() and remaining first
        let remaining = reader.remaining;
        assert_eq!(remaining, 0);
        
        let entries: Vec<_> = reader.collect();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_index_path_derivation() {
        let tree_path = Path::new("/data/my_tree.bin");
        let idx_path = get_file_path_for_db_index(tree_path);
        assert_eq!(idx_path, PathBuf::from("/data/my_tree.idx"));

        let tree_path2 = Path::new("/data/items");
        let idx_path2 = get_file_path_for_db_index(tree_path2);
        assert_eq!(idx_path2, PathBuf::from("/data/items.idx"));
    }
}

///////////
// Create flags file with: > cargo run --bin flags_builder
//
// Case-insensitive
// ================
// All methods are case-insensitive:
// loader.get_flag("de") == loader.get_flag("DE") == loader.get_flag("De")
//
// File Format
// ===========
// [Header - 7 bytes]
//   - Magic: "FLAG"
//   - Version: u8
//   - Entry count: u16
//
// Version 1
// ---------
// [Offset Table - 1352 bytes]
//   - u16 for every possible country code (AA-ZZ)
//   - 0xFFFF = not present
//
// [Data Section]
//   - LZ4-compressed SVGs
//   - Each entry: u16 length + compressed data (with prepended size)
//
// Version 2
// ---------
// [Entry Table - 4056 bytes]
//   - u32 local offset inside the decompressed block per country code
//   - u16 raw SVG length
//   - 0xFFFF_FFFF offset = not present
//
// [Block Table - 312 bytes]
//   - 26 blocks grouped by first country code letter
//   - Each block entry stores absolute file offset, compressed length and raw length
//
// [Data Section]
//   - Brotli-compressed blocks of concatenated SVG payloads
//
///////////
use brotli::Decompressor;
use lz4_flex;
use std::{cell::RefCell, io::Read};

pub const DEFAULT_COMPRESSION_LEVEL: u32 = 10;
const ENTRY_COUNT: usize = 676;
const BLOCK_COUNT: usize = 26;
const ENTRY_MISSING_OFFSET: u32 = u32::MAX;
const V1_ENTRY_SENTINEL: u16 = u16::MAX;

pub fn country_code_to_index(code: &str) -> Option<u16> {
    let bytes = code.as_bytes();
    if bytes.len() != 2 {
        return None;
    }
    let first = bytes[0].checked_sub(b'A')? as u16;
    let second = bytes[1].checked_sub(b'A')? as u16;
    if first > 25 || second > 25 {
        return None;
    }
    Some(first * 26 + second)
}

pub fn index_to_country_code(index: u16) -> Option<String> {
    if index >= ENTRY_COUNT as u16 {
        return None;
    }
    let first = (index / 26) as u8 + b'A';
    let second = (index % 26) as u8 + b'A';
    String::from_utf8(vec![first, second]).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FlagVersion {
    V1 = 1,
    V2 = 2,
}

#[derive(Debug)]
pub struct FlagFileHeader {
    pub magic: [u8; 4],
    pub version: FlagVersion,
    pub entry_count: u16,
}

impl FlagFileHeader {
    pub const MAGIC: [u8; 4] = *b"FLAG";
    pub const SIZE: usize = 7;

    pub fn new(version: FlagVersion, entry_count: u16) -> Self { Self { magic: Self::MAGIC, version, entry_count } }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(Self::SIZE);
        bytes.extend_from_slice(&self.magic);
        bytes.push(self.version as u8);
        bytes.extend_from_slice(&self.entry_count.to_le_bytes());
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::SIZE {
            return None;
        }
        let magic = [bytes[0], bytes[1], bytes[2], bytes[3]];
        if magic != Self::MAGIC {
            return None;
        }
        let version = match bytes[4] {
            1 => FlagVersion::V1,
            2 => FlagVersion::V2,
            _ => return None,
        };
        let entry_count = u16::from_le_bytes([bytes[5], bytes[6]]);

        Some(Self { magic, version, entry_count })
    }
}

#[derive(Debug, Clone)]
pub struct FlagEntry {
    pub country_code: String,
    pub svg_data: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
pub struct FlagDataEntry {
    pub local_offset: u32,
    pub svg_len: u16,
}

#[derive(Debug, Clone, Copy)]
pub struct FlagBlockEntry {
    pub offset: u32,
    pub compressed_len: u32,
    pub raw_len: u32,
}

#[derive(Debug, Clone)]
enum FlagStorage {
    V1 { offset_table: Vec<u16> },
    V2 { entry_table: Vec<FlagDataEntry>, block_table: Vec<FlagBlockEntry> },
}

pub struct FlagsLoader {
    data: Vec<u8>,
    storage: FlagStorage,
    block_cache: RefCell<Vec<Option<Vec<u8>>>>,
}

impl FlagsLoader {
    pub fn from_bytes(data: Vec<u8>) -> Result<Self, String> {
        if data.len() < FlagFileHeader::SIZE {
            return Err("Data too short for header".to_string());
        }

        let header = FlagFileHeader::from_bytes(&data[..FlagFileHeader::SIZE]).ok_or("Invalid header")?;

        if header.entry_count as usize != ENTRY_COUNT {
            return Err(format!("Invalid entry count: {}", header.entry_count));
        }

        let storage = match header.version {
            FlagVersion::V1 => Self::parse_v1(&data)?,
            FlagVersion::V2 => Self::parse_v2(&data)?,
        };

        Ok(Self { data, storage, block_cache: RefCell::new(vec![None; BLOCK_COUNT]) })
    }

    fn parse_v1(data: &[u8]) -> Result<FlagStorage, String> {
        let offset_table_start = FlagFileHeader::SIZE;
        let offset_table_end = offset_table_start + (ENTRY_COUNT * 2);

        if data.len() < offset_table_end {
            return Err("Data too short for offset table".to_string());
        }

        let mut offset_table = Vec::with_capacity(ENTRY_COUNT);
        for i in 0..ENTRY_COUNT {
            let start = offset_table_start + (i * 2);
            let offset = u16::from_le_bytes([data[start], data[start + 1]]);
            offset_table.push(offset);
        }

        Ok(FlagStorage::V1 { offset_table })
    }

    fn parse_v2(data: &[u8]) -> Result<FlagStorage, String> {
        let entry_table_start = FlagFileHeader::SIZE;
        let entry_table_len = ENTRY_COUNT * 6;
        let entry_table_end = entry_table_start + entry_table_len;
        let block_table_len = BLOCK_COUNT * 12;
        let block_table_end = entry_table_end + block_table_len;

        if data.len() < block_table_end {
            return Err("Data too short for V2 metadata".to_string());
        }

        let mut entry_table = Vec::with_capacity(ENTRY_COUNT);
        for i in 0..ENTRY_COUNT {
            let start = entry_table_start + (i * 6);
            let local_offset = u32::from_le_bytes([data[start], data[start + 1], data[start + 2], data[start + 3]]);
            let svg_len = u16::from_le_bytes([data[start + 4], data[start + 5]]);
            entry_table.push(FlagDataEntry { local_offset, svg_len });
        }

        let mut block_table = Vec::with_capacity(BLOCK_COUNT);
        for i in 0..BLOCK_COUNT {
            let start = entry_table_end + (i * 12);
            let offset = u32::from_le_bytes([data[start], data[start + 1], data[start + 2], data[start + 3]]);
            let compressed_len =
                u32::from_le_bytes([data[start + 4], data[start + 5], data[start + 6], data[start + 7]]);
            let raw_len = u32::from_le_bytes([data[start + 8], data[start + 9], data[start + 10], data[start + 11]]);
            block_table.push(FlagBlockEntry { offset, compressed_len, raw_len });
        }

        Ok(FlagStorage::V2 { entry_table, block_table })
    }

    pub fn from_file<P: AsRef<std::path::Path>>(path: P) -> Result<Self, String> {
        let data = std::fs::read(path).map_err(|e| format!("Failed to read file: {e}"))?;
        Self::from_bytes(data)
    }

    pub fn get_flag(&self, country_code: &str) -> Option<String> {
        let upper_code = country_code.to_ascii_uppercase();
        let index = country_code_to_index(&upper_code)?;
        self.get_flag_by_index(index)
    }

    pub fn get_flag_by_index(&self, index: u16) -> Option<String> {
        match &self.storage {
            FlagStorage::V1 { offset_table } => self.get_v1_flag(index, offset_table),
            FlagStorage::V2 { entry_table, block_table } => self.get_v2_flag(index, entry_table, block_table),
        }
    }

    fn get_v1_flag(&self, index: u16, offset_table: &[u16]) -> Option<String> {
        let offset = *offset_table.get(index as usize)?;
        if offset == V1_ENTRY_SENTINEL {
            return None;
        }

        let offset = offset as usize;
        if self.data.len() < offset + 2 {
            return None;
        }

        let compressed_len = u16::from_le_bytes([self.data[offset], self.data[offset + 1]]) as usize;
        if self.data.len() < offset + 2 + compressed_len {
            return None;
        }

        let compressed_data = &self.data[offset + 2..offset + 2 + compressed_len];
        let decompressed = lz4_flex::decompress_size_prepended(compressed_data).ok()?;
        String::from_utf8(decompressed).ok()
    }

    fn get_v2_flag(&self, index: u16, entry_table: &[FlagDataEntry], block_table: &[FlagBlockEntry]) -> Option<String> {
        let entry = *entry_table.get(index as usize)?;
        if entry.local_offset == ENTRY_MISSING_OFFSET {
            return None;
        }

        let block_id = (index / 26) as usize;
        self.ensure_block_cached(block_id, block_table)?;
        let cache = self.block_cache.borrow();
        let block = cache.get(block_id)?.as_ref()?;
        let start = entry.local_offset as usize;
        let end = start.checked_add(entry.svg_len as usize)?;
        let svg = block.get(start..end)?;
        std::str::from_utf8(svg).ok().map(ToOwned::to_owned)
    }

    fn ensure_block_cached(&self, block_id: usize, block_table: &[FlagBlockEntry]) -> Option<()> {
        if self.block_cache.borrow().get(block_id).and_then(|entry| entry.as_ref()).is_some() {
            return Some(());
        }

        let block = *block_table.get(block_id)?;
        if block.raw_len == 0 {
            return None;
        }

        let start = block.offset as usize;
        let end = start.checked_add(block.compressed_len as usize)?;
        let compressed = self.data.get(start..end)?;

        let mut decompressed = Vec::with_capacity(block.raw_len as usize);
        let mut reader = Decompressor::new(compressed, 4096);
        reader.read_to_end(&mut decompressed).ok()?;

        if decompressed.len() != block.raw_len as usize {
            return None;
        }

        if let Some(slot) = self.block_cache.borrow_mut().get_mut(block_id) {
            *slot = Some(decompressed);
        }
        Some(())
    }

    pub fn get_all_flags(&self) -> Vec<FlagEntry> {
        let mut flags = Vec::new();
        for i in 0..ENTRY_COUNT as u16 {
            if let Some(code) = index_to_country_code(i) {
                if let Some(svg) = self.get_flag_by_index(i) {
                    flags.push(FlagEntry { country_code: code, svg_data: svg.into_bytes() });
                }
            }
        }
        flags
    }

    pub fn has_flag(&self, country_code: &str) -> bool {
        let upper_code = country_code.to_ascii_uppercase();
        let Some(idx) = country_code_to_index(&upper_code) else {
            return false;
        };

        match &self.storage {
            FlagStorage::V1 { offset_table } => offset_table[idx as usize] != V1_ENTRY_SENTINEL,
            FlagStorage::V2 { entry_table, .. } => entry_table[idx as usize].local_offset != ENTRY_MISSING_OFFSET,
        }
    }

    pub fn count(&self) -> usize {
        match &self.storage {
            FlagStorage::V1 { offset_table } => offset_table.iter().filter(|&&off| off != V1_ENTRY_SENTINEL).count(),
            FlagStorage::V2 { entry_table, .. } => {
                entry_table.iter().filter(|entry| entry.local_offset != ENTRY_MISSING_OFFSET).count()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brotli::CompressorWriter;
    use std::io::Write;

    fn build_v2_bytes(flags: &[(&str, &str)]) -> Vec<u8> {
        let mut entries = vec![FlagDataEntry { local_offset: ENTRY_MISSING_OFFSET, svg_len: 0 }; ENTRY_COUNT];
        let mut block_table = vec![FlagBlockEntry { offset: 0, compressed_len: 0, raw_len: 0 }; BLOCK_COUNT];
        let mut compressed_blocks = Vec::new();

        for block_id in 0..BLOCK_COUNT {
            let mut raw_block = Vec::new();
            for (code, svg) in flags {
                let index = country_code_to_index(code).unwrap();
                if (index / 26) as usize != block_id {
                    continue;
                }

                let local_offset = raw_block.len() as u32;
                let svg_bytes = svg.as_bytes();
                raw_block.extend_from_slice(svg_bytes);
                entries[index as usize] = FlagDataEntry { local_offset, svg_len: svg_bytes.len() as u16 };
            }

            if raw_block.is_empty() {
                continue;
            }

            let block_offset =
                (FlagFileHeader::SIZE + (ENTRY_COUNT * 6) + (BLOCK_COUNT * 12) + compressed_blocks.len()) as u32;
            let mut compressed = Vec::new();
            {
                let mut writer = CompressorWriter::new(&mut compressed, 4096, 9, 20);
                writer.write_all(&raw_block).unwrap();
            }

            block_table[block_id] = FlagBlockEntry {
                offset: block_offset,
                compressed_len: compressed.len() as u32,
                raw_len: raw_block.len() as u32,
            };
            compressed_blocks.extend_from_slice(&compressed);
        }

        let mut output = Vec::new();
        output.extend_from_slice(&FlagFileHeader::new(FlagVersion::V2, ENTRY_COUNT as u16).to_bytes());
        for entry in entries {
            output.extend_from_slice(&entry.local_offset.to_le_bytes());
            output.extend_from_slice(&entry.svg_len.to_le_bytes());
        }
        for block in block_table {
            output.extend_from_slice(&block.offset.to_le_bytes());
            output.extend_from_slice(&block.compressed_len.to_le_bytes());
            output.extend_from_slice(&block.raw_len.to_le_bytes());
        }
        output.extend_from_slice(&compressed_blocks);
        output
    }

    #[test]
    fn test_country_code_encoding() {
        assert_eq!(country_code_to_index("AA"), Some(0));
        assert_eq!(country_code_to_index("AB"), Some(1));
        assert_eq!(country_code_to_index("BA"), Some(26));
        assert_eq!(country_code_to_index("ZZ"), Some(675));
        assert_eq!(country_code_to_index("A"), None);
        assert_eq!(country_code_to_index("AAA"), None);
        assert_eq!(country_code_to_index("A1"), None);
        assert_eq!(country_code_to_index("1A"), None);
    }

    #[test]
    fn test_index_to_country_code() {
        assert_eq!(index_to_country_code(0), Some("AA".to_string()));
        assert_eq!(index_to_country_code(1), Some("AB".to_string()));
        assert_eq!(index_to_country_code(26), Some("BA".to_string()));
        assert_eq!(index_to_country_code(675), Some("ZZ".to_string()));
        assert_eq!(index_to_country_code(676), None);
    }

    #[test]
    fn test_round_trip() {
        for code in ["AA", "DE", "US", "ZZ", "AB", "ZY"] {
            let index = country_code_to_index(code).unwrap();
            let decoded = index_to_country_code(index).unwrap();
            assert_eq!(decoded, code);
        }
    }

    #[test]
    fn test_v2_round_trip() {
        let svg_de = "<svg viewBox='0 0 100 100'><rect fill='black' x='0' y='33' width='100' height='34'/><rect fill='red' x='0' y='0' width='100' height='33'/><rect fill='gold' x='0' y='67' width='100' height='33'/></svg>";
        let svg_us = "<svg viewBox='0 0 100 100'><rect fill='blue' width='100' height='100'/><rect fill='red' width='100' height='20'/><rect fill='white' width='100' height='20' y='20'/></svg>";

        let loader = FlagsLoader::from_bytes(build_v2_bytes(&[("DE", svg_de), ("US", svg_us)])).unwrap();

        assert_eq!(loader.get_flag("DE"), Some(svg_de.to_string()));
        assert_eq!(loader.get_flag("US"), Some(svg_us.to_string()));
        assert!(loader.get_flag("GB").is_none());
        assert_eq!(loader.count(), 2);
        assert!(loader.has_flag("de"));
        assert!(!loader.has_flag("GB"));
    }

    #[test]
    fn test_case_insensitive() {
        let svg = "<svg viewBox='0 0 100 100'><rect fill='red' width='100' height='100'/></svg>";
        let loader = FlagsLoader::from_bytes(build_v2_bytes(&[("DE", svg)])).unwrap();

        assert_eq!(loader.get_flag("DE"), Some(svg.to_string()));
        assert_eq!(loader.get_flag("de"), Some(svg.to_string()));
        assert_eq!(loader.get_flag("De"), Some(svg.to_string()));
    }
}

///////////
// Flags Builder creates a flags.dat file from a directory with svg files named with <countrycode>.svg
// This file is used by the frontend to display the flags.
//
// This tool is used to build the flags.dat file used by the frontend.
//
// flag svg can be found under https://github.com/lipis/flag-icons
//
// Usage:  cargo run --bin flags_builder
//
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
use brotli::CompressorWriter;
use shared::utils::{country_code_to_index, FlagBlockEntry, FlagDataEntry, FlagFileHeader, FlagVersion};
use std::{collections::HashMap, io::Write};

const SVG_FLAGS_DIR: &str = "/projects/flags/flag-icons/flags/4x3";
const FLAGS_FILE: &str = "/projects/tuliprox/frontend/public/assets/flags.dat";

const ENTRY_COUNT: usize = 676;
const BLOCK_COUNT: usize = 26;
const BLOCK_TABLE_SIZE: usize = BLOCK_COUNT * 12;
const ENTRY_TABLE_SIZE: usize = ENTRY_COUNT * 6;
const ENTRY_MISSING_OFFSET: u32 = u32::MAX;
const BROTLI_BUFFER_SIZE: usize = 4096;
const BROTLI_QUALITY: u32 = 9;
const BROTLI_LGWIN: u32 = 20;

pub struct FlagsBuilder {
    flags: HashMap<String, Vec<u8>>,
}

impl FlagsBuilder {
    pub fn new() -> Self { Self { flags: HashMap::new() } }

    pub fn add_flag(&mut self, country_code: &str, svg_data: &[u8]) -> Result<(), String> {
        if country_code.len() != 2 {
            return Err(format!("Invalid country code '{country_code}', must be 2 characters"));
        }
        let upper_code = country_code.to_ascii_uppercase();
        if country_code_to_index(&upper_code).is_none() {
            return Err(format!("Invalid country code '{country_code}'"));
        }
        self.flags.insert(upper_code, svg_data.to_vec());
        Ok(())
    }

    pub fn build<W: Write>(&self, mut writer: W) -> std::io::Result<()> {
        let header = FlagFileHeader::new(FlagVersion::V2, ENTRY_COUNT as u16);
        writer.write_all(&header.to_bytes())?;

        let mut entry_table = vec![FlagDataEntry { local_offset: ENTRY_MISSING_OFFSET, svg_len: 0 }; ENTRY_COUNT];
        let mut block_table = vec![FlagBlockEntry { offset: 0, compressed_len: 0, raw_len: 0 }; BLOCK_COUNT];
        let mut compressed_blocks = Vec::new();

        for (block_id, block_entry) in block_table.iter_mut().enumerate() {
            let mut block_entries = self
                .flags
                .iter()
                .filter_map(|(code, svg_data)| {
                    let index = country_code_to_index(code)?;
                    ((index / 26) as usize == block_id).then_some((index, svg_data.as_slice()))
                })
                .collect::<Vec<_>>();
            block_entries.sort_unstable_by_key(|(index, _)| *index);

            if block_entries.is_empty() {
                continue;
            }

            let raw_len = block_entries.iter().map(|(_, svg)| svg.len()).sum::<usize>();
            let mut raw_block = Vec::with_capacity(raw_len);

            for (index, svg_data) in block_entries {
                let local_offset = raw_block.len() as u32;
                raw_block.extend_from_slice(svg_data);
                entry_table[index as usize] = FlagDataEntry { local_offset, svg_len: svg_data.len() as u16 };
            }

            let mut compressed = Vec::new();
            {
                let mut compressor =
                    CompressorWriter::new(&mut compressed, BROTLI_BUFFER_SIZE, BROTLI_QUALITY, BROTLI_LGWIN);
                compressor.write_all(&raw_block)?;
            }

            let data_offset =
                (FlagFileHeader::SIZE + ENTRY_TABLE_SIZE + BLOCK_TABLE_SIZE + compressed_blocks.len()) as u32;
            *block_entry = FlagBlockEntry {
                offset: data_offset,
                compressed_len: compressed.len() as u32,
                raw_len: raw_block.len() as u32,
            };
            compressed_blocks.extend_from_slice(&compressed);
        }

        for entry in entry_table {
            writer.write_all(&entry.local_offset.to_le_bytes())?;
            writer.write_all(&entry.svg_len.to_le_bytes())?;
        }

        for block in block_table {
            writer.write_all(&block.offset.to_le_bytes())?;
            writer.write_all(&block.compressed_len.to_le_bytes())?;
            writer.write_all(&block.raw_len.to_le_bytes())?;
        }

        writer.write_all(&compressed_blocks)?;
        Ok(())
    }

    pub fn build_to_file<P: AsRef<std::path::Path>>(&self, path: P) -> std::io::Result<()> {
        let file = std::fs::File::create(path)?;
        self.build(file)
    }
}

impl Default for FlagsBuilder {
    fn default() -> Self { Self::new() }
}

pub fn main() -> Result<(), Box<dyn std::error::Error>> {
    let flags_dir = SVG_FLAGS_DIR;
    let output_file = FLAGS_FILE;

    println!("Create Flags from: {flags_dir}");

    let mut builder = FlagsBuilder::new();
    let mut count = 0;

    for entry in std::fs::read_dir(flags_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.extension().is_some_and(|e| e == "svg") {
            if let Some(stem) = path.file_stem() {
                let country_code = stem.to_string_lossy();

                match builder.add_flag(&country_code, &std::fs::read(&path)?) {
                    Ok(()) => {
                        count += 1;
                        println!("  {} added", country_code.to_ascii_uppercase());
                    }
                    Err(err) => {
                        println!("  {}: {}", country_code, err);
                    }
                }
            }
        }
    }

    println!("\nWriting {count} Flags to: {output_file}");
    builder.build_to_file(output_file)?;

    println!("Done");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::utils::{country_code_to_index, index_to_country_code, FlagFileHeader, FlagsLoader};

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
    fn test_build_flags() {
        let mut builder = FlagsBuilder::new();
        let svg1 = b"<svg viewBox='0 0 100 100'><rect fill='red' width='100' height='100'/></svg>";
        let svg2 = b"<svg viewBox='0 0 100 100'><rect fill='blue' width='100' height='100'/></svg>";

        builder.add_flag("DE", svg1).unwrap();
        builder.add_flag("US", svg2).unwrap();

        let mut buffer = Vec::new();
        builder.build(&mut buffer).unwrap();

        assert!(buffer.len() > FlagFileHeader::SIZE + ENTRY_TABLE_SIZE + BLOCK_TABLE_SIZE);
    }

    #[test]
    fn test_build_and_load_v2_format() {
        let mut builder = FlagsBuilder::new();
        let svg_de = b"<svg viewBox='0 0 100 100'><rect fill='black' x='0' y='33' width='100' height='34'/><rect fill='red' x='0' y='0' width='100' height='33'/><rect fill='gold' x='0' y='67' width='100' height='33'/></svg>";
        let svg_us = b"<svg viewBox='0 0 100 100'><rect fill='blue' width='100' height='100'/><rect fill='red' width='100' height='20'/><rect fill='white' width='100' height='20' y='20'/></svg>";

        builder.add_flag("DE", svg_de).unwrap();
        builder.add_flag("US", svg_us).unwrap();

        let mut buffer = Vec::new();
        builder.build(&mut buffer).unwrap();

        let loader = FlagsLoader::from_bytes(buffer).unwrap();
        assert_eq!(loader.get_flag("DE"), Some(String::from_utf8(svg_de.to_vec()).unwrap()));
        assert_eq!(loader.get_flag("US"), Some(String::from_utf8(svg_us.to_vec()).unwrap()));
    }
}

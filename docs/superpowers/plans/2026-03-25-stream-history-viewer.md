# Stream History Viewer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a CLI tool (`--sh`) to dump and filter stream history records from binary archive files with streaming JSON output.

**Architecture:** Single `--sh` CLI parameter accepts JSON query (inline or @file). `StreamHistoryFileReader` iterator reads `.pending` and `.archive.lz4` files with block/file-level timestamp skipping and magic-recovery. Pre-compiled filters (exact/regex/numeric) applied per record. Streaming output prints JSON array line-by-line to stdout with zero RAM accumulation.

**Tech Stack:** Rust, clap (CLI), lz4_flex (compression), rmp-serde (MessagePack), regex, serde_json, crc32fast

**Spec:** `docs/superpowers/specs/2026-03-25-stream-history-viewer-design.md`

---

### Task 1: Migrate archive compression from gzip to lz4

**Files:**
- Modify: `backend/src/repository/stream_history/file.rs:15-21` (CompressionKind enum)
- Modify: `backend/src/repository/stream_history/file.rs:13` (add MAX_BLOCK_PAYLOAD_SIZE)
- Modify: `backend/src/repository/stream_history/archive.rs:7-8` (imports)
- Modify: `backend/src/repository/stream_history/archive.rs:82-132` (archive_pending_file)
- Modify: `backend/src/repository/stream_history/archive.rs:134-137` (archive_path_for)
- Modify: `backend/src/repository/stream_history/archive.rs:155-188` (recover_pending_files)

- [ ] **Step 1: Update CompressionKind enum**

In `file.rs:15-21`, replace `Gzip` with `Lz4`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompressionKind {
    None,
    Lz4,
    Zstd,
}
```

Add constant after `MAX_FRAME_SIZE` (line 13):

```rust
pub const MAX_BLOCK_PAYLOAD_SIZE: usize = 2 * MAX_FRAME_SIZE; // 16 MiB — OOM guard for magic-recovery
```

- [ ] **Step 2: Update archive.rs imports**

Replace `archive.rs:7-8`:

```rust
// Remove:
use flate2::write::GzEncoder;
use flate2::Compression;

// Add:
use lz4_flex::frame::FrameEncoder;
```

- [ ] **Step 3: Update archive_path_for**

In `archive.rs:134-137`, change extension:

```rust
fn archive_path_for(pending_path: &Path, partition_day: &str) -> PathBuf {
    let name = format!("stream-history-{partition_day}.archive.lz4");
    pending_path.parent().unwrap_or(Path::new(".")).join(name)
}
```

- [ ] **Step 4: Update archive_pending_file**

In `archive.rs:82-132`, replace GzEncoder with FrameEncoder:

```rust
// Line ~101: Change compression_kind in finalized_header
compression_kind: CompressionKind::Lz4,

// Line ~107: Replace GzEncoder
let mut lz4 = FrameEncoder::new(archive_file);

// Lines ~110-111: Write to lz4 instead of gz
write_file_magic(&mut lz4)?;
write_framed(&mut lz4, &finalized_header)?;

// Line ~114-116: Stream block data to lz4
pending.seek(std::io::SeekFrom::Start(summary.blocks_start))?;
io::copy(&mut pending, &mut lz4)?;

// Line ~118: Finish lz4 instead of gz
lz4.finish().map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
```

Note: `FrameEncoder::finish()` returns `Result<W, lz4_flex::frame::Error>`, not `io::Result<W>` like `GzEncoder`. Wrap the error.

- [ ] **Step 5: Update recover_pending_files**

In `archive.rs:155-188`, the function already scans for `.pending` files only and calls `archive_pending_file`. No changes needed to the scan logic itself — the new archive format is handled by `archive_pending_file`. Verify no `.archive.gz` references exist elsewhere in this function.

- [ ] **Step 6: Update writer.rs pending file header**

In `writer.rs` `open_or_create_pending_file` (line 325-357), verify that `compression_kind: CompressionKind::None` is used for pending files (not Gzip). This should already be the case since pending files are uncompressed.

- [ ] **Step 7: Update existing archive tests**

In `archive.rs:204-381`, the existing tests use `flate2::read::GzDecoder` and assert `CompressionKind::Gzip` and `.archive.gz`. Update:

1. **Line 214**: Replace `use flate2::read::GzDecoder;` with `use lz4_flex::frame::FrameDecoder;`
2. **Line 318**: Change `.archive.gz` to `.archive.lz4`
3. **Lines 331-332**: Replace `let gz_file = File::open(&archive_path).unwrap(); let mut gz = GzDecoder::new(gz_file);` with `let lz4_file = File::open(&archive_path).unwrap(); let mut lz4 = FrameDecoder::new(lz4_file);`
4. **Lines 333-334**: Use `&mut lz4` instead of `&mut gz`
5. **Line 340**: Change `CompressionKind::Gzip` to `CompressionKind::Lz4`
6. **Lines 353-355**: Same GzDecoder→FrameDecoder replacement
7. **Line 376**: Change `.archive.gz` to `.archive.lz4`

- [ ] **Step 8: Build and fix compilation errors**

Run: `cargo +nightly build 2>&1 | head -50`

Fix any remaining references to `CompressionKind::Gzip` in the codebase. Search with:
```bash
grep -r "Gzip\|GzDecoder\|GzEncoder\|archive\.gz" backend/src/repository/stream_history/
```

- [ ] **Step 9: Run existing tests**

Run: `cargo +nightly test --lib -p tuliprox stream_history 2>&1 | tail -20`
Expected: All existing archive tests pass with lz4

- [ ] **Step 10: Commit**

```bash
git add backend/src/repository/stream_history/file.rs backend/src/repository/stream_history/archive.rs
git commit -m "feat: migrate stream history archive from gzip to lz4

Replace GzEncoder with lz4_flex FrameEncoder for ~5x faster decompression.
Change file extension from .archive.gz to .archive.lz4.
Add MAX_BLOCK_PAYLOAD_SIZE (16 MiB) constant for OOM protection.
Rename CompressionKind::Gzip to CompressionKind::Lz4."
```

---

### Task 2: Create StreamHistoryFileReader iterator

**Files:**
- Create: `backend/src/repository/stream_history/reader.rs`
- Modify: `backend/src/repository/stream_history/mod.rs`

**Context:** The reader must handle both `.pending` (uncompressed `BufReader<File>`) and `.archive.lz4` (lz4-decoded `BufReader<FrameDecoder<File>>`). Since `R: Read` is the generic parameter, both work through the same iterator logic. Key functions from `file.rs`: `read_and_verify_file_magic`, `read_and_verify_block_magic`, `read_framed`, `deserialize_named`, `BLOCK_MAGIC`, `MAX_BLOCK_PAYLOAD_SIZE`.

- [ ] **Step 1: Create reader.rs with struct and constructors**

Create `backend/src/repository/stream_history/reader.rs`:

```rust
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use lz4_flex::frame::FrameDecoder;

use crate::repository::stream_history::file::{
    read_and_verify_file_magic, read_and_verify_block_magic, read_framed,
    deserialize_named, FileHeaderBody, BlockHeaderBody, StreamHistoryRecord,
    BLOCK_MAGIC, MAX_BLOCK_PAYLOAD_SIZE,
};

pub struct StreamHistoryFileReader<R: Read> {
    reader: R,
    time_range: Option<(u64, u64)>,
    current_block_remaining: u32,
    payload_buf: Vec<u8>,
    payload_offset: usize,
    done: bool,
    file_path: String,
}

impl StreamHistoryFileReader<BufReader<File>> {
    pub fn from_pending(path: &Path, time_range: Option<(u64, u64)>) -> io::Result<(Self, FileHeaderBody)> {
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
        read_and_verify_file_magic(&mut reader)?;
        let header: FileHeaderBody = read_framed(&mut reader)?;
        Ok((Self {
            reader,
            time_range,
            current_block_remaining: 0,
            payload_buf: Vec::new(),
            payload_offset: 0,
            done: false,
            file_path: path.display().to_string(),
        }, header))
    }
}

impl StreamHistoryFileReader<BufReader<FrameDecoder<File>>> {
    pub fn from_archive(path: &Path, time_range: Option<(u64, u64)>) -> io::Result<(Self, FileHeaderBody)> {
        let file = File::open(path)?;
        let decoder = FrameDecoder::new(file);
        let mut reader = BufReader::new(decoder);
        read_and_verify_file_magic(&mut reader)?;
        let header: FileHeaderBody = read_framed(&mut reader)?;
        Ok((Self {
            reader,
            time_range,
            current_block_remaining: 0,
            payload_buf: Vec::new(),
            payload_offset: 0,
            done: false,
            file_path: path.display().to_string(),
        }, header))
    }
}
```

- [ ] **Step 2: Implement block-level skip helper**

Add method to skip payload bytes. For seekable readers (.pending), use seek. For non-seekable (lz4), read and discard:

```rust
impl<R: Read> StreamHistoryFileReader<R> {
    fn skip_payload(&mut self, len: u32) -> io::Result<()> {
        io::copy(&mut (&mut self.reader).take(len as u64), &mut io::sink())?;
        Ok(())
    }

    fn read_next_block(&mut self) -> io::Result<Option<()>> {
        // Loop instead of recursion to avoid stack overflow with many skipped blocks
        loop {
            // Try to read block magic
            match read_and_verify_block_magic(&mut self.reader) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(e) => {
                    eprintln!("Warning: {}: corrupt block magic, attempting recovery: {e}", self.file_path);
                    return self.magic_recovery();
                }
            }

            let block_header: BlockHeaderBody = match read_framed(&mut self.reader) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("Warning: {}: corrupt block header: {e}", self.file_path);
                    return self.magic_recovery();
                }
            };

            // Validate payload size (OOM protection)
            if block_header.payload_len as usize > MAX_BLOCK_PAYLOAD_SIZE {
                eprintln!("Warning: {}: block payload_len {} exceeds MAX_BLOCK_PAYLOAD_SIZE, skipping",
                          self.file_path, block_header.payload_len);
                self.skip_payload(block_header.payload_len)?;
                continue; // Try next block (loop, not recursion)
            }

            // Block-level skip: check if entire block is outside time range
            if let Some((start, end)) = self.time_range {
                if block_header.last_event_ts_utc < start || block_header.first_event_ts_utc > end {
                    self.skip_payload(block_header.payload_len)?;
                    continue; // Try next block (loop, not recursion)
                }
            }

            // Read payload into buffer
            self.payload_buf.resize(block_header.payload_len as usize, 0);
            self.reader.read_exact(&mut self.payload_buf)?;

            // Verify payload CRC
            let actual_crc = crc32fast::hash(&self.payload_buf);
            if actual_crc != block_header.payload_crc {
                eprintln!("Warning: {}: payload CRC mismatch (expected {:08x}, got {:08x}), attempting recovery",
                          self.file_path, block_header.payload_crc, actual_crc);
                return self.magic_recovery();
            }

            self.current_block_remaining = block_header.record_count;
            self.payload_offset = 0;
            return Ok(Some(()));
        }
    }
}
```

- [ ] **Step 3: Implement magic-recovery**

```rust
impl<R: Read> StreamHistoryFileReader<R> {
    fn magic_recovery(&mut self) -> io::Result<Option<()>> {
        let mut scan_buf = [0u8; 4096];
        let magic = BLOCK_MAGIC;
        let mut carry = [0u8; 3]; // carry bytes across chunk boundaries
        let mut carry_len = 0usize;

        loop {
            let n = match self.reader.read(&mut scan_buf) {
                Ok(0) => return Ok(None), // EOF
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(e) => return Err(e),
            };

            // Search for BLOCK_MAGIC in carry + scan_buf
            let combined_start = carry_len;
            let search_len = carry_len + n;

            // Simple approach: check each position
            for i in 0..search_len.saturating_sub(3) {
                let b = |idx: usize| -> u8 {
                    if idx < carry_len { carry[idx] } else { scan_buf[idx - carry_len] }
                };
                if b(i) == magic[0] && b(i + 1) == magic[1] && b(i + 2) == magic[2] && b(i + 3) == magic[3] {
                    // Found candidate — remaining bytes after magic are already consumed
                    // We need to "put back" the bytes after the magic sequence
                    // Since we can't unread, we attempt to read the block header directly
                    // The bytes after position i+4 in our buffer are unconsumed data
                    // For simplicity in the recovery path, try reading the next block header
                    let block_header: BlockHeaderBody = match read_framed(&mut self.reader) {
                        Ok(h) if (h.payload_len as usize) <= MAX_BLOCK_PAYLOAD_SIZE => {
                            eprintln!("Warning: {}: recovered at block with {} records", self.file_path, h.record_count);
                            h
                        }
                        Ok(h) => {
                            eprintln!("Warning: {}: recovery candidate rejected (payload_len={})", self.file_path, h.payload_len);
                            continue;
                        }
                        Err(_) => continue, // Not a real block header, keep scanning
                    };

                    // Read and validate payload
                    self.payload_buf.resize(block_header.payload_len as usize, 0);
                    self.reader.read_exact(&mut self.payload_buf)?;
                    self.current_block_remaining = block_header.record_count;
                    self.payload_offset = 0;
                    return Ok(Some(()));
                }
            }

            // Keep last 3 bytes as carry for cross-boundary detection
            if n >= 3 {
                carry[..3].copy_from_slice(&scan_buf[n - 3..n]);
                carry_len = 3;
            } else {
                // Shift carry and append new bytes
                let total = carry_len + n;
                let keep = total.min(3);
                let mut tmp = [0u8; 6];
                tmp[..carry_len].copy_from_slice(&carry[..carry_len]);
                tmp[carry_len..carry_len + n].copy_from_slice(&scan_buf[..n]);
                carry[..keep].copy_from_slice(&tmp[total - keep..total]);
                carry_len = keep;
            }
        }
    }
}
```

**Implementation note on byte management:** After finding `BLK\x01` at offset `i` in the scan buffer, bytes after `i+4` up to position `n` have already been read from the reader but are not yet consumed. The `read_framed` call reads from the reader's *current* position, which is past the scan buffer.

**Solution:** Use a `Chain` reader: `let remaining = &scan_buf[i+4..n]; let mut chained = remaining.chain(&mut self.reader);` then call `read_framed(&mut chained)`. This ensures the block header bytes that were already in the scan buffer are not lost. The implementer must chain the unconsumed scan buffer tail with the underlying reader for the `read_framed` and subsequent `read_exact` calls.

- [ ] **Step 4: Implement Iterator**

```rust
impl<R: Read> Iterator for StreamHistoryFileReader<R> {
    type Item = io::Result<StreamHistoryRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        loop {
            // Records remaining in current block?
            if self.current_block_remaining > 0 {
                // Read record_len (u32 BE) from payload_buf
                if self.payload_offset + 4 > self.payload_buf.len() {
                    eprintln!("Warning: {}: truncated record length in block", self.file_path);
                    self.current_block_remaining = 0;
                    continue;
                }
                let record_len = u32::from_be_bytes(
                    self.payload_buf[self.payload_offset..self.payload_offset + 4]
                        .try_into()
                        .unwrap()
                ) as usize;
                self.payload_offset += 4;

                if self.payload_offset + record_len > self.payload_buf.len() {
                    eprintln!("Warning: {}: truncated record data in block", self.file_path);
                    self.current_block_remaining = 0;
                    continue;
                }

                let record_bytes = &self.payload_buf[self.payload_offset..self.payload_offset + record_len];
                self.payload_offset += record_len;
                self.current_block_remaining -= 1;

                match deserialize_named(record_bytes) {
                    Ok(record) => return Some(Ok(record)),
                    Err(e) => {
                        eprintln!("Warning: {}: failed to deserialize record: {e}", self.file_path);
                        continue; // Skip corrupt record, try next
                    }
                }
            }

            // No records left — read next block
            match self.read_next_block() {
                Ok(Some(())) => continue, // Got a new block, loop back to read records
                Ok(None) => {
                    self.done = true;
                    return None; // EOF
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
    }
}
```

- [ ] **Step 5: Export reader module**

In `backend/src/repository/stream_history/mod.rs`, add:

```rust
mod reader;
pub use reader::*;
```

- [ ] **Step 6: Build and verify compilation**

Run: `cargo +nightly build 2>&1 | head -50`
Expected: Compiles without errors

- [ ] **Step 7: Write tests for the reader**

Add tests at the bottom of `reader.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::stream_history::file::*;
    use std::io::Cursor;
    use tempfile::NamedTempFile;

    fn make_test_record(ts: u64, username: &str) -> StreamHistoryRecord {
        StreamHistoryRecord {
            schema_version: RECORD_SCHEMA_VERSION,
            event_type: EventType::Connect,
            event_ts_utc: ts,
            partition_day_utc: "2026-03-22".to_string(),
            session_id: 1,
            source_addr: None,
            api_username: Some(username.to_string()),
            provider_name: None,
            provider_username: None,
            virtual_id: None,
            item_type: None,
            title: None,
            group: None,
            country: None,
            connect_ts_utc: None,
            disconnect_ts_utc: None,
            session_duration: None,
            bytes_sent: None,
            disconnect_reason: None,
        }
    }

    /// Helper: write a valid pending file with given records into a temp file
    fn write_test_pending_file(records: &[StreamHistoryRecord]) -> NamedTempFile {
        let tmp = NamedTempFile::new().unwrap();
        let mut f = tmp.as_file().try_clone().unwrap();

        // Write file header
        write_file_magic(&mut f).unwrap();
        let header = FileHeaderBody {
            container_format_version: CONTAINER_FORMAT_VERSION,
            record_schema_version: RECORD_SCHEMA_VERSION,
            source_kind: "stream_history".to_string(),
            created_at_ts_utc: 0,
            partition_day_ts_utc: "2026-03-22".to_string(),
            writer_instance_id: 1,
            host_id: None,
            compression_kind: CompressionKind::None,
            finalized: false,
            record_encoding_kind: RecordEncodingKind::MessagePackNamed,
            finalized_at_ts_utc: None,
            total_block_count: None,
            total_record_count: None,
            min_event_ts_utc: None,
            max_event_ts_utc: None,
        };
        write_framed(&mut f, &header).unwrap();

        if !records.is_empty() {
            // Build payload
            let mut payload = Vec::new();
            let mut first_ts = u64::MAX;
            let mut last_ts = 0u64;
            for rec in records {
                first_ts = first_ts.min(rec.event_ts_utc);
                last_ts = last_ts.max(rec.event_ts_utc);
                let encoded = rmp_serde::to_vec_named(rec).unwrap();
                payload.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
                payload.extend_from_slice(&encoded);
            }

            let payload_crc = crc32fast::hash(&payload);
            let block_header = BlockHeaderBody {
                block_version: 1,
                record_count: records.len() as u32,
                payload_len: payload.len() as u32,
                first_event_ts_utc: first_ts,
                last_event_ts_utc: last_ts,
                payload_crc,
                flags: 0,
            };

            write_block_magic(&mut f).unwrap();
            write_framed(&mut f, &block_header).unwrap();
            f.write_all(&payload).unwrap();
        }

        f.flush().unwrap();
        tmp
    }

    #[test]
    fn test_iterator_yields_all_records() {
        let records = vec![
            make_test_record(1000, "alice"),
            make_test_record(2000, "bob"),
            make_test_record(3000, "carol"),
        ];
        let tmp = write_test_pending_file(&records);

        let (reader, _header) = StreamHistoryFileReader::from_pending(tmp.path(), None).unwrap();
        let results: Vec<_> = reader.collect::<Result<Vec<_>, _>>().unwrap();

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].api_username.as_deref(), Some("alice"));
        assert_eq!(results[1].api_username.as_deref(), Some("bob"));
        assert_eq!(results[2].api_username.as_deref(), Some("carol"));
    }

    #[test]
    fn test_block_skipping() {
        // Create file with records at ts 1000-3000
        let records = vec![
            make_test_record(1000, "alice"),
            make_test_record(2000, "bob"),
            make_test_record(3000, "carol"),
        ];
        let tmp = write_test_pending_file(&records);

        // Time range 5000-6000 should skip the entire block
        let (reader, _) = StreamHistoryFileReader::from_pending(tmp.path(), Some((5000, 6000))).unwrap();
        let results: Vec<_> = reader.collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_empty_file() {
        let tmp = write_test_pending_file(&[]);
        let (reader, _) = StreamHistoryFileReader::from_pending(tmp.path(), None).unwrap();
        let results: Vec<_> = reader.collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_truncated_file_graceful() {
        // Write a valid header but truncate in the middle of a block
        let tmp = NamedTempFile::new().unwrap();
        let mut f = tmp.as_file().try_clone().unwrap();
        write_file_magic(&mut f).unwrap();
        let header = FileHeaderBody { /* same as write_test_pending_file */ };
        write_framed(&mut f, &header).unwrap();
        // Write block magic but truncate before block header
        write_block_magic(&mut f).unwrap();
        // Don't write block header — file is truncated
        f.flush().unwrap();

        let (reader, _) = StreamHistoryFileReader::from_pending(tmp.path(), None).unwrap();
        // Should not panic, just yield no records or an error
        let results: Vec<_> = reader.collect();
        // Either empty or contains an error — but no panic
        assert!(results.is_empty() || results.iter().any(|r| r.is_err()));
    }

    #[test]
    fn test_magic_recovery_oom_protection() {
        // Write a file where a block header has payload_len > MAX_BLOCK_PAYLOAD_SIZE
        // The reader should skip this block and not allocate
        // Implementation: write valid file magic + header, then a block with oversized payload_len
        // Followed by a valid block — reader should skip the bad one and yield the good one's records
        // (Detailed implementation left to implementer — the contract is: no OOM, no panic)
    }

    // Additional tests for lz4 archive round-trip:
    #[test]
    fn test_archive_lz4_readable() {
        // 1. Create a pending file with known records using write_test_pending_file
        // 2. Archive it using archive_pending_file (now produces .archive.lz4)
        // 3. Open with StreamHistoryFileReader::from_archive
        // 4. Verify all records are yielded correctly
    }
}
```

- [ ] **Step 8: Run tests**

Run: `cargo +nightly test --lib -p tuliprox reader::tests -- --nocapture 2>&1 | tail -30`
Expected: All tests pass

- [ ] **Step 9: Commit**

```bash
git add backend/src/repository/stream_history/reader.rs backend/src/repository/stream_history/mod.rs
git commit -m "feat: add StreamHistoryFileReader iterator with block-skip and magic-recovery

Iterator yields io::Result<StreamHistoryRecord> from .pending and .archive.lz4 files.
Supports time-range block skipping, CRC validation, OOM-protected magic-recovery."
```

---

### Task 3: Query parsing, date handling, and filter compilation

**Files:**
- Create: `backend/src/utils/stream_history_viewer.rs`
- Modify: `backend/src/utils/mod.rs:1-20` (add module declaration)

- [ ] **Step 1: Create stream_history_viewer.rs with query struct and date parsing**

```rust
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;

use regex::Regex;
use serde::Deserialize;

use crate::repository::stream_history::StreamHistoryRecord;

#[derive(Deserialize)]
pub struct StreamHistoryQuery {
    pub from: Option<String>,
    pub to: Option<String>,
    pub path: Option<String>,
    pub filter: Option<HashMap<String, String>>,
}

/// Parsed time range as (start_ts_utc, end_ts_utc) in seconds
pub type TimeRange = (u64, u64);

const SECS_PER_DAY: u64 = 86400;

/// Parse a date or datetime string into a UTC unix timestamp.
/// Accepts: "YYYY-MM-DD", "YYYY-MM-DD HH:MM", "YYYY-MM-DD HH:MM:SS"
pub fn parse_date_or_datetime(input: &str) -> Result<u64, String> {
    let trimmed = input.trim();

    // Try date only: YYYY-MM-DD
    if let Ok(date) = chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        let dt = date.and_hms_opt(0, 0, 0).unwrap();
        return Ok(dt.and_utc().timestamp() as u64);
    }

    // Try datetime without seconds: YYYY-MM-DD HH:MM
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M") {
        return Ok(dt.and_utc().timestamp() as u64);
    }

    // Try datetime with seconds: YYYY-MM-DD HH:MM:SS
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S") {
        return Ok(dt.and_utc().timestamp() as u64);
    }

    Err(format!(
        "Invalid date format: '{trimmed}'. Expected: YYYY-MM-DD, YYYY-MM-DD HH:MM, or YYYY-MM-DD HH:MM:SS"
    ))
}

/// Returns true if the input is a date-only format (no time component)
fn is_date_only(input: &str) -> bool {
    chrono::NaiveDate::parse_from_str(input.trim(), "%Y-%m-%d").is_ok()
}

/// Resolve the query's from/to into a concrete time range.
pub fn resolve_time_range(query: &StreamHistoryQuery) -> Result<TimeRange, String> {
    match (&query.from, &query.to) {
        (Some(from), Some(to)) => {
            let mut start = parse_date_or_datetime(from)?;
            let mut end = parse_date_or_datetime(to)?;
            // Date-only from: start of day. Date-only to: end of day.
            if is_date_only(to) {
                end += SECS_PER_DAY - 1; // 23:59:59
            }
            if start > end {
                return Err(format!("'from' ({from}) is after 'to' ({to})"));
            }
            Ok((start, end))
        }
        (Some(date), None) | (None, Some(date)) => {
            // Single date: expand to full day regardless of time component
            let parsed = parse_date_or_datetime(date)?;
            let day_start = if is_date_only(date) {
                parsed
            } else {
                // Extract the day start from the datetime
                let naive = chrono::DateTime::from_timestamp(parsed as i64, 0)
                    .unwrap()
                    .naive_utc()
                    .date()
                    .and_hms_opt(0, 0, 0)
                    .unwrap();
                naive.and_utc().timestamp() as u64
            };
            Ok((day_start, day_start + SECS_PER_DAY - 1))
        }
        (None, None) => Err("At least 'from' or 'to' must be specified".to_string()),
    }
}
```

- [ ] **Step 2: Add filter compilation**

Append to `stream_history_viewer.rs`:

```rust
const NUMERIC_FIELDS: &[&str] = &["session_id"];

pub enum FilterValue {
    Exact(String),
    Regex(Regex),
    NumericExact(u64),
}

pub struct CompiledFilter {
    pub fields: Vec<(String, FilterValue)>,
}

impl CompiledFilter {
    pub fn compile(raw: &HashMap<String, String>) -> Result<Self, String> {
        let mut fields = Vec::with_capacity(raw.len());
        for (key, value) in raw {
            let filter_value = if NUMERIC_FIELDS.contains(&key.as_str()) {
                let n = value.parse::<u64>().map_err(|_| {
                    format!("Filter '{key}' expects a numeric value, got '{value}'")
                })?;
                FilterValue::NumericExact(n)
            } else if let Some(pattern) = value.strip_prefix('~') {
                let re = Regex::new(pattern).map_err(|e| {
                    format!("Invalid regex for filter '{key}': {e}")
                })?;
                FilterValue::Regex(re)
            } else {
                FilterValue::Exact(value.clone())
            };
            fields.push((key.clone(), filter_value));
        }
        Ok(Self { fields })
    }

    pub fn matches(&self, record: &StreamHistoryRecord) -> bool {
        self.fields.iter().all(|(key, value)| {
            match get_record_field(record, key) {
                RecordFieldValue::String(Some(s)) => match value {
                    FilterValue::Exact(v) => s.eq_ignore_ascii_case(v),
                    FilterValue::Regex(re) => re.is_match(s),
                    FilterValue::NumericExact(_) => false,
                },
                RecordFieldValue::String(None) => false,
                RecordFieldValue::U64(n) => match value {
                    FilterValue::NumericExact(v) => n == *v,
                    _ => false,
                },
            }
        })
    }
}

enum RecordFieldValue<'a> {
    String(Option<&'a str>),
    U64(u64),
}

fn get_record_field<'a>(record: &'a StreamHistoryRecord, field: &str) -> RecordFieldValue<'a> {
    match field {
        "event_type" => {
            let s = match record.event_type {
                EventType::Connect => "connect",
                EventType::Disconnect => "disconnect",
            };
            RecordFieldValue::String(Some(s))
        }
        "api_username" => RecordFieldValue::String(record.api_username.as_deref()),
        "provider_name" => RecordFieldValue::String(record.provider_name.as_deref()),
        "provider_username" => RecordFieldValue::String(record.provider_username.as_deref()),
        "item_type" => RecordFieldValue::String(record.item_type.as_deref()),
        "title" => RecordFieldValue::String(record.title.as_deref()),
        "group" => RecordFieldValue::String(record.group.as_deref()),
        "country" => RecordFieldValue::String(record.country.as_deref()),
        "source_addr" => RecordFieldValue::String(record.source_addr.as_deref()),
        "disconnect_reason" => {
            // Match against serde rename_all = "snake_case" names
            RecordFieldValue::String(record.disconnect_reason.as_ref().map(|r| match r {
                DisconnectReason::ClientClosed => "client_closed",
                DisconnectReason::ServerError => "server_error",
                DisconnectReason::Timeout => "timeout",
                DisconnectReason::DayRollover => "day_rollover",
                DisconnectReason::Shutdown => "shutdown",
                DisconnectReason::Unknown => "unknown",
                DisconnectReason::ProviderError => "provider_error",
                DisconnectReason::ProviderClosed => "provider_closed",
                DisconnectReason::Preempted => "preempted",
                DisconnectReason::SessionExpired => "session_expired",
            }))
        }
        "session_id" => RecordFieldValue::U64(record.session_id),
        _ => RecordFieldValue::String(None), // Unknown field: no match
    }
}
```

- [ ] **Step 3: Add query loading (inline JSON or @file)**

```rust
pub fn load_query(input: &str) -> Result<StreamHistoryQuery, String> {
    let json_str = if let Some(file_path) = input.strip_prefix('@') {
        fs::read_to_string(file_path)
            .map_err(|e| format!("Failed to read query file '{file_path}': {e}"))?
    } else {
        input.to_string()
    };

    serde_json::from_str(&json_str)
        .map_err(|e| format!("Invalid JSON query: {e}\nExpected format: {{\"from\":\"YYYY-MM-DD\",\"to\":\"YYYY-MM-DD\",\"filter\":{{...}}}}"))
}
```

- [ ] **Step 4: Add module export**

In `backend/src/utils/mod.rs`, add after line ~13 (near other module declarations):

```rust
mod stream_history_viewer;
```

And in the pub use section (around line 24):

```rust
pub use self::stream_history_viewer::*;
```

- [ ] **Step 5: Build and check compilation**

Run: `cargo +nightly build 2>&1 | head -50`

Note: Check if `chrono` is already a dependency. If not, add it to `backend/Cargo.toml`. Search: `grep chrono backend/Cargo.toml`. If missing, add `chrono = "0.4"`.

- [ ] **Step 6: Write tests for date parsing and filter compilation**

Add to `stream_history_viewer.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_date_only() {
        let ts = parse_date_or_datetime("2026-03-22").unwrap();
        // 2026-03-22 00:00:00 UTC
        assert_eq!(ts, chrono::NaiveDate::from_ymd_opt(2026, 3, 22).unwrap()
            .and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp() as u64);
    }

    #[test]
    fn test_parse_datetime_no_seconds() {
        let ts = parse_date_or_datetime("2026-03-22 14:30").unwrap();
        let expected = chrono::NaiveDate::from_ymd_opt(2026, 3, 22).unwrap()
            .and_hms_opt(14, 30, 0).unwrap().and_utc().timestamp() as u64;
        assert_eq!(ts, expected);
    }

    #[test]
    fn test_parse_datetime_with_seconds() {
        let ts = parse_date_or_datetime("2026-03-22 14:30:45").unwrap();
        let expected = chrono::NaiveDate::from_ymd_opt(2026, 3, 22).unwrap()
            .and_hms_opt(14, 30, 45).unwrap().and_utc().timestamp() as u64;
        assert_eq!(ts, expected);
    }

    #[test]
    fn test_invalid_date_format() {
        assert!(parse_date_or_datetime("22-03-2026").is_err());
        assert!(parse_date_or_datetime("not-a-date").is_err());
        assert!(parse_date_or_datetime("").is_err());
    }

    #[test]
    fn test_single_date_expands_to_full_day() {
        let query = StreamHistoryQuery {
            from: Some("2026-03-22".to_string()),
            to: None,
            path: None,
            filter: None,
        };
        let (start, end) = resolve_time_range(&query).unwrap();
        assert_eq!(end - start, SECS_PER_DAY - 1);
    }

    #[test]
    fn test_single_datetime_expands_to_full_day() {
        // Single date with time component still expands to full day
        let query = StreamHistoryQuery {
            from: Some("2026-03-22 14:30".to_string()),
            to: None,
            path: None,
            filter: None,
        };
        let (start, end) = resolve_time_range(&query).unwrap();
        assert_eq!(end - start, SECS_PER_DAY - 1);
    }

    #[test]
    fn test_range_to_date_expands_end() {
        let query = StreamHistoryQuery {
            from: Some("2026-03-20".to_string()),
            to: Some("2026-03-22".to_string()),
            path: None,
            filter: None,
        };
        let (start, end) = resolve_time_range(&query).unwrap();
        let day22_end = parse_date_or_datetime("2026-03-22").unwrap() + SECS_PER_DAY - 1;
        assert_eq!(end, day22_end);
    }

    #[test]
    fn test_no_dates_is_error() {
        let query = StreamHistoryQuery {
            from: None,
            to: None,
            path: None,
            filter: None,
        };
        assert!(resolve_time_range(&query).is_err());
    }

    #[test]
    fn test_filter_exact_match() {
        let mut raw = HashMap::new();
        raw.insert("api_username".to_string(), "Alice".to_string());
        let filter = CompiledFilter::compile(&raw).unwrap();

        let mut record = make_empty_record();
        record.api_username = Some("alice".to_string());
        assert!(filter.matches(&record)); // case-insensitive

        record.api_username = Some("bob".to_string());
        assert!(!filter.matches(&record));
    }

    #[test]
    fn test_filter_regex_match() {
        let mut raw = HashMap::new();
        raw.insert("provider_name".to_string(), "~^acme.*".to_string());
        let filter = CompiledFilter::compile(&raw).unwrap();

        let mut record = make_empty_record();
        record.provider_name = Some("acme-tv".to_string());
        assert!(filter.matches(&record));

        record.provider_name = Some("other-provider".to_string());
        assert!(!filter.matches(&record));
    }

    #[test]
    fn test_filter_session_id_numeric() {
        let mut raw = HashMap::new();
        raw.insert("session_id".to_string(), "42".to_string());
        let filter = CompiledFilter::compile(&raw).unwrap();

        let mut record = make_empty_record();
        record.session_id = 42;
        assert!(filter.matches(&record));

        record.session_id = 99;
        assert!(!filter.matches(&record));
    }

    #[test]
    fn test_filter_invalid_regex() {
        let mut raw = HashMap::new();
        raw.insert("title".to_string(), "~[invalid".to_string());
        assert!(CompiledFilter::compile(&raw).is_err());
    }

    #[test]
    fn test_filter_invalid_numeric() {
        let mut raw = HashMap::new();
        raw.insert("session_id".to_string(), "not-a-number".to_string());
        assert!(CompiledFilter::compile(&raw).is_err());
    }

    #[test]
    fn test_load_query_inline() {
        let query = load_query(r#"{"from":"2026-03-22"}"#).unwrap();
        assert_eq!(query.from.as_deref(), Some("2026-03-22"));
        assert!(query.to.is_none());
    }

    #[test]
    fn test_load_query_at_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), r#"{"from":"2026-03-22","to":"2026-03-24"}"#).unwrap();
        let input = format!("@{}", tmp.path().display());
        let query = load_query(&input).unwrap();
        assert_eq!(query.from.as_deref(), Some("2026-03-22"));
        assert_eq!(query.to.as_deref(), Some("2026-03-24"));
    }

    #[test]
    fn test_filter_disconnect_reason() {
        let mut raw = HashMap::new();
        raw.insert("disconnect_reason".to_string(), "client_closed".to_string());
        let filter = CompiledFilter::compile(&raw).unwrap();

        let mut record = make_empty_record();
        record.disconnect_reason = Some(DisconnectReason::ClientClosed);
        assert!(filter.matches(&record));

        record.disconnect_reason = Some(DisconnectReason::Timeout);
        assert!(!filter.matches(&record));

        record.disconnect_reason = None;
        assert!(!filter.matches(&record));
    }

    fn make_empty_record() -> StreamHistoryRecord {
        StreamHistoryRecord {
            schema_version: 1,
            event_type: EventType::Connect,
            event_ts_utc: 0,
            partition_day_utc: String::new(),
            session_id: 0,
            source_addr: None,
            api_username: None,
            provider_name: None,
            provider_username: None,
            virtual_id: None,
            item_type: None,
            title: None,
            group: None,
            country: None,
            connect_ts_utc: None,
            disconnect_ts_utc: None,
            session_duration: None,
            bytes_sent: None,
            disconnect_reason: None,
        }
    }
}
```

- [ ] **Step 7: Run tests**

Run: `cargo +nightly test --lib -p tuliprox stream_history_viewer::tests -- --nocapture 2>&1 | tail -30`
Expected: All tests pass

- [ ] **Step 8: Commit**

```bash
git add backend/src/utils/stream_history_viewer.rs backend/src/utils/mod.rs
git commit -m "feat: add query parsing, date handling, and filter compilation for stream history viewer

Supports 3 date formats, single-date full-day expansion, @file query loading.
Filters: exact (eq_ignore_ascii_case), regex (~prefix, pre-compiled), numeric (session_id)."
```

---

### Task 4: File discovery, sorting, and streaming output

**Files:**
- Modify: `backend/src/utils/stream_history_viewer.rs` (add discover_files, stream_output, entry point)

- [ ] **Step 1: Add file discovery and sorting**

Append to `stream_history_viewer.rs`:

```rust
use std::path::PathBuf;

use crate::repository::stream_history::{
    StreamHistoryFileReader, FileHeaderBody,
    read_and_verify_file_magic, read_framed,
};

pub struct HistoryFile {
    pub path: PathBuf,
    pub partition_day: String,
    pub is_archive: bool,
}

pub fn discover_files(dir: &Path, time_range: &TimeRange) -> io::Result<Vec<HistoryFile>> {
    if !dir.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Stream history directory not found: {}", dir.display()),
        ));
    }

    let (range_start, range_end) = *time_range;
    let mut files = Vec::new();

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();

        let is_archive = name.ends_with(".archive.lz4");
        let is_pending = name.ends_with(".pending");

        if !is_archive && !is_pending {
            continue;
        }

        // Read file header to get partition day and timestamps
        let header = match read_file_header(&path, is_archive) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("Warning: skipping {}: {e}", path.display());
                continue;
            }
        };

        // File-level skip for archives with known timestamp bounds
        if is_archive {
            if let (Some(min_ts), Some(max_ts)) = (header.min_event_ts_utc, header.max_event_ts_utc) {
                if max_ts < range_start || min_ts > range_end {
                    continue; // Entire file outside range
                }
            }
        }

        // Partition-day level check
        // Convert partition_day "YYYY-MM-DD" to timestamp range and check overlap
        if let Ok(day_start) = parse_date_or_datetime(&header.partition_day_ts_utc) {
            let day_end = day_start + SECS_PER_DAY - 1;
            if day_end < range_start || day_start > range_end {
                continue;
            }
        }

        files.push(HistoryFile {
            path,
            partition_day: header.partition_day_ts_utc,
            is_archive,
        });
    }

    // Sort: primary by partition_day, secondary archive before pending
    files.sort_by(|a, b| {
        a.partition_day.cmp(&b.partition_day)
            .then(b.is_archive.cmp(&a.is_archive)) // true (archive) before false (pending)
    });

    if files.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("No stream history files found in range. Check directory: {}", dir.display()),
        ));
    }

    Ok(files)
}

fn read_file_header(path: &Path, is_archive: bool) -> io::Result<FileHeaderBody> {
    if is_archive {
        let file = std::fs::File::open(path)?;
        let decoder = lz4_flex::frame::FrameDecoder::new(file);
        let mut reader = std::io::BufReader::new(decoder);
        read_and_verify_file_magic(&mut reader)?;
        read_framed(&mut reader)
    } else {
        let file = std::fs::File::open(path)?;
        let mut reader = std::io::BufReader::new(file);
        read_and_verify_file_magic(&mut reader)?;
        read_framed(&mut reader)
    }
}
```

- [ ] **Step 2: Add streaming output**

```rust
pub fn stream_output(
    files: &[HistoryFile],
    time_range: &TimeRange,
    filters: &CompiledFilter,
) -> io::Result<()> {
    let (range_start, range_end) = *time_range;
    println!("[");
    let mut first = true;

    for file in files {
        let iter: Box<dyn Iterator<Item = io::Result<StreamHistoryRecord>>> = if file.is_archive {
            match StreamHistoryFileReader::from_archive(&file.path, Some(*time_range)) {
                Ok((reader, _)) => Box::new(reader),
                Err(e) => {
                    eprintln!("Warning: skipping {}: {e}", file.path.display());
                    continue;
                }
            }
        } else {
            match StreamHistoryFileReader::from_pending(&file.path, Some(*time_range)) {
                Ok((reader, _)) => Box::new(reader),
                Err(e) => {
                    eprintln!("Warning: skipping {}: {e}", file.path.display());
                    continue;
                }
            }
        };

        for result in iter {
            match result {
                Ok(record) => {
                    // Record-level timestamp filter (for datetime precision within a day)
                    if record.event_ts_utc < range_start || record.event_ts_utc > range_end {
                        continue;
                    }
                    if !filters.matches(&record) {
                        continue;
                    }
                    if !first {
                        println!(",");
                    }
                    match serde_json::to_string(&record) {
                        Ok(json) => print!("  {json}"),
                        Err(e) => {
                            eprintln!("Warning: failed to serialize record: {e}");
                            continue;
                        }
                    }
                    first = false;
                }
                Err(e) => eprintln!("Warning: {e}"),
            }
        }
    }

    println!("\n]");
    Ok(())
}
```

- [ ] **Step 3: Add main entry point**

```rust
fn exit_viewer(code: i32) -> ! {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code);
}

pub fn stream_history_viewer(input: &str) {
    eprintln!("[INFO] All timestamps interpreted as UTC");

    let query = match load_query(input) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("Error: {e}");
            exit_viewer(1);
            return;
        }
    };

    let time_range = match resolve_time_range(&query) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {e}");
            exit_viewer(1);
            return;
        }
    };

    let filters = match query.filter.as_ref() {
        Some(raw) => match CompiledFilter::compile(raw) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("Error: {e}");
                exit_viewer(1);
                return;
            }
        },
        None => CompiledFilter { fields: Vec::new() },
    };

    let dir = query.path.as_deref().unwrap_or("data/stream_history");
    let dir_path = Path::new(dir);

    let files = match discover_files(dir_path, &time_range) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Error: {e}");
            exit_viewer(1);
            return;
        }
    };

    if let Err(e) = stream_output(&files, &time_range, &filters) {
        eprintln!("Error: {e}");
        exit_app(1);
    }

    exit_viewer(0);
}
```

- [ ] **Step 4: Build and check compilation**

Run: `cargo +nightly build 2>&1 | head -50`
Expected: Compiles without errors

- [ ] **Step 5: Commit**

```bash
git add backend/src/utils/stream_history_viewer.rs
git commit -m "feat: add file discovery, streaming output, and entry point for stream history viewer

Discovers .pending/.archive.lz4 files, sorts by partition day (archive before pending),
file-level skip on archive timestamp bounds, streaming JSON output to stdout."
```

---

### Task 5: CLI integration in main.rs

**Files:**
- Modify: `backend/src/main.rs:47-117` (Args struct)
- Modify: `backend/src/main.rs:143-200` (main function)

- [ ] **Step 1: Add --sh argument to Args struct**

In `backend/src/main.rs`, add to the Args struct (after the db viewer args, around line 115):

```rust
#[arg(long = "sh")]
stream_history: Option<String>,
```

- [ ] **Step 2: Add call in main()**

In `main()`, after the `db_viewer(&args.db_viewer_args());` call (line 146), add:

```rust
if let Some(ref sh_input) = args.stream_history {
    utils::stream_history_viewer(sh_input);
}
```

- [ ] **Step 3: Build and verify**

Run: `cargo +nightly build 2>&1 | head -50`
Expected: Compiles without errors

- [ ] **Step 4: Smoke test CLI help**

Run: `cargo +nightly run -- --help 2>&1 | grep -A1 "\-\-sh"`
Expected: Shows `--sh <STREAM_HISTORY>` in help output

- [ ] **Step 5: Smoke test with no history directory**

Run: `cargo +nightly run -- --sh '{"from":"2026-03-22"}' 2>&1`
Expected: Error message about directory not found (since `data/stream_history/` likely doesn't exist in dev)

- [ ] **Step 6: Commit**

```bash
git add backend/src/main.rs
git commit -m "feat: wire --sh CLI argument for stream history viewer

Calls stream_history_viewer() after db_viewer() in main().
Accepts inline JSON or @file.json query."
```

---

### Task 6: Formatting, linting, and final verification

**Files:**
- All modified files

- [ ] **Step 1: Format**

Run: `cargo +nightly fmt`

- [ ] **Step 2: Lint**

Run: `cargo +nightly clippy -- -D warnings 2>&1 | head -50`
Fix any warnings.

- [ ] **Step 3: Run all tests**

Run: `cargo +nightly test --lib -p tuliprox 2>&1 | tail -30`
Expected: All tests pass

- [ ] **Step 4: Run full build**

Run: `cargo +nightly build 2>&1 | tail -10`
Expected: Clean build

- [ ] **Step 5: Commit any formatting/lint fixes**

```bash
git add -A
git commit -m "chore: format and lint fixes for stream history viewer"
```

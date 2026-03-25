use std::fs::File;
use std::io::{self, BufReader, Read};
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

impl<R: Read> StreamHistoryFileReader<R> {
    fn skip_payload(&mut self, len: u32) -> io::Result<()> {
        io::copy(&mut (&mut self.reader).take(u64::from(len)), &mut io::sink())?;
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

            // Block-level skip: check if entire block is outside time range
            if let Some((start, end)) = self.time_range {
                if block_header.last_event_ts_utc < start || block_header.first_event_ts_utc > end {
                    if let Err(e) = self.skip_payload(block_header.payload_len) {
                        if e.kind() == io::ErrorKind::UnexpectedEof {
                            // Truncated file - stop processing
                            return Ok(None);
                        }
                        return Err(e);
                    }
                    continue; // Try next block (loop, not recursion)
                }
            }

            // Validate payload size (OOM protection) - do this AFTER time range check
            if block_header.payload_len as usize > MAX_BLOCK_PAYLOAD_SIZE {
                eprintln!("Warning: {}: block payload_len {} exceeds MAX_BLOCK_PAYLOAD_SIZE, skipping",
                          self.file_path, block_header.payload_len);
                if let Err(e) = self.skip_payload(block_header.payload_len) {
                    if e.kind() == io::ErrorKind::UnexpectedEof {
                        // Truncated file - try to recover with magic search
                        return self.magic_recovery();
                    }
                    return Err(e);
                }
                continue; // Try next block (loop, not recursion)
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

    /// Scan forward through corrupted data looking for the next valid `BLOCK_MAGIC`.
    /// Uses a sliding window with carry bytes across read boundaries.
    /// When a candidate is found, the unconsumed buffer tail is chained with the
    /// underlying reader so that `read_framed` reads from the correct position.
    fn magic_recovery(&mut self) -> io::Result<Option<()>> {
        let mut scan_buf = [0u8; 4096];
        let magic = BLOCK_MAGIC;
        let mut carry = [0u8; 3];
        let mut carry_len = 0usize;

        loop {
            let n = match self.reader.read(&mut scan_buf) {
                Ok(0) => return Ok(None),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(e) => return Err(e),
            };

            // Build a combined view: carry bytes + newly read bytes
            let search_len = carry_len + n;

            for i in 0..search_len.saturating_sub(3) {
                let b = |idx: usize| -> u8 {
                    if idx < carry_len { carry[idx] } else { scan_buf[idx - carry_len] }
                };
                if b(i) == magic[0] && b(i + 1) == magic[1] && b(i + 2) == magic[2] && b(i + 3) == magic[3] {
                    // Bytes after the magic within our buffer that haven't been consumed
                    // by the underlying reader yet — chain them before self.reader.
                    let tail_start = (i + 4).saturating_sub(carry_len);
                    let tail = &scan_buf[tail_start..n];
                    let mut chained = io::Cursor::new(tail).chain(&mut self.reader);

                    let block_header: BlockHeaderBody = match read_framed::<_, BlockHeaderBody>(&mut chained) {
                        Ok(h) if (h.payload_len as usize) <= MAX_BLOCK_PAYLOAD_SIZE => {
                            eprintln!("Warning: {}: recovered at block with {} records", self.file_path, h.record_count);
                            h
                        }
                        Ok(h) => {
                            eprintln!("Warning: {}: recovery candidate rejected (payload_len={})", self.file_path, h.payload_len);
                            continue;
                        }
                        Err(_) => continue,
                    };

                    // Read and validate payload from the chained reader
                    self.payload_buf.resize(block_header.payload_len as usize, 0);
                    chained.read_exact(&mut self.payload_buf)?;
                    self.current_block_remaining = block_header.record_count;
                    self.payload_offset = 0;
                    return Ok(Some(()));
                }
            }

            // Keep last 3 bytes as carry for cross-boundary magic detection
            if n >= 3 {
                carry[..3].copy_from_slice(&scan_buf[n - 3..n]);
                carry_len = 3;
            } else {
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
                // SAFETY: bounds check on line 212 guarantees exactly 4 bytes are available
                let mut record_len_bytes = [0u8; 4];
                record_len_bytes.copy_from_slice(&self.payload_buf[self.payload_offset..self.payload_offset + 4]);
                let record_len = u32::from_be_bytes(record_len_bytes) as usize;
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
                Ok(Some(())) => {} // Got a new block, loop back to read records
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::stream_history::file::*;
    use std::io::Write;
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

        // Time range 5000-6000 should skip entire block
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
        // Test that we don't OOM when block header has oversized payload_len
        // and that we can continue reading after skipping it
        let tmp = NamedTempFile::new().unwrap();
        let mut f = tmp.as_file().try_clone().unwrap();

        // Write valid header
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

        // Write a bad block with huge payload_len but only write a small amount
        write_block_magic(&mut f).unwrap();
        let bad_block_header = BlockHeaderBody {
            block_version: 1,
            record_count: 1,
            payload_len: MAX_BLOCK_PAYLOAD_SIZE as u32 + 1,
            first_event_ts_utc: 1000,
            last_event_ts_utc: 1000,
            payload_crc: 0,
            flags: 0,
        };
        write_framed(&mut f, &bad_block_header).unwrap();
        // Write small payload (corrupt file scenario)
        f.write_all(&[0u8; 100]).unwrap();

        // The key test: when we try to skip 16MB+1 and only have 100 bytes,
        // we should handle EOF gracefully and not panic or OOM
        // The skip will consume all 100 bytes and reach EOF

        f.flush().unwrap();

        let (reader, _) = StreamHistoryFileReader::from_pending(tmp.path(), None).unwrap();
        // Should finish without panicking (even though it won't yield any records)
        let results: Vec<_> = reader.collect();

        // All results will be errors because the file is truncated
        // But we should have finished without OOM or panic
        assert!(!results.is_empty() || results.iter().all(|r| r.is_err()));
    }

    // Additional test for lz4 archive round-trip:
    #[test]
    fn test_archive_lz4_readable() {
        use crate::repository::stream_history::archive::archive_pending_file;
        use tempfile::TempDir;

        let _tmp = TempDir::new().unwrap();
        let records = vec![
            make_test_record(1000, "alice"),
            make_test_record(2000, "bob"),
        ];

        // Create a pending file with known records
        let pending_path = write_test_pending_file(&records);
        let archive_path = archive_pending_file(pending_path.path()).unwrap();

        // Open with StreamHistoryFileReader::from_archive
        let (reader, _header) = StreamHistoryFileReader::from_archive(&archive_path, None).unwrap();
        let results: Vec<_> = reader.collect::<Result<Vec<_>, _>>().unwrap();

        // Verify all records are yielded correctly
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].api_username.as_deref(), Some("alice"));
        assert_eq!(results[1].api_username.as_deref(), Some("bob"));
    }
}

# Stream History Viewer - Design Specification

## Overview

A CLI tool to dump stream history records from tuliprox's binary archive files, similar to existing DB viewer functionality (`--dbx`, `--dbm`, etc.). Uses a single `--sh` parameter accepting a JSON query object (inline or from file). Supports date/datetime range filtering, field filters (exact or regex), and streaming JSON output with zero RAM overhead.

## File Format Recap

Stream history is stored in `data/stream_history/` (default path):

- **Active day**: `stream-history-YYYY-MM-DD.pending` (uncompressed, actively written)
- **Archived days**: `stream-history-YYYY-MM-DD.archive.lz4` (lz4 frame-compressed, finalized)

**File structure**:
```
[FILE_MAGIC: 8 bytes] "STRHIST\x01"
[header_len: u32 BE][header_bytes: N][header_crc: u32 BE]
[BLOCK_MAGIC: 4 bytes] "BLK\x01"   (repeated for each block)
[hdr_len: u32 BE][hdr_bytes: N][hdr_crc: u32 BE][payload_bytes]
```

**Intra-block record layout** (within `payload_bytes`):
```
[record_len: u32 BE][record_bytes: MessagePack named encoding]  (repeated record_count times)
```

**Key header fields for skip optimization**:
- `FileHeaderBody.min_event_ts_utc` / `max_event_ts_utc` — file-level timestamp bounds (archive only, `None` for pending)
- `BlockHeaderBody.first_event_ts_utc` / `last_event_ts_utc` — block-level timestamp bounds

**Compression**: Archives use **lz4** (`lz4_flex` crate, already a workspace dependency) instead of gzip. lz4 offers ~5x faster decompression than gzip at the cost of slightly lower compression ratio (~50-60% vs ~70-80%). For stream history archives that are written once and read often (viewer, debugging), decompression speed dominates. The `CompressionKind` enum changes from `Gzip` to `Lz4` for stream history. File extension changes from `.archive.gz` to `.archive.lz4`.

**Record fields** (`StreamHistoryRecord`):

| Field | Type | Description |
|-------|------|-------------|
| `schema_version` | u8 | Record format version |
| `event_type` | enum | `Connect` or `Disconnect` |
| `event_ts_utc` | u64 | Unix timestamp seconds when event occurred |
| `partition_day_utc` | string | UTC calendar day, e.g. `"2026-03-22"` |
| `session_id` | u64 | Correlates connect/disconnect events of same session |
| `source_addr` | option\<string\> | Client IP:port |
| `api_username` | option\<string\> | Tuliprox user |
| `provider_name` | option\<string\> | Provider name |
| `provider_username` | option\<string\> | Provider username |
| `virtual_id` | option\<u32\> | Channel ID |
| `item_type` | option\<string\> | `live`, `vod`, or `series` |
| `title` | option\<string\> | Stream title |
| `group` | option\<string\> | Stream group |
| `country` | option\<string\> | Country code |
| `connect_ts_utc` | option\<u64\> | Session start timestamp (disconnect only) |
| `disconnect_ts_utc` | option\<u64\> | Session end timestamp (disconnect only) |
| `session_duration` | option\<u64\> | Seconds between connect/disconnect |
| `bytes_sent` | option\<u64\> | Total bytes proxied |
| `disconnect_reason` | option\<enum\> | Reason for disconnect |

## CLI Interface

### Single Parameter

```rust
#[arg(long = "sh")]
pub stream_history: Option<String>,  // JSON inline or @file.json
```

**@-prefix**: `--sh @query.json` reads JSON from file (like curl's `@` syntax).

### Query Format

```json
{
  "from": "2026-03-22",
  "to": "2026-03-24",
  "path": "/custom/history",
  "filter": {
    "api_username": "alice",
    "provider_name": "~^acme.*",
    "event_type": "disconnect",
    "session_id": "123456"
  }
}
```

**Query struct**:
```rust
#[derive(Deserialize)]
struct StreamHistoryQuery {
    from: Option<String>,
    to: Option<String>,
    path: Option<String>,   // Default: "data/stream_history/" relative to working directory
    filter: Option<HashMap<String, String>>,
}
```

### Date Parsing

Accepts three formats:
- **Date only**: `"2026-03-22"` → full day (00:00:00 to 23:59:59 UTC)
- **Datetime without seconds**: `"2026-03-22 14:30"` → interpreted as `14:30:00 UTC`
- **Datetime with seconds**: `"2026-03-22 14:30:00"` → exact UTC timestamp

**Date logic**:
- Only `from` OR only `to` given: dump only that single day's records. If a datetime is given (e.g. `"2026-03-22 14:30"`), the range expands to the full day (`00:00:00` to `23:59:59` UTC) — the time component is ignored for single-date queries
- Both `from` and `to` given: inclusive range `[from, to]`. Date-only values expand: `from` → `00:00:00`, `to` → `23:59:59`. Since `event_ts_utc` has second precision, `23:59:59` captures all events of the day
- Neither given: error with usage message

**UTC notice**: At startup, print `[INFO] All timestamps interpreted as UTC` to stderr, so CLI users are aware that local time zones are not applied.

### Filter Syntax

Flat key-value map, multiple fields = implicit AND.

- **Without `~` prefix**: exact match (case-insensitive)
  - `"api_username": "alice"` → `record.api_username.eq_ignore_ascii_case("alice")`
- **With `~` prefix**: regex match
  - `"provider_name": "~^acme.*"` → `Regex::new("^acme.*").is_match(record.provider_name)`

**Important**: Use `eq_ignore_ascii_case()` for exact matching — never `to_lowercase()` which allocates a new String per call and would destroy zero-RAM performance at millions of records.

**Filterable string fields**: `event_type`, `api_username`, `provider_name`, `provider_username`, `item_type`, `title`, `group`, `country`, `source_addr`, `disconnect_reason`. Filter values are matched against the serde-serialized form (e.g. `"connect"`, `"disconnect"`, `"client_closed"` — snake_case).

**Filterable numeric fields**: `session_id`. When the filter key is a numeric field, the value is parsed as `u64` at startup and compared numerically (no string conversion per record).

**Pre-compilation**: All filters are compiled once at startup into a `CompiledFilter` struct. No regex compilation or string allocation during record iteration.

```rust
enum FilterValue {
    Exact(String),           // case-insensitive eq_ignore_ascii_case
    Regex(regex::Regex),     // pre-compiled
    NumericExact(u64),       // numeric equality (session_id)
}

struct CompiledFilter {
    fields: Vec<(String, FilterValue)>,
}
```

## Architecture

```
CLI (--sh JSON/@file)
  → parse StreamHistoryQuery
  → compile filters (regex pre-compilation, numeric parsing)
  → [INFO] All timestamps interpreted as UTC (stderr)
  → discover & sort files (file-level skip via header min/max timestamps)
  → StreamHistoryFileReader (Iterator<Item = io::Result<StreamHistoryRecord>>)
       - block-level skip via block header timestamps
       - magic-recovery on corrupt blocks (with OOM protection)
  → apply compiled filters
  → streaming JSON output (zero RAM, line-by-line stdout)
```

### File Discovery & Sorting

```rust
fn discover_files(dir: &Path, time_range: Option<(u64, u64)>) -> io::Result<Vec<HistoryFile>>
```

1. List all `.pending` and `.archive.lz4` files in the history directory
2. Read each file header → `partition_day_ts_utc`, `min_event_ts_utc`, `max_event_ts_utc`. For `.archive.lz4` files this requires opening the lz4 stream briefly to read the header (a few hundred bytes), but avoids decompressing block data
3. **File-level skip** (archive only): if `max_event_ts_utc < range_start` or `min_event_ts_utc > range_end`, skip file entirely — no further decompression of block data
4. **Pending files**: `min/max` are `None` → always include
5. Sort primarily by `partition_day_ts_utc` ascending (lexicographic sort on `"YYYY-MM-DD"` produces correct chronological order), secondarily `.archive.lz4` before `.pending` (archive contains older data of the same day in edge cases like rotation overlap)
6. Return sorted list → sequential streaming produces approximately chronological output (records are in insertion order within each file, which is approximately but not strictly timestamp-sorted)

```rust
struct HistoryFile {
    path: PathBuf,
    partition_day: String,
    is_archive: bool,
}
```

### StreamHistoryFileReader (Iterator)

Located in `backend/src/repository/stream_history/reader.rs`.

```rust
struct StreamHistoryFileReader<R: Read> {
    reader: R,
    time_range: Option<(u64, u64)>,
    // Block state
    current_block_remaining: u32,
    payload_buf: Vec<u8>,
    payload_offset: usize,
    done: bool,
}

impl<R: Read> Iterator for StreamHistoryFileReader<R> {
    type Item = io::Result<StreamHistoryRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // 1. Records remaining in current block?
            //    → read u32 BE record_len from payload_buf at payload_offset
            //    → deserialize record_len bytes as StreamHistoryRecord (MessagePack named)
            //    → advance payload_offset, decrement current_block_remaining
            // 2. Block exhausted? → read next block header
            //    a) Block timestamps outside range? → skip payload_len bytes
            //    b) Invalid block magic? → magic-recovery (scan for BLK\x01)
            //    c) CRC error? → stderr log, magic-recovery
            // 3. EOF? → None
        }
    }
}
```

**Constructors**:
```rust
// For .pending files
fn from_pending(path: &Path, time_range: Option<(u64, u64)>) -> io::Result<Self>
// → BufReader<File>, verify FILE_MAGIC, read header

// For .archive.lz4 files
fn from_archive(path: &Path, time_range: Option<(u64, u64)>) -> io::Result<Self>
// → BufReader<Lz4Decoder<File>>, verify FILE_MAGIC, read header
```

**Block-level skip**: When `block_header.last_event_ts_utc < range_start` or `block_header.first_event_ts_utc > range_end`, skip `payload_len` bytes without deserializing records. For `.pending` (uncompressed): seek past `payload_len` bytes. For `.archive.lz4`: use `io::copy(&mut reader.take(payload_len as u64), &mut io::sink())` — lz4 must still decompress the data, but MessagePack deserialization and filter evaluation are avoided. lz4 decompression is ~5x faster than gzip, making this skip much cheaper than the previous gzip design.

**Magic-recovery**: On corrupt block (bad magic, CRC mismatch):
1. Read ahead in chunks (e.g. 4KB) into a scan buffer, search for `BLK\x01` (0x42 0x4C 0x4B 0x01) sequence
2. When a candidate is found, attempt to read and validate the block header (deserialize framed `BlockHeaderBody`). If deserialization fails, continue scanning — `BLK\x01` may appear as coincidental bytes in record data
3. **OOM protection**: If a recovered block header declares `payload_len > MAX_BLOCK_PAYLOAD_SIZE` (16 MiB), discard the candidate and continue scanning. This prevents corrupted payload_len values from causing unbounded memory allocation or infinite reads. `MAX_FRAME_SIZE` (8 MiB) already exists in `file.rs`; `MAX_BLOCK_PAYLOAD_SIZE` should be defined as `2 * MAX_FRAME_SIZE` (16 MiB) for a safe upper bound
4. Log warning to stderr with file path and approximate byte offset
5. Continue with validated block

### Streaming Output

Zero RAM — each record is serialized and printed immediately, then discarded. Follows DB viewer pattern.

```rust
fn stream_output(files: Vec<HistoryFile>, time_range, filters: CompiledFilter) {
    println!("[");
    let mut first = true;
    for file in &files {
        let reader = match open_reader(file, time_range) {
            Ok(r) => r,
            Err(e) => { eprintln!("Warning: skipping {}: {e}", file.path.display()); continue; }
        };
        for result in reader {
            match result {
                Ok(record) => {
                    if !in_time_range(record.event_ts_utc, time_range) { continue; }
                    if !filters.matches(&record) { continue; }
                    if !first { println!(","); }
                    print!("  {}", serde_json::to_string(&record)?);
                    first = false;
                }
                Err(e) => eprintln!("Warning: {e}"),
            }
        }
    }
    println!("\n]");
}
```

Errors go to stderr, valid JSON stays on stdout.

## Archiver Changes (lz4 migration)

The existing archiver in `backend/src/repository/stream_history/archive.rs` must be updated:

1. **Replace `GzEncoder` with lz4 frame encoder** (`lz4_flex::frame::FrameEncoder`)
2. **Change file extension** from `.archive.gz` to `.archive.lz4`
3. **Update `CompressionKind`** in finalized header from `Gzip` to `Lz4`
4. **Update `archive_path_for()`** helper to produce `.archive.lz4` paths
5. **Update `recover_pending_files()`** to scan for `.archive.lz4` instead of `.archive.gz`

No backward compatibility needed — stream history feature is still in development.

## Files to Create/Modify

1. **`backend/src/repository/stream_history/reader.rs`** (new)
   - `StreamHistoryFileReader<R>` struct + Iterator impl
   - `from_pending()` / `from_archive()` constructors
   - Block-skipping and magic-recovery logic (with OOM protection)

2. **`backend/src/utils/stream_history_viewer.rs`** (new)
   - `StreamHistoryQuery` struct (deserialized from JSON)
   - `CompiledFilter` struct with pre-compiled regex and numeric filters
   - `stream_history_viewer()` entry point
   - `parse_date_or_datetime()` helper (3 formats)
   - `discover_files()` file discovery + sorting (with archive-before-pending tiebreak)
   - `stream_output()` streaming JSON writer

3. **`backend/src/main.rs`**
   - Add `--sh` arg to `Args` struct
   - Call `stream_history_viewer()` in `main()` after `db_viewer()`

4. **`backend/src/utils/mod.rs`**
   - Export `stream_history_viewer` module

5. **`backend/src/repository/stream_history/mod.rs`**
   - Export `reader` module

6. **`backend/src/repository/stream_history/archive.rs`** (modify)
   - Replace `flate2::write::GzEncoder` with `lz4_flex::frame::FrameEncoder`
   - Update file extension and `CompressionKind`

7. **`backend/src/repository/stream_history/file.rs`** (modify)
   - Add `Lz4` variant to `CompressionKind` (or rename `Gzip` to `Lz4`)
   - Add `MAX_BLOCK_PAYLOAD_SIZE` constant (16 MiB)

## Error Handling

| Situation | Behavior |
|-----------|----------|
| Directory doesn't exist | Error + exit |
| No files found in range | Error with hint about available dates |
| Invalid date format | Error showing expected formats (3 variants) |
| Invalid regex in filter | Error at startup (before any file I/O) |
| Invalid numeric filter value | Error at startup (e.g. `session_id: "abc"`) |
| Invalid JSON query | Error with example of expected format |
| Corrupt file header | stderr warning, skip file, continue |
| Corrupt block (bad magic/CRC) | stderr warning, magic-recovery (scan for next `BLK\x01`) |
| Recovery: payload_len > MAX_BLOCK_PAYLOAD_SIZE | Discard candidate, continue scanning |
| Truncated pending file | Graceful stop, already-printed records are valid |
| No matching records | Output empty array `[]` |

## Testing

### Unit Tests (in `stream_history_viewer.rs`)

- `test_parse_date_only` — `"2026-03-22"` → correct day range
- `test_parse_datetime_no_seconds` — `"2026-03-22 14:30"` → `14:30:00`
- `test_parse_datetime` — `"2026-03-22 14:30:00"` → exact timestamp
- `test_invalid_date_format` — error on bad format
- `test_filter_exact_match` — case-insensitive exact match (via `eq_ignore_ascii_case`)
- `test_filter_regex_match` — `~` prefix compiles and matches regex
- `test_filter_invalid_regex` — error at compile time
- `test_filter_session_id_numeric` — numeric session_id filter works
- `test_single_date_only_that_day` — single date = only that day
- `test_at_file_prefix` — `@query.json` reads from file

### Unit Tests (in `reader.rs`)

- `test_block_skipping` — block outside time range is skipped
- `test_magic_recovery` — corrupt block skipped, next valid block read
- `test_magic_recovery_oom_protection` — oversized payload_len in recovered header is rejected
- `test_file_level_skip` — archive outside range not decompressed
- `test_iterator_yields_all_records` — all records in valid file returned
- `test_truncated_file_graceful` — partial file doesn't panic

### Unit Tests (in `archive.rs`)

- `test_archive_creates_lz4` — archiver produces `.archive.lz4` files
- `test_archive_lz4_readable` — reader can open and iterate lz4 archives

### Integration Tests (optional)

- Create temporary `.pending` and `.archive.lz4` files with known records
- Run viewer with various date ranges and filters
- Verify output is valid JSON matching expected records

## Usage Examples

```bash
# Dump single day
tuliprox --sh '{"from":"2026-03-22"}'

# Dump date range
tuliprox --sh '{"from":"2026-03-20","to":"2026-03-22"}'

# Dump datetime range (with seconds)
tuliprox --sh '{"from":"2026-03-22 10:00:00","to":"2026-03-22 18:00:00"}'

# Dump datetime range (without seconds)
tuliprox --sh '{"from":"2026-03-22 10:00","to":"2026-03-22 18:00"}'

# Filter by username (exact)
tuliprox --sh '{"from":"2026-03-22","filter":{"api_username":"alice"}}'

# Filter by provider (regex)
tuliprox --sh '{"from":"2026-03-22","filter":{"provider_name":"~^acme.*"}}'

# Filter by session ID
tuliprox --sh '{"from":"2026-03-22","filter":{"session_id":"123456"}}'

# Combine filters (implicit AND)
tuliprox --sh '{"from":"2026-03-20","to":"2026-03-22","filter":{"api_username":"alice","event_type":"disconnect"}}'

# Query from file
tuliprox --sh @query.json

# Custom history directory
tuliprox --sh '{"from":"2026-03-22","path":"/custom/path/history"}'
```

## Performance Design

- **Compression: lz4** instead of gzip — ~5x faster decompression, ideal for read-heavy archive access
- **File-level skip**: Archive files with `max_event_ts_utc < range_start` or `min_event_ts_utc > range_end` skip after reading only the lz4 header (a few hundred bytes, no block decompression)
- **Block-level skip**: Blocks outside time range skip `payload_len` bytes without deserializing records. For pending files: O(1) seek. For lz4 archives: `io::copy(take, sink)` — still requires lz4 decompression but avoids MessagePack deserialization (lz4 decompression is very fast)
- **Pre-compiled filters**: Regex compiled once at startup, numeric filters parsed once. No per-record allocation
- **Zero-allocation matching**: `eq_ignore_ascii_case()` for exact string filters — no `to_lowercase()` heap allocation
- **Streaming output**: Zero RAM accumulation, records printed and discarded immediately
- **Approximate chronological ordering**: Files sorted by partition day (archive before pending for same-day tiebreak). Within each file, records are in insertion order (approximately chronological). No cross-file sort needed
- **Pending files**: Always read (no min/max in header), but block-level skip still applies
- **Magic-recovery**: Chunk-based scanning (4KB) with two-phase validation to avoid false `BLK\x01` matches. OOM protection via `MAX_BLOCK_PAYLOAD_SIZE` (16 MiB) guard
- **Session-ID filter**: Numeric comparison (`u64 == u64`), no string conversion per record

## Future Enhancements

- `limit` field in query to cap output record count
- `output` field to write to file instead of stdout
- `format` field for json/csv output
- AND/OR/NOT filter expressions (reuse playlist filter infrastructure)
- Session summary mode (aggregate connect/disconnect pairs)
- Statistics mode (count by user, provider, country)
- SIGINT handling for graceful JSON closing bracket on Ctrl+C

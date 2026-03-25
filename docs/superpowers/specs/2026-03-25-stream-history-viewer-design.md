# Stream History Viewer - Design Specification

## Overview

A CLI tool to dump stream history records from tuliprox's binary archive files, similar to existing DB viewer functionality (`--dbx`, `--dbm`, etc.). Uses a single `--sh` parameter accepting a JSON query object (inline or from file). Supports date/datetime range filtering, field filters (exact or regex), and streaming JSON output with zero RAM overhead.

## File Format Recap

Stream history is stored in `data/stream_history/` (default path):

- **Active day**: `stream-history-YYYY-MM-DD.pending` (uncompressed, actively written)
- **Archived days**: `stream-history-YYYY-MM-DD.archive.gz` (gzip-compressed, finalized)

**File structure**:
```
[FILE_MAGIC: 8 bytes] "STRHIST\x01"
[header_len: u32 BE][header_bytes: N][header_crc: u32 BE]
[BLOCK_MAGIC: 4 bytes] "BLK\x01"   (repeated for each block)
[hdr_len: u32 BE][hdr_bytes: N][hdr_crc: u32 BE][payload_bytes]
```

**Key header fields for skip optimization**:
- `FileHeaderBody.min_event_ts_utc` / `max_event_ts_utc` — file-level timestamp bounds (archive only, `None` for pending)
- `BlockHeaderBody.first_event_ts_utc` / `last_event_ts_utc` — block-level timestamp bounds

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
    "event_type": "disconnect"
  }
}
```

**Query struct**:
```rust
#[derive(Deserialize)]
struct StreamHistoryQuery {
    from: Option<String>,
    to: Option<String>,
    path: Option<String>,
    filter: Option<HashMap<String, String>>,
}
```

### Date Parsing

Accepts three formats:
- **Date only**: `"2026-03-22"` → full day (00:00:00 to 23:59:59 UTC)
- **Datetime without seconds**: `"2026-03-22 14:30"` → interpreted as `14:30:00 UTC`
- **Datetime with seconds**: `"2026-03-22 14:30:00"` → exact UTC timestamp

**Date logic**:
- Only `from` OR only `to` given: dump only that single day's records
- Both `from` and `to` given: inclusive range `[from, to]`
- Neither given: error with usage message

### Filter Syntax

Flat key-value map, multiple fields = implicit AND.

- **Without `~` prefix**: exact match (case-insensitive)
  - `"api_username": "alice"` → `record.api_username == "alice"` (case-insensitive)
- **With `~` prefix**: regex match
  - `"provider_name": "~^acme.*"` → `Regex::new("^acme.*").is_match(record.provider_name)`

**Filterable fields**: all string fields of `StreamHistoryRecord` — `event_type`, `api_username`, `provider_name`, `provider_username`, `item_type`, `title`, `group`, `country`, `source_addr`, `disconnect_reason`.

**Pre-compilation**: All regex filters are compiled once at startup into a `CompiledFilter` struct. No regex compilation during record iteration.

```rust
enum FilterValue {
    Exact(String),       // case-insensitive eq
    Regex(regex::Regex), // pre-compiled
}

struct CompiledFilter {
    fields: Vec<(String, FilterValue)>,
}
```

## Architecture

```
CLI (--sh JSON/@file)
  → parse StreamHistoryQuery
  → compile filters (regex pre-compilation)
  → discover & sort files (file-level skip via header min/max timestamps)
  → StreamHistoryFileReader (Iterator<Item = io::Result<StreamHistoryRecord>>)
       - block-level skip via block header timestamps
       - magic-recovery on corrupt blocks
  → apply compiled filters
  → streaming JSON output (zero RAM, line-by-line stdout)
```

### File Discovery & Sorting

```rust
fn discover_files(dir: &Path, time_range: Option<(u64, u64)>) -> io::Result<Vec<HistoryFile>>
```

1. List all `.pending` and `.archive.gz` files in the history directory
2. Read each file header → `partition_day_ts_utc`, `min_event_ts_utc`, `max_event_ts_utc`
3. **File-level skip** (archive only): if `max_event_ts_utc < range_start` or `min_event_ts_utc > range_end`, skip file entirely (avoids full gzip decompression)
4. **Pending files**: `min/max` are `None` → always include
5. Sort by `partition_day_ts_utc` ascending
6. Return sorted list → sequential streaming produces chronologically ordered output

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
            // 1. Records remaining in current block? → deserialize next
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

// For .archive.gz files
fn from_archive(path: &Path, time_range: Option<(u64, u64)>) -> io::Result<Self>
// → BufReader<GzDecoder<File>>, verify FILE_MAGIC, read header
```

**Block-level skip**: When `block_header.last_event_ts_utc < range_start` or `block_header.first_event_ts_utc > range_end`, skip `payload_len` bytes without deserializing records. For `.pending` (uncompressed): seek. For `.archive.gz`: read and discard (gzip is not seekable).

**Magic-recovery**: On corrupt block (bad magic, CRC mismatch), scan byte-by-byte for next `BLK\x01` sequence. Log warning to stderr. Continue with next valid block.

### Streaming Output

Zero RAM — each record is serialized and printed immediately, then discarded. Follows DB viewer pattern.

```rust
fn stream_output(files: Vec<HistoryFile>, time_range, filters: CompiledFilter) {
    println!("[");
    let mut first = true;
    for file in &files {
        let reader = open_reader(file, time_range)?;
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

## Files to Create/Modify

1. **`backend/src/repository/stream_history/reader.rs`** (new)
   - `StreamHistoryFileReader<R>` struct + Iterator impl
   - `from_pending()` / `from_archive()` constructors
   - Block-skipping and magic-recovery logic

2. **`backend/src/utils/stream_history_viewer.rs`** (new)
   - `StreamHistoryQuery` struct (deserialized from JSON)
   - `CompiledFilter` struct with pre-compiled regex
   - `stream_history_viewer()` entry point
   - `parse_date_or_datetime()` helper (3 formats)
   - `discover_files()` file discovery + sorting
   - `stream_output()` streaming JSON writer

3. **`backend/src/main.rs`**
   - Add `--sh` arg to `Args` struct
   - Call `stream_history_viewer()` in `main()` after `db_viewer()`

4. **`backend/src/utils/mod.rs`**
   - Export `stream_history_viewer` module

5. **`backend/src/repository/stream_history/mod.rs`**
   - Export `reader` module

## Error Handling

| Situation | Behavior |
|-----------|----------|
| Directory doesn't exist | Error + exit |
| No files found in range | Error with hint about available dates |
| Invalid date format | Error showing expected formats (3 variants) |
| Invalid regex in filter | Error at startup (before any file I/O) |
| Invalid JSON query | Error with example of expected format |
| Corrupt file header | stderr warning, skip file, continue |
| Corrupt block (bad magic/CRC) | stderr warning, magic-recovery (scan for next `BLK\x01`) |
| Truncated pending file | Graceful stop, already-printed records are valid |
| No matching records | Output empty array `[]` |

## Testing

### Unit Tests (in `stream_history_viewer.rs`)

- `test_parse_date_only` — `"2026-03-22"` → correct day range
- `test_parse_datetime_no_seconds` — `"2026-03-22 14:30"` → `14:30:00`
- `test_parse_datetime` — `"2026-03-22 14:30:00"` → exact timestamp
- `test_invalid_date_format` — error on bad format
- `test_filter_exact_match` — case-insensitive exact match
- `test_filter_regex_match` — `~` prefix compiles and matches regex
- `test_filter_invalid_regex` — error at compile time
- `test_single_date_only_that_day` — single date = only that day
- `test_at_file_prefix` — `@query.json` reads from file

### Unit Tests (in `reader.rs`)

- `test_block_skipping` — block outside time range is skipped
- `test_magic_recovery` — corrupt block skipped, next valid block read
- `test_file_level_skip` — archive outside range not decompressed
- `test_iterator_yields_all_records` — all records in valid file returned
- `test_truncated_file_graceful` — partial file doesn't panic

### Integration Tests (optional)

- Create temporary `.pending` and `.archive.gz` files with known records
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

# Combine filters (implicit AND)
tuliprox --sh '{"from":"2026-03-20","to":"2026-03-22","filter":{"api_username":"alice","event_type":"disconnect"}}'

# Query from file
tuliprox --sh @query.json

# Custom history directory
tuliprox --sh '{"from":"2026-03-22","path":"/custom/path/history"}'
```

## Performance Design

- **File-level skip**: Archive files with `max_event_ts_utc < range_start` or `min_event_ts_utc > range_end` are never decompressed
- **Block-level skip**: Blocks outside time range skip `payload_len` bytes without deserializing records
- **Pre-compiled filters**: Regex compiled once at startup, not per-record
- **Streaming output**: Zero RAM accumulation, records printed and discarded immediately
- **Chronological ordering**: Files sorted by partition day before streaming — no cross-file sort needed
- **Pending files**: Always read (no min/max in header), but block-level skip still applies

## Future Enhancements

- `limit` field in query to cap output record count
- `session_id` filter for specific session lookup
- `output` field to write to file instead of stdout
- `format` field for json/csv output
- AND/OR/NOT filter expressions (reuse playlist filter infrastructure)
- Session summary mode (aggregate connect/disconnect pairs)
- Statistics mode (count by user, provider, country)

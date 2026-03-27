use std::path::PathBuf;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use chrono;
use regex::Regex;
use serde::Deserialize;

use crate::repository::{StreamHistoryRecord, EventType, DisconnectReason, StreamHistoryFileReader, FileHeaderBody, read_and_verify_file_magic, read_framed, extract_day_from_filename};

#[derive(Deserialize)]
struct StreamHistoryQuery {
    pub from: Option<String>,
    pub to: Option<String>,
    pub path: Option<String>,
    pub filter: Option<HashMap<String, String>>,
}

/// Parsed time range as (`start_ts_utc`, `end_ts_utc`) in seconds
type TimeRange = (u64, u64);

const SECS_PER_DAY: u64 = 86400;

/// Parse a date or datetime string into a UTC unix timestamp.
/// Accepts: "YYYY-MM-DD", "YYYY-MM-DD HH:MM", "YYYY-MM-DD HH:MM:SS"
fn parse_date_or_datetime(input: &str) -> Result<u64, String> {
    let trimmed = input.trim();

    // Try date only: YYYY-MM-DD
    if let Ok(date) = chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        let dt = date.and_hms_opt(0, 0, 0)
            .ok_or_else(|| format!("Invalid date: '{trimmed}'"))?;
        return Ok(dt.and_utc().timestamp().cast_unsigned());
    }

    // Try datetime without seconds: YYYY-MM-DD HH:MM
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M") {
        return Ok(dt.and_utc().timestamp().cast_unsigned());
    }

    // Try datetime with seconds: YYYY-MM-DD HH:MM:SS
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S") {
        return Ok(dt.and_utc().timestamp().cast_unsigned());
    }

    Err(format!(
        "Invalid date format: '{trimmed}'. Expected: YYYY-MM-DD, YYYY-MM-DD HH:MM, or YYYY-MM-DD HH:MM:SS"
    ))
}

/// Returns true if input is a date-only format (no time component)
fn is_date_only(input: &str) -> bool {
    chrono::NaiveDate::parse_from_str(input.trim(), "%Y-%m-%d").is_ok()
}

/// Resolve the query's from/to into a concrete time range.
fn resolve_time_range(query: &StreamHistoryQuery) -> Result<TimeRange, String> {
    match (&query.from, &query.to) {
        (Some(from), Some(to)) => {
            let start = parse_date_or_datetime(from)?;
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
                let naive = chrono::DateTime::from_timestamp(parsed.cast_signed(), 0)
                    .ok_or_else(|| format!("Invalid timestamp: {parsed}"))?
                    .naive_utc()
                    .date()
                    .and_hms_opt(0, 0, 0)
                    .ok_or_else(|| format!("Invalid date for timestamp: {parsed}"))?;
                naive.and_utc().timestamp().cast_unsigned()
            };
            Ok((day_start, day_start + SECS_PER_DAY - 1))
        }
        (None, None) => Err("At least 'from' or 'to' must be specified".to_string()),
    }
}

const NUMERIC_FIELDS: &[&str] = &["session_id"];

enum FilterValue {
    Exact(String),
    Regex(Regex),
    NumericExact(u64),
}

struct CompiledFilter {
    fields: Vec<(String, FilterValue)>,
}

impl CompiledFilter {
    fn compile(raw: &HashMap<String, String>) -> Result<Self, String> {
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

    fn matches(&self, record: &StreamHistoryRecord) -> bool {
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

/// Load query from inline JSON or @file reference
fn load_query(input: &str) -> Result<StreamHistoryQuery, String> {
    let json_str = if let Some(file_path) = input.strip_prefix('@') {
        fs::read_to_string(file_path)
            .map_err(|e| format!("Failed to read query file '{file_path}': {e}"))?
    } else {
        input.to_string()
    };

    serde_json::from_str(&json_str)
        .map_err(|e| format!("Invalid JSON query: {e}\nExpected format: {{\"from\":\"YYYY-MM-DD\",\"to\":\"YYYY-MM-DD\",\"filter\":{{...}}}}"))
}


struct HistoryFile {
    path: PathBuf,
    partition_day: String,
    is_archive: bool,
}

fn discover_files(dir: &Path, time_range: &TimeRange) -> io::Result<Vec<HistoryFile>> {
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

        // Fast filename-based pre-filter: skip files whose partition day
        // is clearly outside the query range without reading headers.
        if let Some(day) = extract_day_from_filename(&name) {
            if let Ok(day_start) = parse_date_or_datetime(day) {
                let day_end = day_start + SECS_PER_DAY - 1;
                if day_end < range_start || day_start > range_end {
                    continue;
                }
            }
        }

        // Read file header for archive-level timestamp bounds
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
                    continue;
                }
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

fn stream_output(
    files: &[HistoryFile],
    time_range: &TimeRange,
    filters: &CompiledFilter,
) {
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

    if !first {
        println!();
    }
    println!("]");
}

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
        }
    };

    let time_range = match resolve_time_range(&query) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {e}");
            exit_viewer(1);
        }
    };

    let filters = match query.filter.as_ref() {
        Some(raw) => match CompiledFilter::compile(raw) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("Error: {e}");
                exit_viewer(1);
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
        }
    };

    stream_output(files.as_slice(), &time_range, &filters);

    exit_viewer(0);
}

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
        let (_start, end) = resolve_time_range(&query).unwrap();
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

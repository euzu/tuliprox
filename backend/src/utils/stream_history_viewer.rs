use std::path::PathBuf;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use chrono;
use regex::Regex;
use serde::Deserialize;

use crate::repository::{
    ConnectFailureReason, DisconnectReason, EventType, FailureStage, FileHeaderBody, StreamHistoryFileReader,
    StreamHistoryRecord, extract_day_from_filename, read_and_verify_file_magic, read_framed,
};

#[derive(Deserialize)]
pub(crate) struct StreamHistoryQuery {
    pub from: Option<String>,
    pub to: Option<String>,
    pub path: Option<String>,
    pub filter: Option<HashMap<String, String>>,
}

/// Parsed time range as (`start_ts_utc`, `end_ts_utc`) in seconds
pub(crate) type TimeRange = (u64, u64);

const SECS_PER_DAY: u64 = 86400;

/// Parse a date or datetime string into a UTC unix timestamp.
/// Accepts: "YYYY-MM-DD", "YYYY-MM-DD HH:MM", "YYYY-MM-DD HH:MM:SS"
pub(crate) fn parse_date_or_datetime(input: &str) -> Result<u64, String> {
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
pub(crate) fn resolve_time_range(query: &StreamHistoryQuery) -> Result<TimeRange, String> {
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

const NUMERIC_FIELDS: &[&str] = &[
    "session_id",
    "virtual_id",
    "provider_id",
    "target_id",
    "provider_http_status",
    "shared_stream_id",
];
const STRING_FIELDS: &[&str] = &[
    "event_type",
    "api_username",
    "provider_name",
    "provider_username",
    "input_name",
    "item_type",
    "title",
    "group",
    "country",
    "source_addr",
    "disconnect_reason",
    "connect_failure_reason",
    "failure_stage",
    "shared_joined_existing",
    "provider_error_class",
    "user_agent",
    "cluster",
    "container",
    "stream_url_hash",
    "stream_identity_key",
    "video_codec",
    "audio_codec",
    "audio_channels",
    "resolution",
    "fps",
    "shared",
];

enum FilterValue {
    Exact(String),
    Regex(Regex),
    NumericExact(u64),
}

pub(crate) struct CompiledFilter {
    fields: Vec<(String, FilterValue)>,
}

impl CompiledFilter {
    pub(crate) fn compile(raw: &HashMap<String, String>) -> Result<Self, String> {
        let mut fields = Vec::with_capacity(raw.len());
        for (key, value) in raw {
            if !STRING_FIELDS.contains(&key.as_str()) && !NUMERIC_FIELDS.contains(&key.as_str()) {
                return Err(format!("Unknown filter field: '{key}'"));
            }
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

    pub(crate) fn matches(&self, record: &StreamHistoryRecord) -> bool {
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
                EventType::ConnectFailed => "connect_failed",
                EventType::Disconnect => "disconnect",
            };
            RecordFieldValue::String(Some(s))
        }
        "api_username" => RecordFieldValue::String(record.api_username.as_deref()),
        "provider_name" => RecordFieldValue::String(record.provider_name.as_deref()),
        "provider_username" => RecordFieldValue::String(record.provider_username.as_deref()),
        "input_name" => RecordFieldValue::String(record.input_name.as_deref()),
        "item_type" => RecordFieldValue::String(record.item_type.as_deref()),
        "title" => RecordFieldValue::String(record.title.as_deref()),
        "group" => RecordFieldValue::String(record.group.as_deref()),
        "country" => RecordFieldValue::String(record.country.as_deref()),
        "source_addr" => RecordFieldValue::String(record.source_addr.as_deref()),
        "shared" => RecordFieldValue::String(record.shared.map(|shared| if shared { "true" } else { "false" })),
        "shared_joined_existing" => RecordFieldValue::String(
            record
                .shared_joined_existing
                .map(|shared| if shared { "true" } else { "false" }),
        ),
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
                DisconnectReason::UserConnectionsExhausted => "user_connections_exhausted",
                DisconnectReason::ProviderConnectionsExhausted => "provider_connections_exhausted",
            }))
        }
        "connect_failure_reason" => {
            RecordFieldValue::String(record.connect_failure_reason.as_ref().map(|r| match r {
                ConnectFailureReason::UserAccountExpired => "user_account_expired",
                ConnectFailureReason::UserConnectionsExhausted => "user_connections_exhausted",
                ConnectFailureReason::ProviderConnectionsExhausted => "provider_connections_exhausted",
                ConnectFailureReason::ProviderError => "provider_error",
                ConnectFailureReason::ProviderClosed => "provider_closed",
                ConnectFailureReason::ChannelUnavailable => "channel_unavailable",
                ConnectFailureReason::Preempted => "preempted",
                ConnectFailureReason::SessionExpired => "session_expired",
                ConnectFailureReason::Provisioning => "provisioning",
            }))
        }
        "failure_stage" => {
            RecordFieldValue::String(record.failure_stage.as_ref().map(|stage| match stage {
                FailureStage::Admission => "admission",
                FailureStage::ProviderOpen => "provider_open",
                FailureStage::FirstByte => "first_byte",
                FailureStage::Streaming => "streaming",
                FailureStage::SessionReconnect => "session_reconnect",
            }))
        }
        "provider_error_class" => RecordFieldValue::String(record.provider_error_class.as_deref()),
        "user_agent" => RecordFieldValue::String(record.user_agent.as_deref()),
        "cluster" => RecordFieldValue::String(record.cluster.as_deref()),
        "container" => RecordFieldValue::String(record.container.as_deref()),
        "stream_url_hash" => RecordFieldValue::String(record.stream_url_hash.as_deref()),
        "stream_identity_key" => RecordFieldValue::String(record.stream_identity_key.as_deref()),
        "video_codec" => RecordFieldValue::String(record.video_codec.as_deref()),
        "audio_codec" => RecordFieldValue::String(record.audio_codec.as_deref()),
        "audio_channels" => RecordFieldValue::String(record.audio_channels.as_deref()),
        "resolution" => RecordFieldValue::String(record.resolution.as_deref()),
        "fps" => RecordFieldValue::String(record.fps.as_deref()),
        "session_id" => RecordFieldValue::U64(record.session_id),
        "virtual_id" => RecordFieldValue::U64(u64::from(record.virtual_id.unwrap_or(0))),
        "provider_id" => RecordFieldValue::U64(u64::from(record.provider_id.unwrap_or(0))),
        "target_id" => RecordFieldValue::U64(u64::from(record.target_id.unwrap_or(0))),
        "provider_http_status" => RecordFieldValue::U64(u64::from(record.provider_http_status.unwrap_or(0))),
        "shared_stream_id" => RecordFieldValue::U64(record.shared_stream_id.unwrap_or(0)),
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


pub(crate) struct HistoryFile {
    pub path: PathBuf,
    pub partition_day: String,
    pub is_archive: bool,
}

pub(crate) fn discover_files(dir: &Path, time_range: &TimeRange) -> io::Result<Vec<HistoryFile>> {
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
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(out, "[");
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
                        let _ = writeln!(out, ",");
                    }
                    match serde_json::to_string(&record) {
                        Ok(json) => {
                            let _ = write!(out, "  {json}");
                        }
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
        let _ = writeln!(out);
    }
    let _ = writeln!(out, "]");
}

fn run_stream_history_viewer(input: &str) -> Result<(), String> {
    eprintln!("[INFO] All timestamps interpreted as UTC");

    let query = load_query(input)?;
    let time_range = resolve_time_range(&query)?;
    let filters = match query.filter.as_ref() {
        Some(raw) => CompiledFilter::compile(raw)?,
        None => CompiledFilter { fields: Vec::new() },
    };

    let dir = query.path.as_deref().unwrap_or("data/stream_history");
    let dir_path = Path::new(dir);

    let files = discover_files(dir_path, &time_range).map_err(|e| e.to_string())?;

    stream_output(files.as_slice(), &time_range, &filters);
    Ok(())
}

pub fn stream_history_viewer(input: &str) -> i32 {
    match run_stream_history_viewer(input) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("Error: {err}");
            let _ = io::stdout().flush();
            let _ = io::stderr().flush();
            1
        }
    }
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
    fn test_filter_unknown_field_is_rejected() {
        let mut raw = HashMap::new();
        raw.insert("usernam".to_string(), "alice".to_string());
        assert!(CompiledFilter::compile(&raw).is_err());
    }

    #[test]
    fn test_filter_supports_qos_metadata_fields() {
        let mut raw = HashMap::new();
        raw.insert("user_agent".to_string(), "VLC/3.0".to_string());
        raw.insert("cluster".to_string(), "live".to_string());
        raw.insert("video_codec".to_string(), "H.264".to_string());
        raw.insert("failure_stage".to_string(), "streaming".to_string());
        assert!(CompiledFilter::compile(&raw).is_ok());
    }

    #[test]
    fn test_filter_failure_stage() {
        let mut raw = HashMap::new();
        raw.insert("failure_stage".to_string(), "admission".to_string());
        let filter = CompiledFilter::compile(&raw).unwrap();

        let mut record = make_empty_record();
        record.failure_stage = Some(FailureStage::Admission);
        assert!(filter.matches(&record));

        record.failure_stage = Some(FailureStage::Streaming);
        assert!(!filter.matches(&record));

        record.failure_stage = None;
        assert!(!filter.matches(&record));
    }

    #[test]
    fn test_filter_provider_error_metadata() {
        let mut raw = HashMap::new();
        raw.insert("provider_http_status".to_string(), "503".to_string());
        raw.insert("provider_error_class".to_string(), "http_5xx".to_string());
        let filter = CompiledFilter::compile(&raw).unwrap();

        let mut record = make_empty_record();
        record.provider_http_status = Some(503);
        record.provider_error_class = Some("http_5xx".to_string());
        assert!(filter.matches(&record));

        record.provider_http_status = Some(404);
        assert!(!filter.matches(&record));
    }

    #[test]
    fn test_filter_shared_stream_markers() {
        let mut raw = HashMap::new();
        raw.insert("shared_joined_existing".to_string(), "true".to_string());
        raw.insert("shared_stream_id".to_string(), "77".to_string());
        let filter = CompiledFilter::compile(&raw).unwrap();

        let mut record = make_empty_record();
        record.shared_joined_existing = Some(true);
        record.shared_stream_id = Some(77);
        assert!(filter.matches(&record));

        record.shared_joined_existing = Some(false);
        assert!(!filter.matches(&record));
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
    fn test_run_stream_history_viewer_returns_error_instead_of_exiting_on_invalid_query() {
        let result = run_stream_history_viewer("{not-json");
        assert!(result.is_err());
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
            input_name: None,
            virtual_id: None,
            item_type: None,
            title: None,
            group: None,
            country: None,
            user_agent: None,
            shared: None,
            shared_joined_existing: None,
            shared_stream_id: None,
            provider_id: None,
            cluster: None,
            container: None,
            stream_url_hash: None,
            stream_identity_key: None,
            video_codec: None,
            audio_codec: None,
            audio_channels: None,
            resolution: None,
            fps: None,
            connect_ts_utc: None,
            disconnect_ts_utc: None,
            session_duration: None,
            bytes_sent: None,
            first_byte_latency_ms: None,
            provider_reconnect_count: None,
            failure_stage: None,
            provider_http_status: None,
            provider_error_class: None,
            connect_failure_reason: None,
            disconnect_reason: None,
            previous_session_id: None,
            target_id: None,
        }
    }
}

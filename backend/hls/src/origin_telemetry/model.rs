use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginResourceKind {
    Segment,
    Map,
    Key,
    Manifest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginFetchOutcome {
    Ok,
    HttpError,
    Timeout,
    Truncated,
    Cancelled,
    TransportError,
}

/// The phase a failed or interrupted fetch was in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginFetchPhaseError {
    Connect,
    Headers,
    Body,
    CacheWrite,
    Postprocess,
}

/// One origin fetch attempt. All `*_ms` marks are milliseconds since the fetch started,
/// except `queue_wait_ms`, `network_ms`, `cache_write_ms` and `postprocess_ms`, which are durations.
#[derive(Debug, Clone, Serialize)]
pub struct OriginFetchRecord {
    pub seq: u64,
    pub ts_utc_ms: u64,
    pub input_name: String,
    pub resource_kind: OriginResourceKind,
    pub segment_id: Option<String>,
    pub attempt_id: u64,
    pub attempt_no: u32,
    pub http_status: Option<u16>,
    pub bytes_read: u64,
    pub content_length: Option<u64>,
    pub media_duration_ms: Option<u64>,
    pub queue_wait_ms: u64,
    pub headers_ms: Option<u64>,
    pub first_byte_ms: Option<u64>,
    pub body_complete_ms: Option<u64>,
    pub network_ms: Option<u64>,
    pub cache_write_ms: Option<u64>,
    pub postprocess_ms: Option<u64>,
    pub cache_ready_ms: Option<u64>,
    /// Milliseconds from fetch start until this record was emitted, whatever the outcome.
    pub total_ms: u64,
    pub outcome: OriginFetchOutcome,
    pub error_phase: Option<OriginFetchPhaseError>,
}

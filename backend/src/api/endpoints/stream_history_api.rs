use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::io;
use std::path::Path;
use std::sync::Arc;

use crate::api::api_utils::json_or_bin_response;
use crate::api::endpoints::extract_accept_header::ExtractAcceptHeader;
use crate::api::model::AppState;
use crate::model::StreamHistoryRecord;
use crate::repository::{QosSnapshotRecord, QosSnapshotRepository, StreamHistoryFileReader};
use crate::utils::stream_history_viewer::{discover_files, resolve_time_range, CompiledFilter, StreamHistoryQuery, TimeRange};

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use shared::model::{
    PageRequestDto, PagedResponseDto, SearchMode, StreamHistoryEventType, StreamHistoryPageRequestDto,
    StreamHistoryProviderSummaryDto, QosSnapshotRecordDto, StreamHistoryQueryRequestDto, StreamHistoryRecordDto,
};

// TODO make shared Search fields

const MAX_STREAM_HISTORY_PAGE_HEAP_CAPACITY: usize = 100_000;

#[derive(Clone, Copy)]
enum SearchField {
    EventTsUtc,
    EventType,
    Title,
    Group,
    ApiUsername,
    ProviderName,
    ProviderId,
    BytesSent,
    FirstByteLatencyMs,
    UserAgent,
    ItemType,
    Container,
    DisconnectReason,
    SourceAddr,
    Country,
    Cluster,
}

impl SearchField {
    const ALL: [Self; 16] = [
        Self::EventTsUtc,
        Self::EventType,
        Self::Title,
        Self::Group,
        Self::ApiUsername,
        Self::ProviderName,
        Self::ProviderId,
        Self::BytesSent,
        Self::FirstByteLatencyMs,
        Self::UserAgent,
        Self::ItemType,
        Self::Container,
        Self::DisconnectReason,
        Self::SourceAddr,
        Self::Country,
        Self::Cluster,
    ];

    fn parse(value: &str) -> Option<Self> {
        match value {
            "event_ts_utc" => Some(Self::EventTsUtc),
            "event_type" => Some(Self::EventType),
            "title" => Some(Self::Title),
            "group" => Some(Self::Group),
            "api_username" => Some(Self::ApiUsername),
            "provider_name" => Some(Self::ProviderName),
            "provider_id" => Some(Self::ProviderId),
            "bytes_sent" => Some(Self::BytesSent),
            "first_byte_latency_ms" => Some(Self::FirstByteLatencyMs),
            "user_agent" => Some(Self::UserAgent),
            "item_type" => Some(Self::ItemType),
            "container" => Some(Self::Container),
            "disconnect_reason" => Some(Self::DisconnectReason),
            "source_addr" => Some(Self::SourceAddr),
            "country" => Some(Self::Country),
            "cluster" => Some(Self::Cluster),
            _ => None,
        }
    }
}

fn compile_search_fields(fields: Option<Vec<String>>) -> Result<Vec<SearchField>, String> {
    fields
        .unwrap_or_default()
        .into_iter()
        .map(|field| SearchField::parse(&field).ok_or_else(|| format!("Unknown search_field: {field}")))
        .collect()
}

#[derive(Deserialize)]
pub(crate) struct QosSnapshotQueryParams {
    pub limit: Option<usize>,
    #[serde(default)]
    #[serde(flatten)]
    pub filter: HashMap<String, String>,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

fn error_response(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, axum::Json(ErrorResponse { error: msg.into() })).into_response()
}

fn get_history_directory(app_state: &AppState) -> Option<String> {
    let config = app_state.app_config.config.load();
    config
        .reverse_proxy
        .as_ref()
        .and_then(|rp| rp.stream_history.as_ref())
        .filter(|sh| sh.stream_history_enabled)
        .map(|sh| sh.stream_history_directory.clone())
}

fn get_qos_storage_directory(app_state: &AppState) -> Option<String> {
    let config = app_state.app_config.config.load();
    get_qos_storage_directory_from_config(&config)
}

fn get_qos_storage_directory_from_config(config: &crate::model::Config) -> Option<String> {
    config
        .reverse_proxy
        .as_ref()
        .and_then(|rp| rp.qos_aggregation.as_ref())
        .filter(|qos| qos.enabled)
        .map(|_| config.storage_dir.clone())
}

fn compile_search_matcher(search: Option<&str>, mode: SearchMode) -> Result<Option<Regex>, String> {
    let Some(search) = search.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };

    match mode {
        SearchMode::Text => RegexBuilder::new(&regex::escape(search))
            .case_insensitive(true)
            .build()
            .map(Some)
            .map_err(|_| String::from("Invalid text search pattern")),
        SearchMode::Regex => Regex::new(search).map(Some).map_err(|_| String::from("Invalid regex pattern")),
    }
}

fn record_field_matches(record: &StreamHistoryRecord, field: SearchField, matcher: &Regex) -> bool {
    match field {
        SearchField::EventTsUtc => matcher.is_match(&record.event_ts_utc.to_string()),
        SearchField::EventType => matcher.is_match(&record.event_type.to_string()),
        SearchField::Title => record.title.as_deref().is_some_and(|value| matcher.is_match(value)),
        SearchField::Group => record.group.as_deref().is_some_and(|value| matcher.is_match(value)),
        SearchField::ApiUsername => record.api_username.as_deref().is_some_and(|value| matcher.is_match(value)),
        SearchField::ProviderName => record.provider_name.as_deref().is_some_and(|value| matcher.is_match(value)),
        SearchField::ProviderId => record.provider_id.is_some_and(|value| matcher.is_match(&value.to_string())),
        SearchField::BytesSent => record.bytes_sent.is_some_and(|value| matcher.is_match(&value.to_string())),
        SearchField::FirstByteLatencyMs => record
            .first_byte_latency_ms
            .is_some_and(|value| matcher.is_match(&value.to_string())),
        SearchField::UserAgent => record.user_agent.as_deref().is_some_and(|value| matcher.is_match(value)),
        SearchField::ItemType => record.item_type.as_ref().is_some_and(|value| matcher.is_match(&value.to_string())),
        SearchField::Container => record.container.as_deref().is_some_and(|value| matcher.is_match(value)),
        SearchField::DisconnectReason => record
            .disconnect_reason
            .as_ref()
            .is_some_and(|value| matcher.is_match(&value.to_string())),
        SearchField::SourceAddr => record.source_addr.as_deref().is_some_and(|value| matcher.is_match(value)),
        SearchField::Country => record.country.as_deref().is_some_and(|value| matcher.is_match(value)),
        SearchField::Cluster => record.cluster.as_deref().is_some_and(|value| matcher.is_match(value)),
    }
}

fn record_matches_search(record: &StreamHistoryRecord, matcher: Option<&Regex>, fields: &[SearchField]) -> bool {
    let Some(matcher) = matcher else {
        return true;
    };

    if fields.is_empty() {
        return SearchField::ALL.into_iter().any(|field| record_field_matches(record, field, matcher));
    }

    fields.iter().copied().any(|field| record_field_matches(record, field, matcher))
}

/// HLS session key for aggregation.
type HlsKey = (Option<String>, u64, Option<String>, Option<u32>, Option<String>);

#[derive(Default)]
struct HlsSessionAccumulator {
    first_connect: Option<StreamHistoryRecord>,
    last_disconnect: Option<StreamHistoryRecord>,
    total_bytes: u64,
    disconnect_count: usize,
}

impl HlsSessionAccumulator {
    fn observe(&mut self, record: StreamHistoryRecord) {
        self.total_bytes = self.total_bytes.saturating_add(record.bytes_sent.unwrap_or(0));
        match record.event_type {
            StreamHistoryEventType::Connect => {
                let should_replace = self
                    .first_connect
                    .as_ref()
                    .is_none_or(|current| record.event_ts_utc < current.event_ts_utc);
                if should_replace {
                    self.first_connect = Some(record);
                }
            }
            StreamHistoryEventType::Disconnect => {
                self.disconnect_count = self.disconnect_count.saturating_add(1);
                let should_replace = self
                    .last_disconnect
                    .as_ref()
                    .is_none_or(|current| record.event_ts_utc >= current.event_ts_utc);
                if should_replace {
                    self.last_disconnect = Some(record);
                }
            }
            _ => {}
        }
    }

    fn finish(self) -> Vec<StreamHistoryRecord> {
        let mut results = Vec::with_capacity(3);
        let Some(first_connect) = self.first_connect else {
            return results;
        };

        let session_duration = self.last_disconnect.as_ref().and_then(|last_disconnect| {
            if last_disconnect.event_ts_utc > first_connect.event_ts_utc {
                Some(last_disconnect.event_ts_utc - first_connect.event_ts_utc)
            } else {
                None
            }
        });

        let mut connect_row = first_connect;
        connect_row.session_duration = session_duration;
        connect_row.bytes_sent = Some(self.total_bytes);
        if let Some(last_disconnect) = self.last_disconnect.as_ref() {
            connect_row.disconnect_reason = last_disconnect.disconnect_reason;
        }

        if self.disconnect_count > 1 {
            let mut failure_row = connect_row.clone();
            failure_row.event_type = StreamHistoryEventType::Failure;
            failure_row.event_ts_utc = failure_row.event_ts_utc.saturating_add(1);
            failure_row.session_duration = None;
            failure_row.disconnect_reason =
                Some(shared::model::DisconnectReason::IntermediateFailures(self.disconnect_count - 1));
            results.push(failure_row);
        }

        results.push(connect_row);

        if let Some(mut disconnect_row) = self.last_disconnect {
            disconnect_row.bytes_sent = Some(self.total_bytes);
            results.push(disconnect_row);
        }

        results
    }
}

fn event_type_sort_rank(event_type: StreamHistoryEventType) -> u8 {
    match event_type {
        StreamHistoryEventType::Connect => 0,
        StreamHistoryEventType::Disconnect => 1,
        StreamHistoryEventType::Failure => 2,
        StreamHistoryEventType::ConnectFailed => 3,
    }
}

fn compare_history_records(left: &StreamHistoryRecord, right: &StreamHistoryRecord) -> std::cmp::Ordering {
    left.event_ts_utc
        .cmp(&right.event_ts_utc)
        .then_with(|| left.session_id.cmp(&right.session_id))
        .then_with(|| event_type_sort_rank(left.event_type).cmp(&event_type_sort_rank(right.event_type)))
        .then_with(|| left.provider_id.cmp(&right.provider_id))
        .then_with(|| left.virtual_id.cmp(&right.virtual_id))
        .then_with(|| left.source_addr.cmp(&right.source_addr))
        .then_with(|| left.api_username.cmp(&right.api_username))
        .then_with(|| left.title.cmp(&right.title))
        .then_with(|| left.group.cmp(&right.group))
        .then_with(|| left.container.cmp(&right.container))
        .then_with(|| left.country.cmp(&right.country))
        .then_with(|| left.cluster.cmp(&right.cluster))
        .then_with(|| left.input_name.as_deref().cmp(&right.input_name.as_deref()))
        .then_with(|| left.provider_name.as_deref().cmp(&right.provider_name.as_deref()))
}

struct RankedHistoryRecord {
    record: StreamHistoryRecord,
}

impl PartialEq for RankedHistoryRecord {
    fn eq(&self, other: &Self) -> bool { compare_history_records(&self.record, &other.record).is_eq() }
}

impl Eq for RankedHistoryRecord {}

impl PartialOrd for RankedHistoryRecord {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(other)) }
}

impl Ord for RankedHistoryRecord {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering { compare_history_records(&self.record, &other.record) }
}

struct TopHistoryPageCollector {
    capacity: usize,
    start: usize,
    limit: usize,
    total_items: u64,
    heap: BinaryHeap<Reverse<RankedHistoryRecord>>,
}

enum StreamHistoryPageQueryError {
    InvalidWindow(String),
    Internal(String),
}

impl TopHistoryPageCollector {
    fn new(page: u32, page_size: u16) -> Result<Self, String> {
        let start_u64 = u64::from(page.saturating_sub(1))
            .checked_mul(u64::from(page_size))
            .ok_or_else(|| format!("Invalid stream history page window: page={page}, page_size={page_size}"))?;
        let start = usize::try_from(start_u64)
            .map_err(|_| format!("Invalid stream history page start: page={page}, page_size={page_size}"))?;
        let limit = usize::from(page_size);
        let capacity = start
            .checked_add(limit)
            .ok_or_else(|| format!("Invalid stream history page capacity: start={start}, page_size={page_size}"))?;
        if capacity > MAX_STREAM_HISTORY_PAGE_HEAP_CAPACITY {
            return Err(format!(
                "Stream history page window too large: page={page}, page_size={page_size}, start={start}, capacity={capacity}, max={MAX_STREAM_HISTORY_PAGE_HEAP_CAPACITY}"
            ));
        }
        Ok(Self {
            capacity,
            start,
            limit,
            total_items: 0,
            heap: BinaryHeap::with_capacity(capacity),
        })
    }

    fn push(&mut self, record: StreamHistoryRecord, matcher: Option<&Regex>, fields: &[SearchField]) {
        if !record_matches_search(&record, matcher, fields) {
            return;
        }

        self.total_items = self.total_items.saturating_add(1);
        if self.capacity == 0 {
            return;
        }

        let ranked = RankedHistoryRecord {
            record,
        };

        if self.heap.len() < self.capacity {
            self.heap.push(Reverse(ranked));
            return;
        }

        if let Some(mut smallest_kept) = self.heap.peek_mut() {
            if ranked > smallest_kept.0 {
                *smallest_kept = Reverse(ranked);
            }
        }
    }

    fn finish(self, page: u32, page_size: u16) -> PagedResponseDto<StreamHistoryRecordDto> {
        let mut ranked_records: Vec<_> = self.heap.into_iter().map(|reverse| reverse.0).collect();
        ranked_records.sort_unstable_by(|left, right| compare_history_records(&right.record, &left.record));
        let items = ranked_records
            .into_iter()
            .skip(self.start)
            .take(self.limit)
            .map(|ranked| StreamHistoryRecordDto::from(&ranked.record))
            .collect();
        PagedResponseDto::new(items, page, page_size, self.total_items)
    }
}

fn push_hls_or_non_hls(
    record: StreamHistoryRecord,
    hls_sessions: &mut HashMap<HlsKey, HlsSessionAccumulator>,
    collector: &mut TopHistoryPageCollector,
    matcher: Option<&Regex>,
    fields: &[SearchField],
) {
    let is_hls = matches!(record.container.as_deref(), Some("mpegts" | "fmp4" | "hls"));
    let is_hls_session_event = matches!(
        record.event_type,
        StreamHistoryEventType::Connect | StreamHistoryEventType::Disconnect
    );
    if is_hls && is_hls_session_event {
        let key: HlsKey = (
            record.source_addr.clone(),
            record.session_id,
            record.provider_name.as_ref().map(ToString::to_string),
            record.virtual_id,
            record.input_name.as_ref().map(ToString::to_string),
        );
        hls_sessions.entry(key).or_default().observe(record);
    } else {
        collector.push(record, matcher, fields);
    }
}

fn paginate_stream_history_records<I>(
    records: I,
    batch_records: Vec<StreamHistoryRecord>,
    matcher: Option<&Regex>,
    fields: &[SearchField],
    page: u32,
    page_size: u16,
) -> Result<PagedResponseDto<StreamHistoryRecordDto>, String>
where
    I: Iterator<Item = StreamHistoryRecord>,
{
    let mut collector = TopHistoryPageCollector::new(page, page_size)?;
    let mut hls_sessions: HashMap<HlsKey, HlsSessionAccumulator> = HashMap::new();

    for record in records {
        push_hls_or_non_hls(record, &mut hls_sessions, &mut collector, matcher, fields);
    }

    for record in batch_records {
        push_hls_or_non_hls(record, &mut hls_sessions, &mut collector, matcher, fields);
    }

    for session in hls_sessions.into_values() {
        for record in session.finish() {
            collector.push(record, matcher, fields);
        }
    }

    Ok(collector.finish(page, page_size))
}

#[cfg(test)]
/// Aggregate HLS records (same logic as frontend `aggregate_hls_records`).
fn aggregate_hls_session(records: &[&StreamHistoryRecord]) -> Vec<StreamHistoryRecord> {
    let mut accumulator = HlsSessionAccumulator::default();
    for record in records {
        accumulator.observe((*record).clone());
    }
    accumulator.finish()
}

#[cfg(test)]
fn paginate_aggregated_records(
    aggregated: Vec<StreamHistoryRecord>,
    matcher: Option<&Regex>,
    fields: &[SearchField],
    page: u32,
    page_size: u16,
) -> PagedResponseDto<StreamHistoryRecordDto> {
    let mut filtered = Vec::new();
    for record in aggregated {
        if record_matches_search(&record, matcher, fields) {
            filtered.push(record);
        }
    }
    filtered.sort_unstable_by_key(|record| Reverse(record.event_ts_utc));

    let Ok(mut collector) = TopHistoryPageCollector::new(page, page_size) else { return PagedResponseDto::new(Vec::new(), page, page_size, 0) };
    for record in filtered {
        collector.push(record, None, &[]);
    }
    collector.finish(page, page_size)
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn stream_history_page_query(
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<StreamHistoryPageRequestDto>,
) -> Response {
    let Some(history_dir) = get_history_directory(&app_state) else {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "Stream history is not enabled");
    };

    // Normalize pagination
    let mut page_req = PageRequestDto::normalized(params.page, params.page_size);
    page_req.normalize_page();
    page_req.normalize_page_size();

    let search_mode = match params.search_mode.as_deref() {
        Some("regex") => SearchMode::Regex,
        Some("text") | None => SearchMode::Text,
        Some(other) => return error_response(StatusCode::BAD_REQUEST, format!("Unknown search_mode: {other}")),
    };

    let search_matcher = match compile_search_matcher(params.search.as_deref(), search_mode) {
        Ok(matcher) => matcher,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };

    // Build time range
    let query = StreamHistoryQuery {
        from: params.from.clone(),
        to: params.to.clone(),
        path: None,
        filter: None,
    };

    let time_range = match resolve_time_range(&query) {
        Ok(tr) => tr,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, e),
    };

    let search_fields = match compile_search_fields(params.search_fields) {
        Ok(fields) => fields,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    };
    let page = page_req.page;
    let page_size = page_req.page_size;
    let batch_records = if let Some(hw) = app_state
        .connection_manager
        .history_writer()
        .load()
        .as_ref()
    {
        hw.get_current_batch().await.unwrap_or_else(|_| Vec::new())
    } else {
        Vec::new()
    };

    let result = tokio::task::spawn_blocking(move || {
        let empty_filter = CompiledFilter::compile(&HashMap::new())
            .map(Arc::new)
            .map_err(|err| StreamHistoryPageQueryError::Internal(format!("Failed to compile stream history filter: {err}")))?;

        let file_iter = futures::executor::block_on(collect_records_iter(&history_dir, time_range, empty_filter))
            .map_err(|err| StreamHistoryPageQueryError::Internal(format!("Failed to open stream history files: {err}")))?;

        paginate_stream_history_records(file_iter, batch_records, search_matcher.as_ref(), &search_fields, page, page_size)
            .map_err(StreamHistoryPageQueryError::InvalidWindow)
    }).await;

    match result {
        Ok(Ok(response)) => axum::Json(response).into_response(),
        Ok(Err(StreamHistoryPageQueryError::InvalidWindow(message))) => error_response(StatusCode::BAD_REQUEST, message),
        Ok(Err(StreamHistoryPageQueryError::Internal(message))) => error_response(StatusCode::INTERNAL_SERVER_ERROR, message),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, format!("Task failed: {e}")),
    }
}

pub(crate) async fn stream_history_summary_query(
    ExtractAcceptHeader(accept): ExtractAcceptHeader,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<StreamHistoryQueryRequestDto>,
) -> Response {
    let Some(history_dir) = get_history_directory(&app_state) else {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "Stream history is not enabled");
    };

    let query = StreamHistoryQuery {
        from: params.from,
        to: params.to,
        path: None,
        filter: if params.filter.is_empty() { None } else { Some(params.filter) },
    };

    let time_range = match resolve_time_range(&query) {
        Ok(tr) => tr,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, e),
    };

    let filters = match query.filter.as_ref() {
        Some(raw) => match CompiledFilter::compile(raw) {
            Ok(f) => f,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, e),
        },
        None => match CompiledFilter::compile(&HashMap::new()) {
            Ok(filter) => filter,
            Err(err) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, err),
        },
    };

    let filters_arc = Arc::new(filters);
    let batch_records = if let Some(hw) = app_state
        .connection_manager
        .history_writer()
        .load()
        .as_ref()
    {
        hw.get_current_batch().await.unwrap_or_else(|_| Vec::new())
    } else {
        Vec::new()
    };

    let result = tokio::task::spawn_blocking(move || {
        let file_iter = futures::executor::block_on(collect_records_iter(&history_dir, time_range, filters_arc))?;
        let combined = batch_records.into_iter().chain(file_iter);
        io::Result::Ok(aggregate_provider_summaries_from_iter(combined))
    })
    .await;

    match result {
        Ok(Ok(summaries)) => json_or_bin_response(accept.as_deref(), &summaries).into_response(),
        Ok(Err(err)) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to discover history files: {err}"),
        ),
        Err(err) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Stream history summary task failed: {err}"),
        ),
    }
}

pub(crate) async fn qos_snapshot_query(
    ExtractAcceptHeader(accept): ExtractAcceptHeader,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<QosSnapshotQueryParams>,
) -> Response {
    let Some(storage_dir) = get_qos_storage_directory(&app_state) else {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "QoS aggregation is not enabled");
    };

    let filters = match CompiledQosSnapshotFilter::compile(&params.filter) {
        Ok(filter) => filter,
        Err(err) => return error_response(StatusCode::BAD_REQUEST, err),
    };
    let limit = params.limit.unwrap_or(100).max(1);

    let result = tokio::task::spawn_blocking(move || {
        collect_filtered_qos_snapshots(Path::new(&storage_dir), &filters, limit)
    })
        .await;

    match result {
        Ok(Ok(records)) => json_or_bin_response(accept.as_deref(), &records).into_response(),
        Ok(Err(err)) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to read QoS snapshots: {err}"),
        ),
        Err(err) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("QoS snapshot query task failed: {err}"),
        ),
    }
}

pub(crate) async fn qos_snapshot_detail_query(
    ExtractAcceptHeader(accept): ExtractAcceptHeader,
    State(app_state): State<Arc<AppState>>,
    AxumPath(stream_identity_key): AxumPath<String>,
) -> Response {
    let Some(storage_dir) = get_qos_storage_directory(&app_state) else {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "QoS aggregation is not enabled");
    };

    let result = tokio::task::spawn_blocking(move || load_qos_snapshot(&storage_dir, &stream_identity_key))
        .await;

    match result {
        Ok(Ok(Some(record))) => json_or_bin_response(accept.as_deref(), &record).into_response(),
        Ok(Ok(None)) => error_response(StatusCode::NOT_FOUND, "QoS snapshot not found"),
        Ok(Err(err)) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to read QoS snapshot detail: {err}"),
        ),
        Err(err) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("QoS snapshot detail task failed: {err}"),
        ),
    }
}

/// Iterator over all filtered `StreamHistoryRecord`s from history files in a directory.
/// Filters are applied lazily — records are never collected into a Vec.
/// Errors opening files or reading records are logged but do not stop iteration.
struct RecordFileIter {
    files: std::vec::IntoIter<crate::utils::stream_history_viewer::HistoryFile>,
    current_iter: Option<Box<dyn Iterator<Item=std::io::Result<StreamHistoryRecord>>>>,
    time_range: TimeRange,
    filters: Arc<CompiledFilter>,
}

impl RecordFileIter {
    async fn new(dir: &str, time_range: TimeRange, filters: Arc<CompiledFilter>) -> io::Result<Self> {
        let files = discover_files(Path::new(dir), &time_range).await?;
        Ok(Self {
            files: files.into_iter(),
            current_iter: None,
            time_range,
            filters,
        })
    }
}

impl Iterator for RecordFileIter {
    type Item = StreamHistoryRecord;

    fn next(&mut self) -> Option<Self::Item> {
        let (range_start, range_end) = self.time_range;

        loop {
            if self.current_iter.is_none() {
                let file = self.files.next()?;
                let iter: Box<dyn Iterator<Item=io::Result<StreamHistoryRecord>>> = if file.is_archive {
                    match StreamHistoryFileReader::from_archive(&file.path, Some(self.time_range)) {
                        Ok((r, _)) => Box::new(r),
                        Err(e) => {
                            log::warn!("Failed to open archive {}: {e}", file.path.display());
                            continue;
                        }
                    }
                } else {
                    match StreamHistoryFileReader::from_pending(&file.path, Some(self.time_range)) {
                        Ok((r, _)) => Box::new(r),
                        Err(e) => {
                            log::warn!("Failed to open pending file {}: {e}", file.path.display());
                            continue;
                        }
                    }
                };
                self.current_iter = Some(iter);
            }

            match self.current_iter.as_mut().and_then(std::iter::Iterator::next) {
                Some(Ok(record)) => {
                    if record.event_ts_utc < range_start || record.event_ts_utc > range_end {
                        continue;
                    }
                    if !self.filters.matches(&record) {
                        continue;
                    }
                    return Some(record);
                }
                Some(Err(e)) => {
                    log::warn!("Failed to read record: {e}");
                }
                None => {
                    self.current_iter = None;
                }
            }
        }
    }
}


/// Returns a lazy iterator over filtered records from history files.
async fn collect_records_iter(
    dir: &str,
    time_range: TimeRange,
    filters: Arc<CompiledFilter>,
) -> io::Result<RecordFileIter> {
    RecordFileIter::new(dir, time_range, filters).await
}

#[cfg(test)]
// Test-only baseline implementation used to validate the iterator-based summary
// path against a simpler whole-slice aggregation.
pub(crate) fn aggregate_provider_summaries(
    records: &[StreamHistoryRecord],
) -> Vec<StreamHistoryProviderSummaryDto> {
    #[derive(Default)]
    struct Acc {
        session_count: u64,
        disconnect_count: u64,
        total_bytes_sent: u64,
        total_duration: u64,
        duration_count: u64,
        total_first_byte_latency: u64,
        first_byte_count: u64,
    }

    let mut by_provider: std::collections::BTreeMap<Arc<str>, Acc> = std::collections::BTreeMap::new();

    for record in records {
        let provider_name = record.provider_name.clone().unwrap_or_else(|| Arc::from("unknown"));
        let acc = by_provider.entry(provider_name).or_default();
        acc.session_count = acc.session_count.saturating_add(1);
        if matches!(record.event_type, StreamHistoryEventType::Disconnect) {
            acc.disconnect_count = acc.disconnect_count.saturating_add(1);
        }
        acc.total_bytes_sent = acc.total_bytes_sent.saturating_add(record.bytes_sent.unwrap_or(0));
        if let Some(duration) = record.session_duration {
            acc.total_duration = acc.total_duration.saturating_add(duration);
            acc.duration_count = acc.duration_count.saturating_add(1);
        }
        if let Some(latency) = record.first_byte_latency_ms {
            acc.total_first_byte_latency = acc.total_first_byte_latency.saturating_add(latency);
            acc.first_byte_count = acc.first_byte_count.saturating_add(1);
        }
    }

    by_provider
        .into_iter()
        .map(|(provider_name, acc)| StreamHistoryProviderSummaryDto {
            provider_name: provider_name.to_string(),
            session_count: acc.session_count,
            disconnect_count: acc.disconnect_count,
            total_bytes_sent: acc.total_bytes_sent,
            avg_session_duration_secs: acc.total_duration.checked_div(acc.duration_count),
            avg_first_byte_latency_ms: acc.total_first_byte_latency.checked_div(acc.first_byte_count),
        })
        .collect()
}

/// Lazily aggregates provider summaries from a (possibly infinite) iterator of records.
/// Unlike `aggregate_provider_summaries` which collects into a Vec first, this processes
/// records one-by-one without any intermediate allocation.
pub fn aggregate_provider_summaries_from_iter<I: Iterator<Item=StreamHistoryRecord>>(
    records: I,
) -> Vec<StreamHistoryProviderSummaryDto> {
    #[derive(Default)]
    struct Acc {
        session_count: u64,
        disconnect_count: u64,
        total_bytes_sent: u64,
        total_duration: u64,
        duration_count: u64,
        total_first_byte_latency: u64,
        first_byte_count: u64,
    }

    let mut by_provider: std::collections::BTreeMap<Arc<str>, Acc> = std::collections::BTreeMap::new();

    for record in records {
        let provider_name = record.provider_name.unwrap_or_else(|| Arc::from("unknown"));
        let acc = by_provider.entry(provider_name).or_default();
        acc.session_count = acc.session_count.saturating_add(1);
        if matches!(record.event_type, StreamHistoryEventType::Disconnect) {
            acc.disconnect_count = acc.disconnect_count.saturating_add(1);
        }
        acc.total_bytes_sent = acc.total_bytes_sent.saturating_add(record.bytes_sent.unwrap_or(0));
        if let Some(duration) = record.session_duration {
            acc.total_duration = acc.total_duration.saturating_add(duration);
            acc.duration_count = acc.duration_count.saturating_add(1);
        }
        if let Some(latency) = record.first_byte_latency_ms {
            acc.total_first_byte_latency = acc.total_first_byte_latency.saturating_add(latency);
            acc.first_byte_count = acc.first_byte_count.saturating_add(1);
        }
    }

    by_provider
        .into_iter()
        .map(|(provider_name, acc)| StreamHistoryProviderSummaryDto {
            provider_name: provider_name.to_string(),
            session_count: acc.session_count,
            disconnect_count: acc.disconnect_count,
            total_bytes_sent: acc.total_bytes_sent,
            avg_session_duration_secs: acc.total_duration.checked_div(acc.duration_count),
            avg_first_byte_latency_ms: acc.total_first_byte_latency.checked_div(acc.first_byte_count),
        })
        .collect()
}

#[derive(Debug, Clone)]
struct CompiledQosSnapshotFilter {
    stream_identity_key: Option<String>,
    input_name: Option<String>,
    provider_name: Option<String>,
    item_type: Option<String>,
    target_name: Option<String>,
}

impl CompiledQosSnapshotFilter {
    fn compile(raw: &HashMap<String, String>) -> Result<Self, String> {
        let parse_string = |key: &str| -> Result<Option<String>, String> {
            Ok(raw.get(key).filter(|value| !value.trim().is_empty()).cloned())
        };

        Ok(Self {
            stream_identity_key: raw.get("stream_identity_key").cloned().filter(|value| !value.trim().is_empty()),
            input_name: raw.get("input_name").cloned().filter(|value| !value.trim().is_empty()),
            provider_name: raw.get("provider_name").cloned().filter(|value| !value.trim().is_empty()),
            item_type: raw.get("item_type").cloned().filter(|value| !value.trim().is_empty()),
            target_name: parse_string("target_name")?,
        })
    }

    fn matches(&self, snapshot: &QosSnapshotRecord) -> bool {
        self.stream_identity_key
            .as_ref()
            .is_none_or(|value| snapshot.stream_identity_key == *value)
            && self.input_name.as_ref().is_none_or(|value| snapshot.input_name.as_ref() == value)
            && self.provider_name.as_ref().is_none_or(|value| snapshot.provider_name.as_ref() == value)
            && self.item_type.as_ref().is_none_or(|value| snapshot.item_type.as_str() == value)
            && self.target_name.as_ref().is_none_or(|value| snapshot.target_name.as_ref() == value)
    }
}

fn collect_filtered_qos_snapshots(
    storage_dir: &Path,
    filter: &CompiledQosSnapshotFilter,
    limit: usize,
) -> io::Result<Vec<QosSnapshotRecordDto>> {
    let mut filtered = Vec::with_capacity(limit);
    QosSnapshotRepository::for_each_snapshot_read_only(storage_dir, |snapshot| {
        if !filter.matches(snapshot) {
            return;
        }
        filtered.push(snapshot.clone());
        filtered.sort_by(qos_snapshot_order);
        if filtered.len() > limit {
            filtered.pop();
        }
    })?;
    Ok(filtered.iter().map(QosSnapshotRecordDto::from).collect())
}

fn qos_snapshot_order(left: &QosSnapshotRecord, right: &QosSnapshotRecord) -> std::cmp::Ordering {
    right
        .window_24h
        .score
        .cmp(&left.window_24h.score)
        .then_with(|| left.stream_identity_key.cmp(&right.stream_identity_key))
}

fn load_qos_snapshot(storage_dir: &str, stream_identity_key: &str) -> io::Result<Option<QosSnapshotRecordDto>> {
    Ok(QosSnapshotRepository::get_snapshot_read_only(Path::new(storage_dir), stream_identity_key)?
        .as_ref()
        .map(QosSnapshotRecordDto::from))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{QosAggregationConfig, ResourceRetryConfig, ReverseProxyConfig};
    use crate::repository::{
        QosSnapshotDailyBucket, QosSnapshotRecord, QosSnapshotWindow,
    };
    use shared::model::{DisconnectReason, PlaylistItemType};
    use shared::utils::Internable;

    fn make_record(
        provider_name: &str,
        duration: Option<u64>,
        bytes_sent: Option<u64>,
        first_byte_latency_ms: Option<u64>,
        disconnect_reason: Option<DisconnectReason>,
    ) -> StreamHistoryRecord {
        StreamHistoryRecord {
            schema_version: 1,
            event_type: StreamHistoryEventType::Disconnect,
            event_ts_utc: 1,
            partition_day_utc: String::from("2026-03-22"),
            session_id: 1,
            source_addr: None,
            api_username: Some(String::from("alice")),
            provider_name: Some(provider_name.intern()),
            provider_username: None,
            input_name: Some("input".intern()),
            virtual_id: Some(1),
            item_type: Some(PlaylistItemType::Live),
            title: Some(String::from("Title")),
            group: None,
            country: None,
            user_agent: Some(String::from("VLC/3.0")),
            shared: Some(false),
            shared_joined_existing: None,
            shared_stream_id: None,
            provider_id: Some(1),
            cluster: Some(String::from("live")),
            container: None,
            stream_url_hash: None,
            stream_identity_key: None,
            video_codec: None,
            audio_codec: None,
            audio_channels: None,
            resolution: None,
            fps: None,
            connect_ts_utc: Some(1),
            disconnect_ts_utc: Some(2),
            session_duration: duration,
            bytes_sent,
            first_byte_latency_ms,
            provider_reconnect_count: None,
            failure_stage: None,
            provider_http_status: None,
            provider_error_class: None,
            connect_failure_reason: None,
            disconnect_reason,
            previous_session_id: None,
            target_name: None,
        }
    }

    #[test]
    fn provider_summary_aggregates_qos_metrics() {
        let summaries = aggregate_provider_summaries(&[
            make_record("acme", Some(10), Some(100), Some(50), Some(DisconnectReason::ClientClosed)),
            make_record("acme", Some(20), Some(300), Some(150), Some(DisconnectReason::ProviderError)),
            make_record("beta", Some(5), Some(25), None, Some(DisconnectReason::ClientClosed)),
        ]);

        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].provider_name.as_str(), "acme");
        assert_eq!(summaries[0].session_count, 2);
        assert_eq!(summaries[0].total_bytes_sent, 400);
        assert_eq!(summaries[0].avg_session_duration_secs, Some(15));
        assert_eq!(summaries[0].avg_first_byte_latency_ms, Some(100));
        assert_eq!(summaries[0].disconnect_count, 2);
    }

    #[test]
    fn paged_stream_history_returns_only_requested_slice() {
        let mut first = make_record("acme", Some(10), Some(100), Some(50), Some(DisconnectReason::ClientClosed));
        first.event_ts_utc = 30;
        let mut second = make_record("beta", Some(20), Some(200), Some(40), Some(DisconnectReason::ClientClosed));
        second.event_ts_utc = 20;
        let mut third = make_record("gamma", Some(30), Some(300), Some(30), Some(DisconnectReason::ClientClosed));
        third.event_ts_utc = 10;

        let response = paginate_aggregated_records(vec![first, second, third], None, &[], 2, 1);

        assert_eq!(response.total_items, 3);
        assert_eq!(response.items.len(), 1);
        assert_eq!(response.items[0].event_ts_utc, 20);
        assert_eq!(response.page, 2);
        assert!(response.has_prev);
        assert!(response.has_next);
    }

    #[test]
    fn record_search_respects_selected_fields() {
        let mut record = make_record("acme", Some(10), Some(100), Some(50), Some(DisconnectReason::ClientClosed));
        record.title = Some(String::from("Alpha"));
        record.group = Some(String::from("Beta"));

        let matcher = compile_search_matcher(Some("beta"), SearchMode::Text)
            .expect("text matcher should compile")
            .expect("text matcher should exist");

        assert!(record_matches_search(&record, Some(&matcher), &[]));
        assert!(record_matches_search(&record, Some(&matcher), &[SearchField::Group]));
        assert!(!record_matches_search(&record, Some(&matcher), &[SearchField::Title]));
    }

    #[test]
    fn aggregate_hls_session_preserves_connect_failure_disconnect_rows() {
        let mut connect = make_record("acme", Some(10), Some(0), Some(50), Some(DisconnectReason::ClientClosed));
        connect.event_type = StreamHistoryEventType::Connect;
        connect.event_ts_utc = 100;
        connect.bytes_sent = None;

        let mut first_disconnect = make_record("acme", Some(10), Some(120), Some(50), Some(DisconnectReason::ProviderError));
        first_disconnect.event_ts_utc = 110;

        let mut final_disconnect = make_record("acme", Some(10), Some(80), Some(50), Some(DisconnectReason::ClientClosed));
        final_disconnect.event_ts_utc = 120;

        let rows = aggregate_hls_session(&[&connect, &first_disconnect, &final_disconnect]);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].event_type, StreamHistoryEventType::Failure);
        assert_eq!(rows[0].disconnect_reason, Some(DisconnectReason::IntermediateFailures(1)));
        assert_eq!(rows[1].event_type, StreamHistoryEventType::Connect);
        assert_eq!(rows[1].bytes_sent, Some(200));
        assert_eq!(rows[1].disconnect_reason, Some(DisconnectReason::ClientClosed));
        assert_eq!(rows[2].event_type, StreamHistoryEventType::Disconnect);
        assert_eq!(rows[2].bytes_sent, Some(200));
    }

    #[test]
    fn paged_stream_history_keeps_hls_connect_failed_rows() -> Result<(), String> {
        let mut failed = make_record("acme", None, Some(0), None, None);
        failed.event_type = StreamHistoryEventType::ConnectFailed;
        failed.event_ts_utc = 77;
        failed.container = Some(String::from("hls"));

        let response = paginate_stream_history_records(vec![failed].into_iter(), Vec::new(), None, &[], 1, 50)?;
        assert_eq!(response.total_items, 1);
        assert_eq!(response.items.len(), 1);
        assert_eq!(response.items[0].event_type, StreamHistoryEventType::ConnectFailed);
        Ok(())
    }

    #[test]
    fn paged_stream_history_rejects_oversized_page_window_before_heap_allocation() {
        let result = TopHistoryPageCollector::new(u32::MAX, 200);

        assert!(result.is_err());
    }

    #[test]
    fn paged_stream_history_uses_deterministic_tie_breakers_for_equal_timestamps() -> Result<(), String> {
        let mut left = make_record("acme", Some(10), Some(100), Some(50), Some(DisconnectReason::ClientClosed));
        left.event_ts_utc = 100;
        left.session_id = 10;

        let mut right = make_record("acme", Some(10), Some(200), Some(50), Some(DisconnectReason::ProviderError));
        right.event_ts_utc = 100;
        right.session_id = 20;

        let first = paginate_stream_history_records(
            vec![left.clone(), right.clone()].into_iter(),
            Vec::new(),
            None,
            &[],
            1,
            1,
        )?;
        let second = paginate_stream_history_records(
            vec![right, left].into_iter(),
            Vec::new(),
            None,
            &[],
            1,
            1,
        )?;

        assert_eq!(first.items.len(), 1);
        assert_eq!(second.items.len(), 1);
        assert_eq!(first.items[0].session_id, second.items[0].session_id);
        Ok(())
    }

    fn make_qos_snapshot(
        stream_identity_key: &str,
        input_name: &str,
        provider_name: &str,
        target_name: &str,
        score_24h: u8,
    ) -> QosSnapshotRecord {
        QosSnapshotRecord {
            stream_identity_key: stream_identity_key.to_string(),
            input_name: input_name.intern(),
            target_name: target_name.intern(),
            provider_name: provider_name.intern(),
            provider_id: 1,
            virtual_id: 101,
            item_type: PlaylistItemType::Live,
            updated_at: 100,
            last_event_at: 99,
            window_24h: QosSnapshotWindow {
                score: score_24h,
                confidence: 70,
                ..QosSnapshotWindow::default()
            },
            window_7d: QosSnapshotWindow::default(),
            window_30d: QosSnapshotWindow::default(),
            daily_buckets: std::collections::BTreeMap::<String, QosSnapshotDailyBucket>::new(),
        }
    }

    #[test]
    fn qos_snapshot_filter_matches_identity_and_provider_fields() {
        let snapshots = vec![
            make_qos_snapshot("stream-a", "input-a", "provider-a", "target-a", 81),
            make_qos_snapshot("stream-b", "input-b", "provider-b", "target-b", 55),
        ];

        let mut raw = HashMap::new();
        raw.insert("provider_name".to_string(), "provider-b".to_string());
        raw.insert("target_name".to_string(), "target-b".to_string());
        let filter = CompiledQosSnapshotFilter::compile(&raw).expect("qos snapshot filter should compile");

        let mut filtered = snapshots
            .into_iter()
            .filter(|snapshot| filter.matches(snapshot))
            .collect::<Vec<_>>();
        filtered.sort_by(qos_snapshot_order);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].stream_identity_key, "stream-b");
    }

    #[test]
    fn get_qos_storage_directory_requires_enabled_qos_aggregation() {
        let mut cfg = crate::model::Config {
            storage_dir: "/var/lib/tuliprox".to_string(),
            reverse_proxy: Some(ReverseProxyConfig {
                resource_rewrite_disabled: false,
                rewrite_secret: [0; 16],
                resource_retry: ResourceRetryConfig::default(),
                disabled_header: None,
                stream: None,
                cache: None,
                rate_limit: None,
                geoip: None,
                stream_history: None,
                qos_aggregation: Some(QosAggregationConfig {
                    enabled: false,
                    interval_secs: 300,
                }),
                hls_cache: None,
            }),
            ..crate::model::Config::default()
        };

        assert!(get_qos_storage_directory_from_config(&cfg).is_none());

        if let Some(reverse_proxy) = cfg.reverse_proxy.as_mut() {
            reverse_proxy.qos_aggregation = Some(QosAggregationConfig {
                enabled: true,
                interval_secs: 300,
            });
        }

        assert_eq!(
            get_qos_storage_directory_from_config(&cfg).as_deref(),
            Some("/var/lib/tuliprox")
        );
    }

    #[test]
    fn filter_qos_snapshots_orders_by_score_and_keeps_exact_match_filters() {
        let snapshots = vec![
            make_qos_snapshot("stream-a", "input-a", "provider-a", "target-a", 60),
            make_qos_snapshot("stream-b", "input-b", "provider-a", "target-a", 80),
            make_qos_snapshot("stream-c", "input-c", "provider-b", "target-b", 95),
        ];

        let mut raw = HashMap::new();
        raw.insert("provider_name".to_string(), "provider-a".to_string());
        let filter = CompiledQosSnapshotFilter::compile(&raw).expect("qos snapshot filter should compile");

        let mut filtered = snapshots
            .into_iter()
            .filter(|snapshot| filter.matches(snapshot))
            .collect::<Vec<_>>();
        filtered.sort_by(qos_snapshot_order);
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].stream_identity_key, "stream-b");
        assert_eq!(filtered[1].stream_identity_key, "stream-a");
    }
}

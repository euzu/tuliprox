use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Arc;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::api::api_utils::json_or_bin_response;
use crate::api::endpoints::extract_accept_header::ExtractAcceptHeader;
use crate::api::model::AppState;
use crate::model::{EventType, StreamHistoryRecord};
use crate::repository::{QosSnapshotRecord, QosSnapshotRepository, StreamHistoryFileReader};
use crate::utils::stream_history_viewer::{
    discover_files, resolve_time_range, CompiledFilter,
    StreamHistoryQuery, TimeRange,
};

#[derive(Deserialize)]
pub(crate) struct HistoryQueryParams {
    pub from: Option<String>,
    pub to: Option<String>,
    #[serde(default)]
    #[serde(flatten)]
    pub filter: HashMap<String, String>,
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

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ProviderSummary {
    pub provider_name: Arc<str>,
    pub session_count: u64,
    pub disconnect_count: u64,
    pub total_bytes_sent: u64,
    pub avg_session_duration_secs: Option<u64>,
    pub avg_first_byte_latency_ms: Option<u64>,
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

pub(crate) async fn stream_history_query(
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<HistoryQueryParams>,
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
        None => CompiledFilter::compile(&HashMap::new()).unwrap_or_else(|_| unreachable!()),
    };

    // Collect file records and batch records on blocking thread, then combine.
    // batch_records must be fetched on a real OS thread (not the async thread),
    // so we include it inside spawn_blocking where blocking_recv() is safe.
    let app_state_clone = Arc::clone(&app_state);
    let mut file_records = collect_records(&history_dir, &time_range, &filters).await.unwrap_or_default();
    let batch_records = if let Some(hw) = app_state_clone
        .connection_manager
        .history_writer()
        .load()
        .as_ref()
    {
        hw.get_current_batch().await.unwrap_or_else(|_| Vec::new())
    } else {
        Vec::new()
    };

    let mut all_records = batch_records;
    all_records.append(&mut file_records);
    axum::Json(all_records).into_response()
}

pub(crate) async fn stream_history_summary_query(
    ExtractAcceptHeader(accept): ExtractAcceptHeader,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<HistoryQueryParams>,
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
        None => CompiledFilter::compile(&HashMap::new()).unwrap_or_else(|_| unreachable!()),
    };

    // Chain batch records (in-memory) with file records iterator.
    // batch_records must be fetched on a real OS thread (not the async thread),
    // so we include it inside spawn_blocking where blocking_recv() is safe.
    let filters_arc = Arc::new(filters);
    let app_state_clone = Arc::clone(&app_state);
    // Fetch batch records from within the blocking thread so blocking_recv() is safe
    let batch_records = if let Some(hw) = app_state_clone
        .connection_manager
        .history_writer()
        .load()
        .as_ref()
    {
        hw.get_current_batch().await.unwrap_or_else(|_| Vec::new())
    } else {
        Vec::new()
    };

    let file_iter = collect_records_iter(&history_dir, time_range, filters_arc).await;

    let combined = batch_records.into_iter().chain(file_iter.into_iter().flatten());
    let summaries = aggregate_provider_summaries_from_iter(combined);
    json_or_bin_response(accept.as_deref(), &summaries).into_response()
    // match result {
    //     Ok(Ok(summaries)) => ,
    //     Ok(Err(e)) => {
    //         if e.kind() == io::ErrorKind::NotFound {
    //             json_or_bin_response(accept.as_deref(), &Vec::<ProviderSummary>::new()).into_response()
    //         } else {
    //             error_response(StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to summarize history: {e}"))
    //         }
    //     }
    //     Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, format!("History summary task failed: {e}")),
    // }
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
                let iter: Box<dyn Iterator<Item = io::Result<StreamHistoryRecord>>> = if file.is_archive {
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

/// Returns a Vec of filtered records from history files.
/// Used by `stream_history_query` to collect all records before responding.
async fn collect_records(
    dir: &str,
    time_range: &TimeRange,
    filters: &CompiledFilter,
) -> io::Result<Vec<StreamHistoryRecord>> {
    let files = discover_files(Path::new(dir), time_range).await?;
    let (range_start, range_end) = *time_range;

    let mut records = Vec::new();

    for file in &files {
        let iter: Box<dyn Iterator<Item=io::Result<StreamHistoryRecord>>> = if file.is_archive {
            let (reader, _) = StreamHistoryFileReader::from_archive(&file.path, Some(*time_range))?;
            Box::new(reader)
        } else {
            let (reader, _) = StreamHistoryFileReader::from_pending(&file.path, Some(*time_range))?;
            Box::new(reader)
        };

        for result in iter {
            let record = result?;
            if record.event_ts_utc < range_start || record.event_ts_utc > range_end {
                continue;
            }
            if !filters.matches(&record) {
                continue;
            }
            records.push(record);
        }
    }

    Ok(records)
}

/// Returns a lazy iterator over filtered records from history files.
async fn collect_records_iter(
    dir: &str,
    time_range: TimeRange,
    filters: Arc<CompiledFilter>,
) -> io::Result<RecordFileIter> {
    RecordFileIter::new(dir, time_range, filters).await
}

#[allow(dead_code)]
pub(crate) fn aggregate_provider_summaries(
    records: &[StreamHistoryRecord],
) -> Vec<ProviderSummary> {
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
        if matches!(record.event_type, EventType::Disconnect) {
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
        .map(|(provider_name, acc)| ProviderSummary {
            provider_name,
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
) -> Vec<ProviderSummary> {
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
        if matches!(record.event_type, EventType::Disconnect) {
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
        .map(|(provider_name, acc)| ProviderSummary {
            provider_name,
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
            && self.item_type.as_ref().is_none_or(|value| snapshot.item_type.as_ref() == value)
            && self.target_name.as_ref().is_none_or(|value| snapshot.target_name.as_ref() == value)
    }
}

fn collect_filtered_qos_snapshots(
    storage_dir: &Path,
    filter: &CompiledQosSnapshotFilter,
    limit: usize,
) -> io::Result<Vec<QosSnapshotRecord>> {
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
    Ok(filtered)
}

fn qos_snapshot_order(left: &QosSnapshotRecord, right: &QosSnapshotRecord) -> std::cmp::Ordering {
    right
        .window_24h
        .score
        .cmp(&left.window_24h.score)
        .then_with(|| left.stream_identity_key.cmp(&right.stream_identity_key))
}

fn load_qos_snapshot(storage_dir: &str, stream_identity_key: &str) -> io::Result<Option<QosSnapshotRecord>> {
    QosSnapshotRepository::get_snapshot_read_only(Path::new(storage_dir), stream_identity_key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DisconnectReason, QosAggregationConfig, ResourceRetryConfig, ReverseProxyConfig};
    use crate::repository::{
        QosSnapshotDailyBucket, QosSnapshotRecord, QosSnapshotWindow,
    };
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
            event_type: EventType::Disconnect,
            event_ts_utc: 1,
            partition_day_utc: String::from("2026-03-22"),
            session_id: 1,
            source_addr: None,
            api_username: Some(String::from("alice")),
            provider_name: Some(provider_name.intern()),
            provider_username: None,
            input_name: Some("input".intern()),
            virtual_id: Some(1),
            item_type: Some("live".intern()),
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
        assert_eq!(summaries[0].provider_name.as_ref(), "acme");
        assert_eq!(summaries[0].session_count, 2);
        assert_eq!(summaries[0].total_bytes_sent, 400);
        assert_eq!(summaries[0].avg_session_duration_secs, Some(15));
        assert_eq!(summaries[0].avg_first_byte_latency_ms, Some(100));
        assert_eq!(summaries[0].disconnect_count, 2);
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
            item_type: "live".intern(),
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
        raw.insert("target_id".to_string(), "2".to_string());
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
        let mut cfg = crate::model::Config::default();
        cfg.storage_dir = "/var/lib/tuliprox".to_string();
        cfg.reverse_proxy = Some(ReverseProxyConfig {
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
        });

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

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::api::model::AppState;
use crate::repository::StreamHistoryFileReader;
use crate::utils::stream_history_viewer::{
    CompiledFilter, StreamHistoryQuery, TimeRange,
    discover_files, resolve_time_range,
};

#[derive(Deserialize)]
pub(crate) struct HistoryQueryParams {
    pub from: Option<String>,
    pub to: Option<String>,
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
    pub provider_name: String,
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

    // Run blocking file I/O on the blocking thread pool
    let result = tokio::task::spawn_blocking(move || {
        collect_records(&history_dir, &time_range, &filters)
    })
    .await;

    match result {
        Ok(Ok(records)) => axum::Json(records).into_response(),
        Ok(Err(e)) => {
            if e.kind() == io::ErrorKind::NotFound {
                // Return empty array when no files found, not an error
                axum::Json(Vec::<crate::repository::StreamHistoryRecord>::new()).into_response()
            } else {
                error_response(StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to read history: {e}"))
            }
        }
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, format!("History query task failed: {e}")),
    }
}

pub(crate) async fn stream_history_summary_query(
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

    let result = tokio::task::spawn_blocking(move || {
        collect_records(&history_dir, &time_range, &filters)
            .map(|records| aggregate_provider_summaries(records.as_slice()))
    })
    .await;

    match result {
        Ok(Ok(summaries)) => axum::Json(summaries).into_response(),
        Ok(Err(e)) => {
            if e.kind() == io::ErrorKind::NotFound {
                axum::Json(Vec::<ProviderSummary>::new()).into_response()
            } else {
                error_response(StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to summarize history: {e}"))
            }
        }
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, format!("History summary task failed: {e}")),
    }
}

fn collect_records(
    dir: &str,
    time_range: &TimeRange,
    filters: &CompiledFilter,
) -> io::Result<Vec<crate::repository::StreamHistoryRecord>> {
    let files = discover_files(Path::new(dir), time_range)?;
    let (range_start, range_end) = *time_range;

    let mut records = Vec::new();

    for file in &files {
        let iter: Box<dyn Iterator<Item = io::Result<crate::repository::StreamHistoryRecord>>> =
            if file.is_archive {
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

pub(crate) fn aggregate_provider_summaries(
    records: &[crate::repository::StreamHistoryRecord],
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

    let mut by_provider: std::collections::BTreeMap<String, Acc> = std::collections::BTreeMap::new();

    for record in records {
        let provider_name = record.provider_name.clone().unwrap_or_else(|| String::from("unknown"));
        let acc = by_provider.entry(provider_name).or_default();
        acc.session_count = acc.session_count.saturating_add(1);
        if matches!(record.event_type, crate::repository::EventType::Disconnect) {
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
            avg_session_duration_secs: if acc.duration_count > 0 {
                Some(acc.total_duration / acc.duration_count)
            } else {
                None
            },
            avg_first_byte_latency_ms: if acc.first_byte_count > 0 {
                Some(acc.total_first_byte_latency / acc.first_byte_count)
            } else {
                None
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::{DisconnectReason, EventType, StreamHistoryRecord};

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
            provider_name: Some(provider_name.to_string()),
            provider_username: None,
            virtual_id: Some(1),
            item_type: Some(String::from("live")),
            title: Some(String::from("Title")),
            group: None,
            country: None,
            user_agent: Some(String::from("VLC/3.0")),
            shared: Some(false),
            provider_id: Some(1),
            cluster: Some(String::from("live")),
            container: None,
            video_codec: None,
            audio_codec: None,
            resolution: None,
            connect_ts_utc: Some(1),
            disconnect_ts_utc: Some(2),
            session_duration: duration,
            bytes_sent,
            first_byte_latency_ms,
            provider_reconnect_count: None,
            disconnect_reason,
            previous_session_id: None,
            target_id: Some(1),
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
        assert_eq!(summaries[0].provider_name, "acme");
        assert_eq!(summaries[0].session_count, 2);
        assert_eq!(summaries[0].total_bytes_sent, 400);
        assert_eq!(summaries[0].avg_session_duration_secs, Some(15));
        assert_eq!(summaries[0].avg_first_byte_latency_ms, Some(100));
        assert_eq!(summaries[0].disconnect_count, 2);
    }
}

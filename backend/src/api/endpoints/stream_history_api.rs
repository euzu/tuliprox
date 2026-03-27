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

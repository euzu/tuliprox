use crate::{
    api::{model::AppState, tasks::next_schedule_run},
    model::{ConfigTarget, ScheduleConfig},
};
use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Local, TimeZone};
use cron::Schedule;
use shared::model::{ScheduleTaskType, TargetAnalyticsEntryDto, TargetAnalyticsResponseDto, TargetType};
use std::{str::FromStr, sync::Arc};
use tokio_util::io::ReaderStream;

const TARGET_ANALYTICS_READ_CONCURRENCY: usize = 8;

#[derive(Debug, Clone, Copy)]
pub(crate) struct ScheduleRun<'a> {
    pub next: Option<i64>,
    pub targets: Option<&'a [String]>,
}

pub(crate) fn compute_target_next_scheduled(schedule_next_runs: &[ScheduleRun<'_>], target_name: &str) -> Option<i64> {
    schedule_next_runs
        .iter()
        .filter(|run| match run.targets {
            None => true,
            Some(t_list) => t_list.is_empty() || t_list.iter().any(|t| t.eq_ignore_ascii_case(target_name)),
        })
        .filter_map(|run| run.next)
        .min()
}

pub(crate) fn parse_playlist_schedule_next_runs<'a, Z: TimeZone>(
    schedules: &'a [ScheduleConfig],
    now: &DateTime<Z>,
) -> Vec<ScheduleRun<'a>> {
    schedules
        .iter()
        .filter(|s| s.task_type == ScheduleTaskType::PlaylistUpdate)
        .filter_map(|s| match Schedule::from_str(&s.schedule) {
            Ok(sched) => {
                let next = next_schedule_run(&sched, now).map(|dt| dt.timestamp());
                Some(ScheduleRun { next, targets: s.targets.as_deref() })
            }
            Err(err) => {
                log::warn!("Invalid cron expression in playlist update schedule '{}': {err}", s.schedule);
                None
            }
        })
        .collect()
}

pub(super) async fn playlist_analytics(State(app_state): State<Arc<AppState>>) -> Json<TargetAnalyticsResponseDto> {
    let targets: Vec<Arc<ConfigTarget>> = {
        let sources = app_state.app_config.sources.load();
        sources.sources.iter().flat_map(|source| &source.targets).cloned().collect()
    };
    let config = app_state.app_config.config.load_full();

    let now = Local::now();
    let schedules = config.schedules.as_deref().unwrap_or_default();
    let schedule_next_runs = parse_playlist_schedule_next_runs(schedules, &now);

    let reads = futures::stream::iter(targets.into_iter().map(|target| {
        let config = Arc::clone(&config);
        async move {
            let storage_path = tuliprox_repository::get_target_storage_path(&config, &target.name);
            let analytics = if let Some(path) = storage_path {
                match tuliprox_repository::load_target_analytics(&path).await {
                    Ok(data) => data,
                    Err(err) => {
                        log::warn!("Failed to read target analytics for target '{}': {err}", target.name);
                        None
                    }
                }
            } else {
                None
            };
            (target, analytics)
        }
    }));
    let reads = {
        use futures::StreamExt as _;
        reads.buffered(TARGET_ANALYTICS_READ_CONCURRENCY)
    };
    let target_analytics_pairs: Vec<_> = futures::StreamExt::collect(reads).await;

    let mut entries = Vec::with_capacity(target_analytics_pairs.len());
    for (target, analytics) in target_analytics_pairs {
        let outputs: Vec<TargetType> = target.output.iter().map(TargetType::from).collect();
        // Disabled targets are skipped by scheduled runs.
        let next_scheduled =
            target.enabled.then(|| compute_target_next_scheduled(&schedule_next_runs, &target.name)).flatten();
        entries.push(TargetAnalyticsEntryDto {
            target_id: target.id,
            name: target.name.clone(),
            outputs,
            next_scheduled,
            analytics,
        });
    }

    Json(TargetAnalyticsResponseDto { targets: entries })
}

pub(super) async fn playlist_analytics_unmapped_csv(
    State(app_state): State<Arc<AppState>>,
    Path(target_id): Path<u16>,
) -> Response {
    let (target_name, storage_path) = {
        let sources = app_state.app_config.sources.load();
        let Some(target) = sources.get_target_by_id(target_id) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let config = app_state.app_config.config.load();
        let Some(storage_path) = tuliprox_repository::get_target_storage_path(&config, &target.name) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        (target.name.clone(), storage_path)
    };
    let csv_path = tuliprox_repository::get_target_analytics_unmapped_file_path(&storage_path);
    let Ok(file) = tokio::fs::File::open(&csv_path).await else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let mut sanitized = shared::utils::sanitize_filename_chars(&target_name, true);
    if sanitized.is_empty() {
        sanitized = format!("target-{target_id}");
    }
    let filename = format!("{sanitized}-unmapped.csv");

    let stream = ReaderStream::new(file);
    let body = axum::body::Body::from_stream(stream);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/csv; charset=utf-8")
        .header(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{filename}\""))
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

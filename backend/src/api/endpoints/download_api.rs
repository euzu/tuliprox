use crate::{
    api::endpoints::v1_api_playlist,
    api::model::{
        mutate, mutate_optional, AppState, ActiveProviderManager, ConnectionManager, DownloadControl, DownloadKind,
        DownloadQueue, DownloadState, DownloadWaitOutcome, EventManager, EventMessage, FileDownload,
        FileDownloadRequest, FileRecordingRequest, PersistedFileDownload, QueueMutationError,
        RecordingExecutionResult, recording_partial_path, run_recording,
    },
    messaging::send_message,
    model::{AppConfig, MessageContent, VideoDownloadConfig},
    utils::{async_file_writer, request, request::create_client, IO_BUFFER_SIZE},
};
use crate::api::model::recording_notification::LifecycleEvent;
use crate::api::model::recording_notification_adapter::{DispatchDecision, build_marker, decide, message_for};
use axum::response::IntoResponse;
use futures::stream::TryStreamExt;
use log::{debug, error, info, warn};
use serde::Deserialize;
use serde_json::json;
use shared::{
    error::to_io_error,
    model::{DownloadsDelta, DownloadsResponse, RecordingMetadata},
    utils::bytes_to_megabytes,
};
use std::{collections::HashMap, ops::Deref, pin::Pin, sync::Arc};
use tokio::{
    fs,
    io::{AsyncWrite, AsyncWriteExt},
    sync::{Notify, RwLock},
    time::{self, Duration, Instant, Sleep},
};
use tokio_util::sync::CancellationToken;

const DOWNLOAD_PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(5);
const DOWNLOAD_PROGRESS_LOG_BYTES: u64 = 16 * 1024 * 1024;
const DOWNLOAD_SNAPSHOT_UPDATE_INTERVAL: Duration = Duration::from_secs(2);
const DOWNLOAD_SNAPSHOT_UPDATE_BYTES: u64 = 4 * 1024 * 1024;
// Pause/cancel/restart are delivered immediately via `control_notify` while the
// worker is parked in the `select!`. This poll is only a fallback for the rare
// race where a control change fires while a chunk is being written (notify is not
// persisted), so it does not need to run on every chunk.
const DOWNLOAD_CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(200);
const RECORDING_PROGRESS_UPDATE_INTERVAL: Duration = Duration::from_secs(5);
type ProviderCapacities = Vec<(Arc<str>, usize, usize)>;

enum DownloadExecutionResult {
    Completed,
    Paused,
    Cancelled,
    Preempted,
    Retryable(String),
    Failed(String),
}

enum ProviderAcquireResult {
    Acquired(Option<crate::api::model::ProviderHandle>),
    Paused,
    Cancelled,
    Preempted,
}

fn recording_execution_download(app_config: &AppConfig, download: &FileDownload) -> Result<FileDownload, String> {
    let source = download
        .recording
        .as_ref()
        .and_then(|metadata| metadata.source.as_ref())
        .ok_or_else(|| "Recording source metadata missing".to_string())?;
    let virtual_id = source
        .virtual_id
        .parse::<u32>()
        .map_err(|_| "Recording source virtual id invalid".to_string())?;
    let url = v1_api_playlist::build_stable_recording_url(
        app_config,
        &source.target_id,
        &source.input_name,
        virtual_id,
        source.cluster,
    )
    .ok_or_else(|| "Recording execution URL unavailable".to_string())?;
    let mut execution_download = download.clone();
    execution_download.url = reqwest::Url::parse(&url).map_err(|_| "Recording execution URL invalid".to_string())?;
    Ok(execution_download)
}

fn classify_download_open_error(url: &reqwest::Url, err: &reqwest::Error) -> DownloadExecutionResult {
    if is_retryable_download_error(err) {
        DownloadExecutionResult::Retryable(format!("Error while opening url: {url} {err}"))
    } else {
        DownloadExecutionResult::Failed(format!("Error while opening url: {url} {err}"))
    }
}

fn classify_download_stream_io_error(file_path_str: &str, err: &std::io::Error) -> DownloadExecutionResult {
    if retryable_transport_error_message(&err.to_string()) {
        DownloadExecutionResult::Retryable(format!("Error while downloading file: {file_path_str} {err}"))
    } else {
        DownloadExecutionResult::Failed(format!("Error while downloading file: {file_path_str} {err}"))
    }
}

fn apply_download_retry_jitter(base_secs: u64, jitter_percent: u8) -> u64 {
    let jitter_percent = i64::from(jitter_percent.min(95));
    if jitter_percent == 0 {
        return base_secs.max(1);
    }
    let jitter_percent = fastrand::i64(-jitter_percent..=jitter_percent);
    let base_i64 = i64::try_from(base_secs.max(1)).unwrap_or(i64::MAX);
    let jitter_delta = base_i64.saturating_mul(jitter_percent).saturating_div(100);
    let jittered = base_i64.saturating_add(jitter_delta);
    u64::try_from(jittered.max(1)).unwrap_or(1)
}

#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn compute_download_retry_backoff_secs(attempts: u8, download_cfg: &VideoDownloadConfig) -> u64 {
    let exponent = i32::from(attempts.saturating_sub(1));
    let scaled_secs = (download_cfg.retry_backoff_initial_secs as f64) * download_cfg.retry_backoff_multiplier.powi(exponent);
    let clamped_secs = scaled_secs.clamp(
        download_cfg.retry_backoff_initial_secs as f64,
        download_cfg.retry_backoff_max_secs as f64,
    );
    let base_secs = clamped_secs.round() as u64;
    apply_download_retry_jitter(base_secs, download_cfg.retry_backoff_jitter_percent)
}

fn is_retryable_download_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

fn is_retryable_download_error(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect() || retryable_transport_error_message(&err.to_string())
}

fn retryable_transport_error_message(message: &str) -> bool {
    let msg = message.to_ascii_lowercase();
    msg.contains("timed out")
        || msg.contains("connection reset")
        || msg.contains("connection refused")
        || msg.contains("temporary failure")
        || msg.contains("temporarily unavailable")
        || msg.contains("network is unreachable")
        || msg.contains("dns")
        || msg.contains("name or service not known")
        || msg.contains("connection closed before message completed")
        || msg.contains("unexpected eof")
}

fn parse_content_range_total(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers.get("content-range").and_then(|v| {
        v.to_str().ok().and_then(|s| s.split('/').next_back().and_then(|total| total.parse::<u64>().ok()))
    })
}

fn compute_download_total_size(response: &reqwest::Response, existing_size: u64) -> Option<u64> {
    let is_resume = existing_size > 0 || response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    if is_resume {
        parse_content_range_total(response.headers()).or_else(|| {
            if response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
                response.content_length().map(|len| len.saturating_add(existing_size))
            } else {
                response.content_length()
            }
        })
    } else {
        response.content_length()
    }
}

fn background_download_should_wait(
    priority: i8,
    capacities: &[(Arc<str>, usize, usize)],
    download_cfg: &VideoDownloadConfig,
) -> bool {
    if priority <= 0 || capacities.is_empty() {
        return false;
    }

    let background_limit = usize::from(download_cfg.max_background_per_provider);
    let reserve_slots = usize::from(download_cfg.reserve_slots_for_users);

    let blocked_by_background_limit =
        background_limit > 0 && capacities.iter().all(|(_, current, _)| *current >= background_limit);
    let blocked_by_reserved_slots = reserve_slots > 0
        && capacities
            .iter()
            .all(|(_, current, max)| *max > 0 && current.saturating_add(reserve_slots) >= *max);

    blocked_by_background_limit || blocked_by_reserved_slots
}

fn capacities_have_free_slot(capacities: &[(Arc<str>, usize, usize)]) -> bool {
    capacities.iter().any(|(_, current, max)| *max == 0 || current < max)
}

#[cfg(test)]
fn recording_deadline_reached(download: &FileDownload, now_ts: i64) -> bool {
    download.kind == crate::api::model::DownloadKind::Recording
        && download
            .start_at
            .zip(download.duration_secs)
            .is_some_and(|(start_at, duration_secs)| now_ts >= start_at.saturating_add(i64::try_from(duration_secs).unwrap_or(90)))
}

#[cfg(test)]
async fn active_download_snapshot(active: &RwLock<Option<FileDownload>>) -> Option<FileDownload> { active.read().await.clone() }

async fn active_download_snapshot_for_worker(
    active: &RwLock<Option<FileDownload>>,
    worker_uuid: &str,
) -> Option<FileDownload> {
    active
        .read()
        .await
        .as_ref()
        .filter(|download| download.uuid == worker_uuid)
        .cloned()
}

async fn update_active_download_for_worker<F>(
    active: &RwLock<Option<FileDownload>>,
    worker_uuid: &str,
    update: F,
) -> bool
where
    F: FnOnce(&mut FileDownload) -> bool,
{
    let mut active = active.write().await;
    let Some(download) = active.as_mut().filter(|download| download.uuid == worker_uuid) else {
        return false;
    };
    update(download)
}

pub async fn download_queue_snapshot(download_queue: &DownloadQueue) -> DownloadsResponse {
    let (queue, active, finished) = download_queue.committed_download_snapshot().await;
    let queue = queue.iter().map(shared::model::FileDownloadDto::from).collect();
    let finished = finished.iter().map(shared::model::FileDownloadDto::from).collect();
    let active = active.as_ref().map(shared::model::FileDownloadDto::from).into_iter().collect();

    DownloadsResponse {
        queue,
        finished,
        active,
    }
}

async fn broadcast_download_queue_update(event_manager: &Arc<EventManager>, download_queue: &DownloadQueue) {
    if !event_manager.has_event_receivers() {
        return;
    }
    let (queue, active, finished) = download_queue.committed_download_snapshot().await;
    let queue = queue.iter().map(shared::model::FileDownloadDto::from).collect();
    let finished = finished.iter().map(shared::model::FileDownloadDto::from).collect();
    let _ = event_manager.send_event(EventMessage::DownloadsDeltaUpdate(DownloadsDelta::QueueReplaced { queue }));
    let _ = event_manager.send_event(EventMessage::DownloadsDeltaUpdate(DownloadsDelta::FinishedReplaced {
        finished,
    }));
    if let Some(download) = active.as_ref() {
        let _ = event_manager.send_event(EventMessage::DownloadsDeltaUpdate(DownloadsDelta::ActivePatched(
            shared::model::FileDownloadDto::from(download),
        )));
    } else {
        let _ = event_manager.send_event(EventMessage::DownloadsDeltaUpdate(DownloadsDelta::ActiveCleared));
    }
    let _ = event_manager.send_event(EventMessage::RecordingChanged);
}

async fn broadcast_worker_mutation(
    event_manager: &Arc<EventManager>,
    download_queue: &DownloadQueue,
    result: Result<bool, QueueMutationError>,
    action: &str,
) -> Result<bool, QueueMutationError> {
    match result {
        Ok(true) => {
            broadcast_download_queue_update(event_manager, download_queue).await;
            Ok(true)
        }
        Ok(false) => Ok(false),
        Err(err) => Err(QueueMutationError::new(format!("{action}: {err}"))),
    }
}

async fn broadcast_required_worker_mutation(
    event_manager: &Arc<EventManager>,
    download_queue: &DownloadQueue,
    result: Result<bool, QueueMutationError>,
    action: &str,
) -> Result<(), QueueMutationError> {
    if broadcast_worker_mutation(event_manager, download_queue, result, action).await? {
        Ok(())
    } else {
        Err(QueueMutationError::new(format!("{action}: active task changed")))
    }
}

async fn broadcast_active_download_delta(event_manager: &Arc<EventManager>, active: &RwLock<Option<FileDownload>>) {
    if !event_manager.has_event_receivers() {
        return;
    }
    if let Some(download) = active.read().await.as_ref() {
        let _ = event_manager.send_event(EventMessage::DownloadsDeltaUpdate(DownloadsDelta::ActivePatched(
            shared::model::FileDownloadDto::from(download),
        )));
    }
}

async fn refresh_recording_progress(
    active: &RwLock<Option<FileDownload>>,
    worker_uuid: &str,
    file_path: &std::path::Path,
    event_manager: &Arc<EventManager>,
) {
    let current_size = tokio::fs::metadata(file_path).await.map_or(0, |metadata| metadata.len());
    let changed = update_active_download_for_worker(active, worker_uuid, |download| {
        if download.kind == DownloadKind::Recording && download.size != current_size {
            download.size = current_size;
            true
        } else {
            false
        }
    })
    .await;
    if changed {
        broadcast_active_download_delta(event_manager, active).await;
    }
}

#[allow(clippy::too_many_lines)]
async fn download_file(
    active: Arc<RwLock<Option<FileDownload>>>,
    file_download: FileDownload,
    client: &reqwest::Client,
    control_signal: Arc<RwLock<DownloadControl>>,
    control_notify: Arc<Notify>,
    provider_cancel_token: Option<CancellationToken>,
    event_manager: Option<&Arc<EventManager>>,
) -> DownloadExecutionResult {
    let worker_uuid = file_download.uuid.as_str();
    let url = file_download.url.clone();
    let file_path = file_download.file_path.clone();
        // Check for existing partial file for resume
        let existing_size = tokio::fs::metadata(&file_path).await.map_or(0, |metadata| metadata.len());

        let mut request_builder = client.get(url.clone());
        if existing_size > 0 {
            request_builder = request_builder.header("Range", format!("bytes={existing_size}-"));
        }

        if let Some(result) = handle_download_control_without_writer(current_download_control(&control_signal)) {
            return result;
        }

        let send_request = request_builder.send();
        tokio::pin!(send_request);
        let response_result = loop {
            if let Some(cancel_token) = provider_cancel_token.as_ref() {
                tokio::select! {
                    biased;
                    () = cancel_token.cancelled() => return DownloadExecutionResult::Preempted,
                    () = control_notify.notified() => {
                        if let Some(result) =
                            handle_download_control_without_writer(*control_signal.read().await)
                        {
                            return result;
                        }
                    }
                    response = &mut send_request => break response,
                }
            } else {
                tokio::select! {
                    biased;
                    () = control_notify.notified() => {
                        if let Some(result) =
                            handle_download_control_without_writer(*control_signal.read().await)
                        {
                            return result;
                        }
                    }
                    response = &mut send_request => break response,
                }
            }
        };

        match response_result {
            Ok(response) => {
                let status = response.status();
                if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
                    if is_retryable_download_status(status) {
                        return DownloadExecutionResult::Retryable(format!(
                            "Download request failed for {url} with transient HTTP {status}"
                        ));
                    }
                    return DownloadExecutionResult::Failed(format!("Download request failed for {url} with HTTP {status}"));
                }
                let is_resume = status == reqwest::StatusCode::PARTIAL_CONTENT;

                let total_size = compute_download_total_size(&response, existing_size);

                if let Some(total) = total_size {
                    let changed = update_active_download_for_worker(&active, worker_uuid, |download| {
                        download.total_size = Some(total);
                        true
                    })
                    .await;
                    if changed {
                        if let Some(event_manager) = event_manager {
                            broadcast_active_download_delta(event_manager, &active).await;
                        }
                    }
                }

                match fs::create_dir_all(&file_download.file_dir).await {
                    Ok(()) => {
                        let mut open_options = tokio::fs::OpenOptions::new();
                        let file_mode = if existing_size > 0 && is_resume {
                            open_options.append(true)
                        } else {
                            open_options.write(true).create(true).truncate(true)
                        };

                        if let Some(file_path_str) = file_path.to_str() {
                            info!("{} {}", if is_resume { "Resuming" } else { "Downloading" }, file_path_str);
                            match file_mode.open(&file_path).await {
                                Ok(file) => {
                                    let mut buf_writer = async_file_writer(file);
                                    let mut downloaded: u64 = if is_resume { existing_size } else { 0 };
                                    let mut stream = response.bytes_stream();
                                    let mut write_counter = 0;
                                    let mut saw_first_chunk = existing_size > 0;
                                    let mut last_progress_log_at = Instant::now();
                                    let mut last_progress_logged_bytes = downloaded;
                                    let mut last_snapshot_update_at = Instant::now();
                                    let mut last_snapshot_update_bytes = downloaded;
                                    let mut control_poll = time::interval(DOWNLOAD_CONTROL_POLL_INTERVAL);
                                    control_poll.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
                                    control_poll.tick().await;
                                    let deadline_at = recording_deadline_instant(&file_download);
                                    let mut deadline_sleep = deadline_at.map(|deadline| {
                                        Box::pin(time::sleep_until(deadline)) as Pin<Box<Sleep>>
                                    });

                                    loop {
                                        if deadline_at.is_some_and(|deadline| Instant::now() >= deadline) {
                                            if let Err(err) = buf_writer.flush().await {
                                                return DownloadExecutionResult::Failed(err.to_string());
                                            }
                                            if let Err(err) = buf_writer.shutdown().await {
                                                return DownloadExecutionResult::Failed(err.to_string());
                                            }
                                            return DownloadExecutionResult::Completed;
                                        }

                                        let next_item = if let (Some(cancel_token), Some(deadline_sleep)) =
                                            (provider_cancel_token.as_ref(), deadline_sleep.as_mut())
                                        {
                                            tokio::select! {
                                                biased;
                                                () = cancel_token.cancelled() => return DownloadExecutionResult::Preempted,
                                                () = control_notify.notified() => {
                                                    if let Some(result) = handle_download_control(
                                                        &active,
                                                        *control_signal.read().await,
                                                        &mut buf_writer,
                                                    ).await {
                                                        return result;
                                                    }
                                                    continue;
                                                }
                                                _ = control_poll.tick() => {
                                                    if let Some(result) = handle_download_control(
                                                        &active,
                                                        current_download_control(&control_signal),
                                                        &mut buf_writer,
                                                    ).await {
                                                        return result;
                                                    }
                                                    continue;
                                                }
                                                () = deadline_sleep.as_mut() => return DownloadExecutionResult::Completed,
                                                next_item = stream.try_next() => next_item.map_err(to_io_error),
                                            }
                                        } else if let Some(cancel_token) = provider_cancel_token.as_ref() {
                                            tokio::select! {
                                                biased;
                                                () = cancel_token.cancelled() => return DownloadExecutionResult::Preempted,
                                                () = control_notify.notified() => {
                                                    if let Some(result) = handle_download_control(
                                                        &active,
                                                        *control_signal.read().await,
                                                        &mut buf_writer,
                                                    ).await {
                                                        return result;
                                                    }
                                                    continue;
                                                }
                                                _ = control_poll.tick() => {
                                                    if let Some(result) = handle_download_control(
                                                        &active,
                                                        current_download_control(&control_signal),
                                                        &mut buf_writer,
                                                    ).await {
                                                        return result;
                                                    }
                                                    continue;
                                                }
                                                next_item = stream.try_next() => next_item.map_err(to_io_error),
                                            }
                                        } else if let Some(deadline_sleep) = deadline_sleep.as_mut() {
                                            tokio::select! {
                                                biased;
                                                () = control_notify.notified() => {
                                                    if let Some(result) = handle_download_control(
                                                        &active,
                                                        *control_signal.read().await,
                                                        &mut buf_writer,
                                                    ).await {
                                                        return result;
                                                    }
                                                    continue;
                                                }
                                                _ = control_poll.tick() => {
                                                    if let Some(result) = handle_download_control(
                                                        &active,
                                                        current_download_control(&control_signal),
                                                        &mut buf_writer,
                                                    ).await {
                                                        return result;
                                                    }
                                                    continue;
                                                }
                                                () = deadline_sleep.as_mut() => return DownloadExecutionResult::Completed,
                                                next_item = stream.try_next() => next_item.map_err(to_io_error),
                                            }
                                        } else {
                                            tokio::select! {
                                                biased;
                                                () = control_notify.notified() => {
                                                    if let Some(result) = handle_download_control(
                                                        &active,
                                                        *control_signal.read().await,
                                                        &mut buf_writer,
                                                    ).await {
                                                        return result;
                                                    }
                                                    continue;
                                                }
                                                _ = control_poll.tick() => {
                                                    if let Some(result) = handle_download_control(
                                                        &active,
                                                        current_download_control(&control_signal),
                                                        &mut buf_writer,
                                                    ).await {
                                                        return result;
                                                    }
                                                    continue;
                                                }
                                                next_item = stream.try_next() => next_item.map_err(to_io_error),
                                            }
                                        };

                                        match next_item {
                                            Ok(item) => {
                                                if let Some(chunk) = item {
                                                    match buf_writer.write_all(&chunk).await {
                                                        Ok(()) => {
                                                            write_counter += chunk.len();
                                                            if write_counter >= IO_BUFFER_SIZE {
                                                                if let Err(err) = buf_writer.flush().await {
                                                                    return DownloadExecutionResult::Failed(err.to_string());
                                                                }
                                                                write_counter = 0;
                                                            }

                                                            downloaded += chunk.len() as u64;
                                                            if saw_first_chunk {
                                                                let now = Instant::now();
                                                                let should_log_progress = now.duration_since(last_progress_log_at)
                                                                    >= DOWNLOAD_PROGRESS_LOG_INTERVAL
                                                                    || downloaded.saturating_sub(last_progress_logged_bytes)
                                                                    >= DOWNLOAD_PROGRESS_LOG_BYTES;
                                                                if should_log_progress {
                                                                    match total_size {
                                                                        Some(total) if total > 0 => {
                                                                            let percent = downloaded
                                                                                .saturating_mul(100)
                                                                                .checked_div(total)
                                                                                .unwrap_or(0)
                                                                                .min(100);
                                                                            debug!(
                                                                                "Download progress for {file_path_str}: {}MB / {}MB ({}%)",
                                                                                bytes_to_megabytes(downloaded),
                                                                                bytes_to_megabytes(total),
                                                                                percent
                                                                            );
                                                                        }
                                                                        _ => {
                                                                            debug!(
                                                                                "Download progress for {file_path_str}: {}MB received",
                                                                                bytes_to_megabytes(downloaded)
                                                                            );
                                                                        }
                                                                    }
                                                                    last_progress_log_at = now;
                                                                    last_progress_logged_bytes = downloaded;
                                                                if let Some(event_manager) = event_manager {
                                                                    broadcast_active_download_delta(event_manager, &active).await;
                                                                }
                                                            }
                                                        } else {
                                                                saw_first_chunk = true;
                                                                info!(
                                                                    "Receiving download data for {file_path_str}: {}MB received",
                                                                    bytes_to_megabytes(downloaded)
                                                                );
                                                                last_progress_log_at = Instant::now();
                                                                last_progress_logged_bytes = downloaded;
                                                                if let Some(event_manager) = event_manager {
                                                                    broadcast_active_download_delta(event_manager, &active).await;
                                                                }
                                                            }
                                                            let should_update_snapshot = downloaded
                                                                .saturating_sub(last_snapshot_update_bytes)
                                                                >= DOWNLOAD_SNAPSHOT_UPDATE_BYTES
                                                                || Instant::now().duration_since(last_snapshot_update_at)
                                                                    >= DOWNLOAD_SNAPSHOT_UPDATE_INTERVAL;
                                                            if should_update_snapshot {
                                                                update_active_download_for_worker(
                                                                    &active,
                                                                    worker_uuid,
                                                                    |download| {
                                                                        download.size = downloaded;
                                                                        true
                                                                    },
                                                                )
                                                                .await;
                                                                last_snapshot_update_at = Instant::now();
                                                                last_snapshot_update_bytes = downloaded;
                                                            }
                                                        }
                                                        Err(err) => {
                                                            return DownloadExecutionResult::Failed(format!(
                                                                "Error while writing to file: {file_path_str} {err}"
                                                            ));
                                                        }
                                                    }
                                                } else {
                                                    let megabytes = bytes_to_megabytes(downloaded);
                                                    info!("Downloaded {file_path_str}, filesize: {megabytes}MB");
                                                    update_active_download_for_worker(
                                                        &active,
                                                        worker_uuid,
                                                        |download| {
                                                            download.size = downloaded;
                                                            true
                                                        },
                                                    )
                                                    .await;
                                                    if let Err(err) = buf_writer.flush().await {
                                                        return DownloadExecutionResult::Failed(err.to_string());
                                                    }
                                                    if let Err(err) = buf_writer.shutdown().await {
                                                        return DownloadExecutionResult::Failed(err.to_string());
                                                    }
                                                    return DownloadExecutionResult::Completed;
                                                }
                                            }
                                            Err(err) => return classify_download_stream_io_error(file_path_str, &err),
                                        }
                                    }
                                }
                                Err(err) => DownloadExecutionResult::Failed(format!("Error while opening file: {file_path_str} {err}")),
                            }
                        } else {
                            DownloadExecutionResult::Failed("Error file-download file-path unknown".to_string())
                        }
                    }
                    Err(err) => DownloadExecutionResult::Failed(format!(
                        "Error while creating directory for file: {} {}",
                        file_download.file_dir.to_str().unwrap_or("?"),
                        err
                    )),
                }
            }
            Err(err) => classify_download_open_error(&url, &err),
        }
}

fn current_download_control(control_signal: &RwLock<DownloadControl>) -> DownloadControl {
    control_signal.try_read().map_or(DownloadControl::None, |control| *control)
}

fn should_exit_worker_after_preempt(control: DownloadControl) -> bool { control == DownloadControl::Restart }

async fn handle_download_control<W>(
    _active: &Arc<RwLock<Option<FileDownload>>>,
    control: DownloadControl,
    buf_writer: &mut W,
) -> Option<DownloadExecutionResult>
where
    W: AsyncWrite + Unpin,
{
    match control {
        DownloadControl::Pause => {
            if let Err(err) = buf_writer.flush().await {
                return Some(DownloadExecutionResult::Failed(err.to_string()));
            }
            if let Err(err) = buf_writer.shutdown().await {
                return Some(DownloadExecutionResult::Failed(err.to_string()));
            }
            Some(DownloadExecutionResult::Paused)
        }
        DownloadControl::Cancel => {
            if let Err(err) = buf_writer.flush().await {
                return Some(DownloadExecutionResult::Failed(err.to_string()));
            }
            if let Err(err) = buf_writer.shutdown().await {
                return Some(DownloadExecutionResult::Failed(err.to_string()));
            }
            Some(DownloadExecutionResult::Cancelled)
        }
        DownloadControl::Restart => {
            if let Err(err) = buf_writer.flush().await {
                return Some(DownloadExecutionResult::Failed(err.to_string()));
            }
            if let Err(err) = buf_writer.shutdown().await {
                return Some(DownloadExecutionResult::Failed(err.to_string()));
            }
            Some(DownloadExecutionResult::Preempted)
        }
        DownloadControl::None => None,
    }
}

fn handle_download_control_without_writer(control: DownloadControl) -> Option<DownloadExecutionResult> {
    match control {
        DownloadControl::Pause => Some(DownloadExecutionResult::Paused),
        DownloadControl::Cancel => Some(DownloadExecutionResult::Cancelled),
        DownloadControl::Restart => Some(DownloadExecutionResult::Preempted),
        DownloadControl::None => None,
    }
}

fn recording_deadline_instant(download: &FileDownload) -> Option<Instant> {
    if download.kind != DownloadKind::Recording {
        return None;
    }
    let (start_at, duration_secs) = download.start_at.zip(download.duration_secs)?;
    let deadline_ts = start_at.saturating_add(i64::try_from(duration_secs).unwrap_or(i64::MAX));
    let now_ts = chrono::Utc::now().timestamp();
    if now_ts >= deadline_ts {
        return Some(Instant::now());
    }
    let remaining_secs = u64::try_from(deadline_ts.saturating_sub(now_ts)).ok()?;
    Some(Instant::now() + Duration::from_secs(remaining_secs))
}

async fn set_active_download_state(
    download_queue: &DownloadQueue,
    uuid: &str,
    state: DownloadState,
    error: Option<String>,
    paused: bool,
) -> Result<bool, QueueMutationError> {
    Ok(mutate_optional(download_queue, |candidate| {
        let Some(download) = candidate.active.as_mut().filter(|active| active.uuid == uuid) else {
            return Ok(None);
        };
        if download.state == state && download.error == error && download.paused == paused && !download.finished {
            return Ok(None);
        }
        download.state = state;
        download.error = error;
        download.paused = paused;
        download.finished = false;
        Ok(Some(true))
    })
    .await?
    .unwrap_or(false))
}

async fn commit_acquired_download(
    download_queue: &DownloadQueue,
    uuid: &str,
) -> Result<Option<RecordingNotificationPlan>, QueueMutationError> {
    mutate_optional(download_queue, |candidate| {
        let Some(active) = candidate.active.as_mut().filter(|active| active.uuid == uuid) else {
            return Ok(None);
        };
        active.state = DownloadState::Downloading;
        active.error = None;
        active.paused = false;
        active.finished = false;
        let notification = mark_recording_metadata_notification(
            active.recording.as_mut(),
            LifecycleEvent::Started,
            None,
        );
        Ok(Some(notification))
    })
    .await
}

#[cfg(test)]
fn mark_recording_notification(
    download: &mut FileDownload,
    event: LifecycleEvent,
    failure_reason: Option<String>,
) -> RecordingNotificationPlan {
    mark_recording_metadata_notification(download.recording.as_mut(), event, failure_reason)
}

fn mark_recording_metadata_notification(
    recording: Option<&mut RecordingMetadata>,
    event: LifecycleEvent,
    failure_reason: Option<String>,
) -> RecordingNotificationPlan {
    let Some(meta) = recording else {
        return RecordingNotificationPlan::empty();
    };
    let is_admin_owner = match &meta.owner {
        shared::model::recording::RecordingOwner::LegacyAdmin => true,
        shared::model::recording::RecordingOwner::User(owner) => owner.is_builtin_admin(),
    };
    match decide(meta, event, chrono::Utc::now().timestamp(), is_admin_owner, failure_reason) {
        DispatchDecision::PersistAndDeliver { payload, kind, attempted_at } => {
            meta.notification_markers.push(build_marker(kind.clone(), attempted_at));
            RecordingNotificationPlan {
                message: Some(MessageContent::RecordingLifecycle(message_for(event, &payload))),
                #[cfg(test)]
                marker_kind: Some(kind),
            }
        }
        DispatchDecision::AlreadyDelivered { .. } | DispatchDecision::Suppressed { .. } => RecordingNotificationPlan::empty(),
    }
}

/// Result of `mark_recording_notification`. `marker_kind` is set when a
/// marker was added to the in-memory recording; the caller uses it to roll
/// the marker back if the subsequent persist fails.
struct RecordingNotificationPlan {
    message: Option<MessageContent>,
    #[cfg(test)]
    marker_kind: Option<shared::model::recording::NotificationMarkerKind>,
}

impl RecordingNotificationPlan {
    fn empty() -> Self {
        Self {
            message: None,
            #[cfg(test)]
            marker_kind: None,
        }
    }
}

#[cfg(test)]
fn rollback_last_recording_marker(fd: &mut FileDownload, kind: &shared::model::recording::NotificationMarkerKind) {
    if let Some(meta) = fd.recording.as_mut() {
        if let Some(idx) = meta.notification_markers.iter().rposition(|marker| marker.kind == *kind) {
            meta.notification_markers.remove(idx);
        }
    }
}

fn spawn_recording_notification_after_persist(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    plan: RecordingNotificationPlan,
    persisted: bool,
) {
    if persisted {
        if let Some(message) = plan.message {
            let app_config = Arc::clone(app_config);
            let client = client.clone();
            tokio::spawn(async move {
                send_message(&app_config, &client, message).await;
            });
        }
    }
}

async fn requeue_active_download_for_retry(
    download_queue: &DownloadQueue,
    uuid: &str,
    promote: bool,
) -> Result<bool, QueueMutationError> {
    Ok(mutate_optional(download_queue, |candidate| {
        let Some(mut download) = candidate.active.take() else {
            return Ok(None);
        };
        if download.uuid != uuid {
            candidate.active = Some(download);
            return Ok(None);
        }
        download.finished = false;
        download.paused = false;
        download.error = None;
        download.state = DownloadState::Queued;
        download.next_retry_at = None;
        candidate.queue.insert(0, download);
        if promote {
            candidate.active = Some(candidate.queue.remove(0));
        }
        Ok(Some(true))
    })
    .await?
    .unwrap_or(false))
}

async fn requeue_active_download_for_capacity_wait(
    download_queue: &DownloadQueue,
    uuid: &str,
    reason: &str,
    promote: bool,
    consumed_control: Option<DownloadControl>,
) -> Result<bool, QueueMutationError> {
    let mutation = |candidate: &mut crate::api::model::PersistedDownloadQueue| {
        let Some(mut download) = candidate.active.take() else {
            return Ok(None);
        };
        if download.uuid != uuid {
            candidate.active = Some(download);
            return Ok(None);
        }
        download.finished = false;
        download.paused = false;
        download.error = Some(reason.to_string());
        download.state = DownloadState::WaitingForCapacity;
        download.next_retry_at = None;
        candidate.queue.insert(0, download);
        if promote {
            candidate.active = Some(candidate.queue.remove(0));
        }
        Ok(Some(true))
    };
    let result = if let Some(control) = consumed_control {
        download_queue
            .mutate_optional_and_clear_control(control, mutation)
            .await?
    } else {
        mutate_optional(download_queue, mutation).await?
    };
    Ok(result.unwrap_or(false))
}

async fn promote_next_download(download_queue: &DownloadQueue) -> Result<Option<(String, String)>, QueueMutationError> {
    mutate_optional(download_queue, |candidate| {
        if candidate.active.is_some() || candidate.queue.is_empty() {
            return Ok(None);
        }
        let next = candidate.queue.remove(0);
        let promoted = (next.uuid.clone(), next.filename.clone());
        candidate.active = Some(next);
        Ok(Some(promoted))
    })
    .await
}

async fn finish_active_and_promote<F>(
    download_queue: &DownloadQueue,
    uuid: &str,
    finish: F,
) -> Result<Option<RecordingNotificationPlan>, QueueMutationError>
where
    F: FnOnce(&mut PersistedFileDownload) -> RecordingNotificationPlan,
{
    mutate_optional(download_queue, |candidate| {
        let Some(mut active) = candidate.active.take() else {
            return Ok(None);
        };
        if active.uuid != uuid {
            candidate.active = Some(active);
            return Ok(None);
        }
        let notification = finish(&mut active);
        candidate.finished.push(active);
        if !candidate.queue.is_empty() {
            candidate.active = Some(candidate.queue.remove(0));
        }
        Ok(Some(notification))
    })
    .await
}

async fn cancel_active_and_promote(
    download_queue: &DownloadQueue,
    uuid: &str,
) -> Result<bool, QueueMutationError> {
    Ok(download_queue
        .mutate_optional_and_clear_control(DownloadControl::Cancel, |candidate| {
        let Some(mut active) = candidate.active.take() else {
            return Ok(None);
        };
        if active.uuid != uuid {
            candidate.active = Some(active);
            return Ok(None);
        }
        active.finished = true;
        active.paused = false;
        active.next_retry_at = None;
        active.error.get_or_insert_with(|| "Cancelled by user".to_string());
        active.state = DownloadState::Cancelled;
        candidate.finished.push(active);
        if !candidate.queue.is_empty() {
            candidate.active = Some(candidate.queue.remove(0));
        }
        Ok(Some(true))
    })
    .await?
    .unwrap_or(false))
}

enum RetryCommit {
    Waiting { delay_secs: u64, attempts: u8 },
    Failed(RecordingNotificationPlan),
}

async fn prepare_active_retry(
    download_queue: &DownloadQueue,
    uuid: &str,
    download_cfg: &VideoDownloadConfig,
) -> Result<Option<RetryCommit>, QueueMutationError> {
    mutate_optional(download_queue, |candidate| {
        let Some(active) = candidate.active.as_mut().filter(|active| active.uuid == uuid) else {
            return Ok(None);
        };
        active.retry_attempts = active.retry_attempts.saturating_add(1);
        let attempts = active.retry_attempts;
        if attempts > download_cfg.retry_max_attempts {
            let Some(mut failed) = candidate.active.take() else {
                return Ok(None);
            };
            let error = format!("Retry limit reached after {} attempts", download_cfg.retry_max_attempts);
            failed.finished = true;
            failed.paused = false;
            failed.next_retry_at = None;
            failed.state = DownloadState::Failed;
            failed.error = Some(error.clone());
            let notification = mark_recording_metadata_notification(
                failed.recording.as_mut(),
                LifecycleEvent::Failed,
                Some(error),
            );
            candidate.finished.push(failed);
            if !candidate.queue.is_empty() {
                candidate.active = Some(candidate.queue.remove(0));
            }
            return Ok(Some(RetryCommit::Failed(notification)));
        }

        let delay_secs = compute_download_retry_backoff_secs(attempts, download_cfg);
        let next_retry_at = chrono::Utc::now()
            .timestamp()
            .saturating_add(i64::try_from(delay_secs).unwrap_or(i64::MAX));
        active.next_retry_at = Some(next_retry_at);
        active.state = DownloadState::RetryWaiting;
        active.paused = false;
        active.finished = false;
        active.error = Some(format!(
            "Retrying after transient failure in {delay_secs}s (attempt {attempts}/{})",
            download_cfg.retry_max_attempts
        ));
        Ok(Some(RetryCommit::Waiting { delay_secs, attempts }))
    })
    .await
}

const DOWNLOAD_PREEMPTED_REASON: &str = "Preempted by higher-priority foreground stream";
const RECORDING_PREEMPTED_REASON: &str =
    "Recording preempted by higher-priority foreground stream; waiting to resume within the remaining window";

fn preemption_reason_for(download: &FileDownload) -> &'static str {
    match download.kind {
        DownloadKind::Download => DOWNLOAD_PREEMPTED_REASON,
        DownloadKind::Recording => RECORDING_PREEMPTED_REASON,
    }
}

#[allow(clippy::too_many_lines)]
pub(in crate::api) async fn ensure_download_worker_running(
    cfg: &AppConfig,
    download_cfg: &VideoDownloadConfig,
    download_queue: &Arc<DownloadQueue>,
    event_manager: &Arc<EventManager>,
    active_provider: &Arc<ActiveProviderManager>,
    connection_manager: &Arc<ConnectionManager>,
) -> Result<(), String> {
    let mut worker_running = download_queue.worker_running.write().await;
    if *worker_running {
        debug!("Download worker already running");
        return Ok(());
    }
    *worker_running = true;
    drop(worker_running);

    match promote_next_download(download_queue).await {
        Ok(Some((uuid, filename))) => {
            debug!("Promoting queued download {uuid} ({filename}) to active");
            broadcast_download_queue_update(event_manager, download_queue).await;
        }
        Ok(None) => {}
        Err(err) => {
            *download_queue.worker_running.write().await = false;
            return Err(err.to_string());
        }
    }

    if download_queue.active.read().await.is_some() {
        let config = cfg.config.load();
        let disabled_headers = cfg.get_disabled_headers();
        let headers = request::get_request_headers(
            Some(&download_cfg.headers),
            None,
            disabled_headers.as_ref(),
            config.default_user_agent.as_deref(),
        );
        let dq = Arc::clone(download_queue);
        let control_signal = Arc::clone(&dq.control_signal);
        let control_notify = Arc::clone(&dq.control_notify);
        let event_manager = Arc::clone(event_manager);
        let active_provider = Arc::clone(active_provider);
        let connection_manager = Arc::clone(connection_manager);
        let download_cfg = download_cfg.clone();
        let app_config = Arc::new(cfg.clone());

        if let Ok(client) = create_client(cfg).default_headers(headers).build() {
                if let Some(active) = dq.active.read().await.as_ref() {
                    info!(
                        "Starting download worker for active download {} ({})",
                        active.uuid, active.filename
                    );
                }
                tokio::spawn(async move {
                    'worker: loop {
                        if dq.active.read().await.deref().is_some() {
                            if let Some(download) = dq.active.read().await.as_ref() {
                                if download.paused {
                                    break;
                                }
                            }
                            let Some(worker_uuid) = dq.active.read().await.as_ref().map(|download| download.uuid.clone()) else {
                                break;
                            };

                            // Acquire a provider connection slot for this download.
                            // If the provider is at capacity, wait in the priority queue until signalled.
                            // Never proceeds without a slot when input_name is set — account bans otherwise.
                            let provider_acquire_result = {
                                let (input_name, priority) = {
                                    let active = dq.active.read().await;
                                    active.as_ref().map_or((None, 0i8), |dl| (dl.input_name.clone(), dl.priority))
                                };
                                if let Some(input_name) = input_name {
                                    loop {
                                        let capacities = active_provider.provider_capacities_for_input(&input_name).await;
                                        if background_download_should_wait(priority, &capacities, &download_cfg) {
                                            if let Err(err) = broadcast_worker_mutation(
                                                &event_manager,
                                                &dq,
                                                set_active_download_state(
                                                    &dq,
                                                    &worker_uuid,
                                                    DownloadState::WaitingForCapacity,
                                                    None,
                                                    false,
                                                )
                                                .await,
                                                "waiting-for-capacity state",
                                            )
                                            .await
                                            {
                                                error!("Download worker commit failed: {err}");
                                                break 'worker;
                                            }
                                            match dq
                                                .slot_waiters
                                                .wait(
                                                    Some(Arc::clone(&input_name)),
                                                    priority,
                                                    control_signal.as_ref(),
                                                    control_notify.as_ref(),
                                                )
                                                .await
                                            {
                                                DownloadWaitOutcome::Signalled => {}
                                                DownloadWaitOutcome::Paused => break ProviderAcquireResult::Paused,
                                                DownloadWaitOutcome::Cancelled => break ProviderAcquireResult::Cancelled,
                                                DownloadWaitOutcome::Restarted => break ProviderAcquireResult::Preempted,
                                            }
                                            continue;
                                        }
                                        if let Some(handle) = active_provider.acquire_connection_for_download(&input_name, priority).await {
                                            break ProviderAcquireResult::Acquired(Some(handle));
                                        }
                                        if *control_signal.read().await == DownloadControl::Cancel {
                                            break ProviderAcquireResult::Cancelled;
                                        }
                                        if *control_signal.read().await == DownloadControl::Pause {
                                            break ProviderAcquireResult::Paused;
                                        }
                                        if *control_signal.read().await == DownloadControl::Restart {
                                            break ProviderAcquireResult::Preempted;
                                        }
                                        if let Err(err) = broadcast_worker_mutation(
                                            &event_manager,
                                            &dq,
                                            set_active_download_state(
                                                &dq,
                                                &worker_uuid,
                                                DownloadState::WaitingForCapacity,
                                                None,
                                                false,
                                            )
                                            .await,
                                            "waiting-for-capacity state",
                                        )
                                        .await
                                        {
                                            error!("Download worker commit failed: {err}");
                                            break 'worker;
                                        }
                                        // Wait for highest-priority signal — no sleep, no polling.
                                        match dq
                                            .slot_waiters
                                            .wait(
                                                Some(Arc::clone(&input_name)),
                                                priority,
                                                control_signal.as_ref(),
                                                control_notify.as_ref(),
                                            )
                                            .await
                                        {
                                            DownloadWaitOutcome::Signalled => {}
                                            DownloadWaitOutcome::Paused => break ProviderAcquireResult::Paused,
                                            DownloadWaitOutcome::Cancelled => break ProviderAcquireResult::Cancelled,
                                            DownloadWaitOutcome::Restarted => break ProviderAcquireResult::Preempted,
                                        }
                                    }
                                } else {
                                    ProviderAcquireResult::Acquired(None)
                                }
                            };

                            let provider_handle = match provider_acquire_result {
                                ProviderAcquireResult::Acquired(handle) => {
                                    match commit_acquired_download(&dq, &worker_uuid).await {
                                        Ok(Some(notification)) => {
                                            spawn_recording_notification_after_persist(
                                                &app_config,
                                                &client,
                                                notification,
                                                true,
                                            );
                                            broadcast_download_queue_update(&event_manager, &dq).await;
                                        }
                                        Ok(None) => {
                                            connection_manager.release_provider_handle(handle).await;
                                            error!("Download worker active task changed after provider acquire");
                                            break 'worker;
                                        }
                                        Err(err) => {
                                            connection_manager.release_provider_handle(handle).await;
                                            error!("Download worker commit failed after provider acquire: {err}");
                                            break 'worker;
                                        }
                                    }
                                    handle
                                }
                                ProviderAcquireResult::Paused => {
                                    break;
                                }
                                ProviderAcquireResult::Cancelled => {
                                    if let Err(err) = broadcast_required_worker_mutation(
                                        &event_manager,
                                        &dq,
                                        cancel_active_and_promote(&dq, &worker_uuid).await,
                                        "provider-wait cancellation",
                                    )
                                    .await
                                    {
                                        error!("Download worker commit failed: {err}");
                                        break 'worker;
                                    }
                                    continue;
                                }
                                ProviderAcquireResult::Preempted => {
                                    if let Err(err) = broadcast_required_worker_mutation(
                                        &event_manager,
                                        &dq,
                                        requeue_active_download_for_capacity_wait(
                                            &dq,
                                            &worker_uuid,
                                            "Reloading download service configuration",
                                            true,
                                            Some(DownloadControl::Restart),
                                        )
                                        .await,
                                        "configuration-reload requeue",
                                    )
                                    .await
                                    {
                                        error!("Download worker commit failed: {err}");
                                        break 'worker;
                                    }
                                    break;
                                }
                            };

                            let execution_result = {
                                let Some(download) = active_download_snapshot_for_worker(&dq.active, &worker_uuid).await
                                else {
                                    connection_manager.release_provider_handle(provider_handle).await;
                                    break 'worker;
                                };
                                match download.kind.clone() {
                                    DownloadKind::Download => download_file(
                                        Arc::clone(&dq.active),
                                        download,
                                        &client,
                                        Arc::clone(&control_signal),
                                        Arc::clone(&control_notify),
                                        provider_handle.as_ref().and_then(|handle| handle.cancel_token.clone()),
                                        Some(&event_manager),
                                    )
                                    .await,
                                    DownloadKind::Recording => 'recording_execution: {
                                        let execution_download = match recording_execution_download(&app_config, &download) {
                                            Ok(execution_download) => execution_download,
                                            Err(err) => break 'recording_execution DownloadExecutionResult::Failed(err),
                                        };
                                        let progress_path = recording_partial_path(&execution_download.file_path);
                                        let mut recording_future = Box::pin(run_recording(
                                            &execution_download,
                                            &control_signal,
                                            &control_notify,
                                            provider_handle.as_ref().and_then(|handle| handle.cancel_token.as_ref()),
                                        ));
                                        let mut progress_tick = time::interval(RECORDING_PROGRESS_UPDATE_INTERVAL);
                                        progress_tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

                                        let result = loop {
                                            tokio::select! {
                                                recording_result = &mut recording_future => break recording_result,
                                                _ = progress_tick.tick() => {
                                                    if event_manager.has_event_receivers() {
                                                        refresh_recording_progress(
                                                            &dq.active,
                                                            &worker_uuid,
                                                            &progress_path,
                                                            &event_manager,
                                                        )
                                                        .await;
                                                    }
                                                }
                                            }
                                        };
                                        if event_manager.has_event_receivers() {
                                            refresh_recording_progress(
                                                &dq.active,
                                                &worker_uuid,
                                                &progress_path,
                                                &event_manager,
                                            )
                                            .await;
                                        }

                                        match result {
                                            RecordingExecutionResult::Completed => DownloadExecutionResult::Completed,
                                            RecordingExecutionResult::Paused => DownloadExecutionResult::Paused,
                                            RecordingExecutionResult::Cancelled => DownloadExecutionResult::Cancelled,
                                            RecordingExecutionResult::Preempted => DownloadExecutionResult::Preempted,
                                            RecordingExecutionResult::Retryable(err) => DownloadExecutionResult::Retryable(err),
                                            RecordingExecutionResult::Failed(err) => DownloadExecutionResult::Failed(err),
                                        }
                                    }
                                }
                            };

                            match execution_result
                            {
                                DownloadExecutionResult::Completed => {
                                    connection_manager.release_provider_handle(provider_handle).await;
                                    let measured_bytes = {
                                        let active = dq.active.read().await;
                                        match active.as_ref() {
                                            Some(fd) => tokio::fs::metadata(&fd.file_path)
                                                .await
                                                .map_or(fd.size, |metadata| metadata.len()),
                                            None => 0,
                                        }
                                    };
                                    let committed = finish_active_and_promote(&dq, &worker_uuid, |fd| {
                                        fd.finished = true;
                                        fd.paused = false;
                                        fd.state = DownloadState::Completed;
                                        fd.size = measured_bytes;
                                        fd.error = None;
                                        fd.next_retry_at = None;
                                        if let Some(meta) = fd.recording.as_mut() {
                                            meta.measured_bytes = measured_bytes;
                                            meta.reserved_bytes = 0;
                                            meta.completed_at = Some(chrono::Utc::now().timestamp());
                                            meta.partial_relative_path = None;
                                        }
                                        mark_recording_metadata_notification(
                                            fd.recording.as_mut(),
                                            LifecycleEvent::Completed,
                                            None,
                                        )
                                    })
                                    .await;
                                    match committed {
                                        Ok(Some(notification)) => {
                                            spawn_recording_notification_after_persist(
                                                &app_config,
                                                &client,
                                                notification,
                                                true,
                                            );
                                            broadcast_download_queue_update(&event_manager, &dq).await;
                                        }
                                        Ok(None) => {}
                                        Err(err) => {
                                            error!("Failed to persist completed download state: {err}");
                                            break 'worker;
                                        }
                                    }
                                }
                                DownloadExecutionResult::Paused => {
                                    connection_manager.release_provider_handle(provider_handle).await;
                                    if let Err(err) = broadcast_required_worker_mutation(
                                        &event_manager,
                                        &dq,
                                        set_active_download_state(&dq, &worker_uuid, DownloadState::Paused, None, true).await,
                                        "paused state",
                                    )
                                    .await
                                    {
                                        error!("Download worker commit failed: {err}");
                                        break 'worker;
                                    }
                                    break;
                                }
                                DownloadExecutionResult::Cancelled => {
                                    connection_manager.release_provider_handle(provider_handle).await;
                                    if let Err(err) = broadcast_required_worker_mutation(
                                        &event_manager,
                                        &dq,
                                        cancel_active_and_promote(&dq, &worker_uuid).await,
                                        "cancelled state",
                                    )
                                    .await
                                    {
                                        error!("Download worker commit failed: {err}");
                                        break 'worker;
                                    }
                                }
                                DownloadExecutionResult::Preempted => {
                                    connection_manager.release_provider_handle(provider_handle).await;
                                    let control = *control_signal.read().await;
                                    match control {
                                        DownloadControl::Restart => warn!("Active transfer is restarting to apply updated download service configuration"),
                                        _ => warn!("Active transfer was preempted by a higher-priority stream"),
                                    }
                                    let reason = {
                                        let active = dq.active.read().await;
                                        if control == DownloadControl::Restart {
                                            "Reloading download service configuration"
                                        } else {
                                            active.as_ref().map_or(DOWNLOAD_PREEMPTED_REASON, preemption_reason_for)
                                        }
                                    };
                                    if let Err(err) = broadcast_required_worker_mutation(
                                        &event_manager,
                                        &dq,
                                        requeue_active_download_for_capacity_wait(
                                            &dq,
                                            &worker_uuid,
                                            reason,
                                            true,
                                            (control == DownloadControl::Restart).then_some(DownloadControl::Restart),
                                        )
                                        .await,
                                        "preempted requeue",
                                    )
                                    .await
                                    {
                                        error!("Download worker commit failed: {err}");
                                        break 'worker;
                                    }
                                    if should_exit_worker_after_preempt(control) {
                                        break;
                                    }
                                }
                                DownloadExecutionResult::Retryable(_err) => {
                                    connection_manager.release_provider_handle(provider_handle).await;
                                    warn!("Retrying active download after transient failure");
                                    let retry_commit = prepare_active_retry(&dq, &worker_uuid, &download_cfg).await;
                                    let retry_delay_secs = match retry_commit {
                                        Ok(Some(RetryCommit::Waiting { delay_secs, attempts })) => {
                                            debug!("Download retry attempt {attempts} scheduled in {delay_secs}s");
                                            broadcast_download_queue_update(&event_manager, &dq).await;
                                            delay_secs
                                        }
                                        Ok(Some(RetryCommit::Failed(notification))) => {
                                        spawn_recording_notification_after_persist(
                                            &app_config,
                                            &client,
                                            notification,
                                                true,
                                        );
                                            broadcast_download_queue_update(&event_manager, &dq).await;
                                            if dq.active.read().await.is_some() {
                                                continue;
                                            }
                                            break;
                                        }
                                        Ok(None) => break,
                                        Err(err) => {
                                            error!("Failed to persist retry state: {err}");
                                            break;
                                        }
                                    };
                                    let mut retry_sleep = Box::pin(time::sleep(Duration::from_secs(retry_delay_secs)));
                                    let retry_wait_outcome = loop {
                                        tokio::select! {
                                            () = &mut retry_sleep => break DownloadExecutionResult::Retryable(String::new()),
                                            () = control_notify.notified() => {
                                                match *control_signal.read().await {
                                                    DownloadControl::Pause => break DownloadExecutionResult::Paused,
                                                    DownloadControl::Cancel => break DownloadExecutionResult::Cancelled,
                                                    DownloadControl::Restart => break DownloadExecutionResult::Preempted,
                                                    DownloadControl::None => {}
                                                }
                                            }
                                        }
                                    };

                                    match retry_wait_outcome {
                                        DownloadExecutionResult::Retryable(_) => {
                                            if let Err(err) = broadcast_required_worker_mutation(
                                                &event_manager,
                                                &dq,
                                                requeue_active_download_for_retry(&dq, &worker_uuid, true).await,
                                                "retry requeue",
                                            )
                                            .await
                                            {
                                                error!("Download worker commit failed: {err}");
                                                break 'worker;
                                            }
                                        }
                                        DownloadExecutionResult::Paused => {
                                            if let Err(err) = broadcast_required_worker_mutation(
                                                &event_manager,
                                                &dq,
                                                set_active_download_state(
                                                    &dq,
                                                    &worker_uuid,
                                                    DownloadState::Paused,
                                                    None,
                                                    true,
                                                )
                                                .await,
                                                "paused retry state",
                                            )
                                            .await
                                            {
                                                error!("Download worker commit failed: {err}");
                                                break 'worker;
                                            }
                                            break;
                                        }
                                        DownloadExecutionResult::Cancelled => {
                                            if let Err(err) = broadcast_required_worker_mutation(
                                                &event_manager,
                                                &dq,
                                                cancel_active_and_promote(&dq, &worker_uuid).await,
                                                "cancelled retry state",
                                            )
                                            .await
                                            {
                                                error!("Download worker commit failed: {err}");
                                                break 'worker;
                                            }
                                        }
                                        DownloadExecutionResult::Completed
                                        | DownloadExecutionResult::Failed(_) => {}
                                        DownloadExecutionResult::Preempted => {
                                            if let Err(err) = broadcast_required_worker_mutation(
                                                &event_manager,
                                                &dq,
                                                requeue_active_download_for_capacity_wait(
                                                    &dq,
                                                    &worker_uuid,
                                                    "Reloading download service configuration",
                                                    true,
                                                    Some(DownloadControl::Restart),
                                                )
                                                .await,
                                                "configuration-reload retry requeue",
                                            )
                                            .await
                                            {
                                                error!("Download worker commit failed: {err}");
                                                break 'worker;
                                            }
                                            break;
                                        }
                                    }
                                }
                                DownloadExecutionResult::Failed(err) => {
                                    connection_manager.release_provider_handle(provider_handle).await;
                                    warn!("Download failed permanently: {err}");
                                    let committed = finish_active_and_promote(&dq, &worker_uuid, |fd| {
                                        fd.finished = true;
                                        fd.paused = false;
                                        fd.next_retry_at = None;
                                        fd.error = Some(err.clone());
                                        fd.state = DownloadState::Failed;
                                        mark_recording_metadata_notification(
                                            fd.recording.as_mut(),
                                            LifecycleEvent::Failed,
                                            Some(err),
                                        )
                                    })
                                    .await;
                                    match committed {
                                        Ok(Some(notification)) => {
                                            spawn_recording_notification_after_persist(
                                                &app_config,
                                                &client,
                                                notification,
                                                true,
                                            );
                                            broadcast_download_queue_update(&event_manager, &dq).await;
                                        }
                                        Ok(None) => {}
                                        Err(commit_err) => {
                                            error!("Failed to persist failed download state: {commit_err}");
                                            break 'worker;
                                        }
                                    }
                                }
                            }
                        } else {
                            break;
                        }
                    }
                    *dq.worker_running.write().await = false;
                });
        } else {
            *download_queue.worker_running.write().await = false;
            return Err("Failed to build http client".to_string());
        }
    } else {
        *download_queue.worker_running.write().await = false;
    }
    Ok(())
}

pub(in crate::api) fn start_download_scheduler(
    app_config: Arc<AppConfig>,
    download_cfg: VideoDownloadConfig,
    download_queue: Arc<DownloadQueue>,
    event_manager: Arc<EventManager>,
    active_provider: Arc<ActiveProviderManager>,
    connection_manager: Arc<ConnectionManager>,
    cancel_token: CancellationToken,
) {
    // Bridge task: whenever any provider connection is released, wake only the
    // highest-priority download waiter. This prevents lower-priority downloads
    // from racing ahead of higher-priority ones.
    let capacity_notify = connection_manager.capacity_notified();
    let slot_waiters = Arc::clone(&download_queue.slot_waiters);
    let bridge_active_provider = Arc::clone(&active_provider);
    let bridge_download_cfg = download_cfg.clone();
    let bridge_cancel_token = cancel_token.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = bridge_cancel_token.cancelled() => break,
                () = capacity_notify.notified() => {}
            }

            let mut capacities_by_input: HashMap<Arc<str>, ProviderCapacities> = HashMap::new();
            let mut ready_waiter = None;

            let mut waiters = slot_waiters.snapshots().await;
            waiters.sort_by_key(|waiter| waiter.priority);

            for waiter in waiters {
                let Some(input_name) = waiter.input_name.as_ref() else {
                    ready_waiter = Some(waiter.id);
                    break;
                };
                let capacities = if let Some(capacities) = capacities_by_input.get(input_name) {
                    capacities.clone()
                } else {
                    let capacities = bridge_active_provider.provider_capacities_for_input(input_name).await;
                    capacities_by_input.insert(Arc::clone(input_name), capacities.clone());
                    capacities
                };

                if !capacities_have_free_slot(&capacities) {
                    continue;
                }
                if background_download_should_wait(waiter.priority, &capacities, &bridge_download_cfg) {
                    continue;
                }
                ready_waiter = Some(waiter.id);
                break;
            }

            if let Some(waiter_id) = ready_waiter {
                let _ = slot_waiters.signal_waiter(waiter_id).await;
            }
        }
    });

    let scheduler_download_cfg = download_cfg;
    let scheduler_cancel_token = cancel_token;
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                () = scheduler_cancel_token.cancelled() => break,
                _ = interval.tick() => {}
            }

            if download_queue.promote_due_scheduled_now().await == 0 {
                continue;
            }

            broadcast_download_queue_update(&event_manager, &download_queue).await;

            let _ = ensure_download_worker_running(
                &app_config,
                &scheduler_download_cfg,
                &download_queue,
                &event_manager,
                &active_provider,
                &connection_manager,
            )
            .await;
        }
    });
}

pub(in crate::api) fn spawn_download_services(app_state: &AppState, cancel_token: &CancellationToken) {
    let config = app_state.app_config.config.load();
    let Some(download_cfg) = config.video.as_ref().and_then(|video| video.download.as_ref()).cloned() else {
        return;
    };

    start_download_scheduler(
        Arc::clone(&app_state.app_config),
        download_cfg.clone(),
        Arc::clone(&app_state.downloads),
        Arc::clone(&app_state.event_manager),
        Arc::clone(&app_state.active_provider),
        Arc::clone(&app_state.connection_manager),
        cancel_token.clone(),
    );
}

pub(in crate::api) async fn resume_download_worker_if_needed(
    app_state: &AppState,
    download_cfg: &VideoDownloadConfig,
) -> Result<(), String> {
    if app_state.downloads.queue.lock().await.is_empty() && app_state.downloads.active.read().await.is_none() {
        return Ok(());
    }

    ensure_download_worker_running(
        &app_state.app_config,
        download_cfg,
        &app_state.downloads,
        &app_state.event_manager,
        &app_state.active_provider,
        &app_state.connection_manager,
    )
    .await
}

pub async fn queue_download_file(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(req): axum::extract::Json<FileDownloadRequest>,
) -> impl axum::response::IntoResponse + Send {
    let app_config = &*app_state.app_config;

    let config = app_config.config.load();
    if let Some(video_cfg) = config.video.as_ref() {
        if let Some(download_cfg) = video_cfg.download.as_ref() {
            if download_cfg.directory.is_empty() {
                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(json!({"error": "Server config missing video.download.directory configuration"})),
                )
                    .into_response();
            }
            let input_name = req.input_name.map(|s| std::sync::Arc::from(s.as_str()));
            let priority = req.priority.unwrap_or(download_cfg.download_priority);
            match FileDownload::new(req.url.as_str(), req.filename.as_str(), download_cfg, input_name, priority) {
                Some(file_download) => {
                    if let Some(existing) = app_state.downloads.find_duplicate(&file_download).await {
                        info!(
                            "Skipping duplicate download request for {} (matched existing task {})",
                            file_download.url, existing.uuid
                        );
                        return axum::Json(shared::model::FileDownloadDto::from(&existing)).into_response();
                    }
                    info!(
                        "Queueing download {} ({}) from {}",
                        file_download.uuid, file_download.filename, file_download.url
                    );
                    if let Err(err) = mutate(&app_state.downloads, |candidate| {
                        candidate.queue.push(DownloadQueue::to_persisted(&file_download));
                        Ok(())
                    })
                    .await
                    {
                        return (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            axum::Json(json!({"error": err.message()})),
                        )
                            .into_response();
                    }
                    if app_state.downloads.active.read().await.is_none() {
                        match ensure_download_worker_running(
                            &app_state.app_config,
                            download_cfg,
                            &app_state.downloads,
                            &app_state.event_manager,
                            &app_state.active_provider,
                            &app_state.connection_manager,
                        )
                        .await
                        {
                            Ok(()) => {}
                            Err(err) => {
                                return (
                                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                    axum::Json(json!({"error": err})),
                                )
                                    .into_response()
                            }
                        }
                    }
                    broadcast_download_queue_update(&app_state.event_manager, &app_state.downloads).await;
                    axum::Json(shared::model::FileDownloadDto::from(&file_download)).into_response()
                }
                None => (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid Arguments"})))
                    .into_response(),
            }
        } else {
            (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "Server config missing video.download configuration"})),
            )
                .into_response()
        }
    } else {
        (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Server config missing video configuration"})))
            .into_response()
    }
}

pub async fn download_file_info(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl axum::response::IntoResponse + Send {
    axum::Json(download_queue_snapshot(&app_state.downloads).await)
}

pub async fn queue_recording_file(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(req): axum::extract::Json<FileRecordingRequest>,
) -> impl axum::response::IntoResponse + Send {
    let app_config = &*app_state.app_config;
    let config = app_config.config.load();

    if req.duration_secs == 0 {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "duration_secs must be greater than zero"})),
        )
            .into_response();
    }

    if let Some(video_cfg) = config.video.as_ref() {
        if let Some(download_cfg) = video_cfg.download.as_ref() {
            if download_cfg.directory.is_empty() {
                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(json!({"error": "Server config missing video.download.directory configuration"})),
                )
                    .into_response();
            }
            let input_name = req.input_name.map(|s| std::sync::Arc::from(s.as_str()));
            let priority = req.priority.unwrap_or(download_cfg.recording_priority);
            match FileDownload::new_recording(req.url.as_str(), req.filename.as_str(), download_cfg, req.start_at, req.duration_secs, input_name, priority) {
                Some(recording) => {
                    if let Some(existing) = app_state.downloads.find_duplicate(&recording).await {
                        info!(
                            "Skipping duplicate recording request for {} at {} (matched existing task {})",
                            recording.url,
                            recording.start_at.unwrap_or_default(),
                            existing.uuid
                        );
                        return axum::Json(shared::model::FileDownloadDto::from(&existing)).into_response();
                    }
                    if let Err(err) = mutate(&app_state.downloads, |candidate| {
                        candidate.scheduled.push(DownloadQueue::to_persisted(&recording));
                        Ok(())
                    })
                    .await
                    {
                        return (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            axum::Json(json!({"error": err.message()})),
                        )
                            .into_response();
                    }
                    broadcast_download_queue_update(&app_state.event_manager, &app_state.downloads).await;
                    axum::Json(shared::model::FileDownloadDto::from(&recording)).into_response()
                }
                None => (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid Arguments"})))
                    .into_response(),
            }
        } else {
            (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "Server config missing video.download configuration"})),
            )
                .into_response()
        }
    } else {
        (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Server config missing video configuration"})))
            .into_response()
    }
}

#[derive(Deserialize)]
pub(in crate::api) struct DownloadActionRequest {
    uuid: String,
}

pub async fn pause_download(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(req): axum::extract::Json<DownloadActionRequest>,
) -> impl axum::response::IntoResponse + Send {
    match app_state.downloads.pause_active(&req.uuid).await {
        Ok(true) => {
            broadcast_download_queue_update(&app_state.event_manager, &app_state.downloads).await;
            axum::Json(json!({"success": true})).into_response()
        }
        Ok(false) => axum::Json(json!({"success": false})).into_response(),
        Err(err) => {
            error!("Failed to persist paused download state: {err}");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": err.message()})),
            )
                .into_response()
        }
    }
}

pub async fn resume_download(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(req): axum::extract::Json<DownloadActionRequest>,
) -> impl axum::response::IntoResponse + Send {
    match app_state.downloads.resume_active(&req.uuid).await {
        Ok(true) => {
            let download_cfg = app_state
                .app_config
                .config
                .load()
                .video
                .as_ref()
                .and_then(|video| video.download.as_ref())
                .cloned();
            if let Some(download_cfg) = download_cfg {
                let app_state = Arc::clone(&app_state);
                tokio::spawn(async move {
                    for _ in 0..50 {
                        if !*app_state.downloads.worker_running.read().await {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    if let Err(err) = ensure_download_worker_running(
                        &app_state.app_config,
                        &download_cfg,
                        &app_state.downloads,
                        &app_state.event_manager,
                        &app_state.active_provider,
                        &app_state.connection_manager,
                    )
                    .await
                    {
                        error!("Failed to restart resumed download worker: {err}");
                    }
                });
            }
            broadcast_download_queue_update(&app_state.event_manager, &app_state.downloads).await;
            axum::Json(json!({"success": true})).into_response()
        }
        Ok(false) => axum::Json(json!({"success": false})).into_response(),
        Err(err) => {
            error!("Failed to persist resumed download state: {err}");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": err.message()})),
            )
                .into_response()
        }
    }
}

pub async fn cancel_download(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(req): axum::extract::Json<DownloadActionRequest>,
) -> impl axum::response::IntoResponse + Send {
    match app_state.downloads.cancel_requested(&req.uuid).await {
        Ok(Some(was_paused)) => {
            if was_paused {
                let config = app_state.app_config.config.load();
                if let Some(download_cfg) = config.video.as_ref().and_then(|video| video.download.as_ref()) {
                    if let Err(err) = ensure_download_worker_running(
                        &app_state.app_config,
                        download_cfg,
                        &app_state.downloads,
                        &app_state.event_manager,
                        &app_state.active_provider,
                        &app_state.connection_manager,
                    )
                    .await
                    {
                        error!("Failed to start download worker after cancelling paused task: {err}");
                    }
                }
            }
            broadcast_download_queue_update(&app_state.event_manager, &app_state.downloads).await;
            axum::Json(json!({"success": true})).into_response()
        }
        Ok(None) => {
            match app_state.downloads.remove_from_queue(&req.uuid).await {
                Ok(true) => {
                    broadcast_download_queue_update(&app_state.event_manager, &app_state.downloads).await;
                    axum::Json(json!({"success": true})).into_response()
                }
                Ok(false) => axum::Json(json!({"success": false})).into_response(),
                Err(err) => (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(json!({"error": err.message()})),
                )
                    .into_response(),
            }
        }
        Err(err) => {
            error!("Failed to persist cancelled download state: {err}");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": err.message()})),
            )
                .into_response()
        }
    }
}

pub async fn remove_download(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(req): axum::extract::Json<DownloadActionRequest>,
) -> impl axum::response::IntoResponse + Send {
    match app_state.downloads.remove(&req.uuid).await {
        Ok(removed) => {
            if removed {
                broadcast_download_queue_update(&app_state.event_manager, &app_state.downloads).await;
            }
            axum::Json(json!({"success": removed})).into_response()
        }
        Err(err) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": err.message()})),
        )
            .into_response(),
    }
}

pub async fn retry_download(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(req): axum::extract::Json<DownloadActionRequest>,
) -> impl axum::response::IntoResponse + Send {
    let retried = match app_state.downloads.retry_finished(&req.uuid).await {
        Ok(retried) => retried,
        Err(err) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": err.message()})),
            )
                .into_response();
        }
    };
    if retried {
        // Start the queue if not running
        let app_config = &app_state.app_config;
        let config = app_config.config.load();
        if let Some(video_cfg) = config.video.as_ref() {
            if let Some(download_cfg) = video_cfg.download.as_ref() {
                if app_state.downloads.active.read().await.is_none() {
                    let _ = ensure_download_worker_running(
                        app_config,
                        download_cfg,
                        &app_state.downloads,
                        &app_state.event_manager,
                        &app_state.active_provider,
                        &app_state.connection_manager,
                    )
                    .await;
                }
            }
        }
    }
    broadcast_download_queue_update(&app_state.event_manager, &app_state.downloads).await;
    axum::Json(json!({"success": retried})).into_response()
}

#[cfg(test)]
mod tests {
    use super::{
        active_download_snapshot, active_download_snapshot_for_worker, broadcast_download_queue_update,
        broadcast_required_worker_mutation, broadcast_worker_mutation, cancel_active_and_promote, cancel_download,
        commit_acquired_download, finish_active_and_promote,
        mark_recording_notification,
        parse_content_range_total, pause_download, preemption_reason_for, recording_deadline_reached,
        recording_execution_download,
        refresh_recording_progress, requeue_active_download_for_capacity_wait,
        requeue_active_download_for_retry, resume_download, rollback_last_recording_marker,
        retryable_transport_error_message, set_active_download_state, should_exit_worker_after_preempt,
        DownloadActionRequest,
        DOWNLOAD_PREEMPTED_REASON, RECORDING_PREEMPTED_REASON,
    };
    use crate::{
        api::model::{
            ActiveProviderManager, ActiveUserManager, AppState, CancelTokens, ConnectionManager, DownloadControl,
            DownloadKind, DownloadQueue, DownloadState, EventManager, EventMessage, FileDownload, MetadataUpdateManager,
            PlaylistStorageState, SharedStreamManager, UpdateGuard,
        },
        api::model::recording_notification::LifecycleEvent,
        model::{
            ApiProxyConfig, ApiProxyServerInfo, AppConfig, Config, ConfigInput, MediaToolCapabilities,
            MessageContent, ProcessTargets, SourcesConfig,
        },
        utils::{FileLockManager, GeoIp},
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use axum::response::IntoResponse;
    use reqwest::header::{HeaderMap, HeaderValue};
    use shared::{
        model::{
            ConfigPaths, InputFetchMethod, InputType, RecordingMetadata, RecordingOwner, RecordingSource,
            RecordingVisibility, UserId,
        },
        utils::Internable,
    };
    use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
    use tokio::sync::mpsc;
    use tokio::sync::RwLock;

    fn make_download(kind: DownloadKind, state: DownloadState, start_at: Option<i64>, duration_secs: Option<u64>) -> FileDownload {
        FileDownload {
            uuid: "id".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/file.ts"),
            filename: "file.ts".to_string(),
            url: reqwest::Url::parse("https://example.com/file.ts").expect("valid url"),
            finished: false,
            size: 128,
            total_size: Some(1024),
            paused: false,
            error: Some("transient".to_string()),
            state,
            start_at,
            duration_secs,
            kind,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: None,
        }
    }

    fn attach_recording(download: &mut FileDownload, owner: RecordingOwner, visibility: RecordingVisibility) {
        let mut metadata = RecordingMetadata::new(
            owner,
            visibility,
            RecordingSource::new("1", "42", "input-a"),
            1_000,
            1_060,
            0,
            0,
        );
        metadata.program_title = Some("Programme".to_string());
        metadata.channel_name = Some("Channel".to_string());
        metadata.relative_path = Some("Channel/Programme.ts".to_string());
        download.recording = Some(metadata);
    }

    #[test]
    fn recording_notification_marker_is_at_most_once() {
        let mut download = make_download(DownloadKind::Recording, DownloadState::Completed, Some(1_000), Some(60));
        attach_recording(&mut download, RecordingOwner::LegacyAdmin, RecordingVisibility::Shared);

        let first = mark_recording_notification(&mut download, LifecycleEvent::Completed, None);
        let duplicate = mark_recording_notification(&mut download, LifecycleEvent::Completed, None);

        assert!(matches!(first.message, Some(MessageContent::RecordingLifecycle(_))));
        assert!(duplicate.message.is_none());
        assert_eq!(
            download
                .recording
                .as_ref()
                .map_or(0, |metadata| metadata.notification_markers.len()),
            1
        );
    }

    #[test]
    fn private_user_recording_notification_is_suppressed() {
        let mut download = make_download(DownloadKind::Recording, DownloadState::Completed, Some(1_000), Some(60));
        attach_recording(
            &mut download,
            RecordingOwner::User(UserId::from("web:alice")),
            RecordingVisibility::Private,
        );

        let message = mark_recording_notification(&mut download, LifecycleEvent::Completed, None);

        assert!(message.message.is_none());
        assert_eq!(
            download
                .recording
                .as_ref()
                .map_or(0, |metadata| metadata.notification_markers.len()),
            0
        );
    }

    #[test]
    fn rollback_last_recording_marker_removes_most_recent_matching_kind() {
        let mut download = make_download(DownloadKind::Recording, DownloadState::Completed, Some(1_000), Some(60));
        attach_recording(&mut download, RecordingOwner::LegacyAdmin, RecordingVisibility::Shared);

        // Two distinct marker kinds end up in the same metadata after a
        // successful Completed followed by a Failed on the same task — this
        // mirrors what would happen in production across two persist rounds.
        let first = mark_recording_notification(&mut download, LifecycleEvent::Completed, None);
        let second = mark_recording_notification(&mut download, LifecycleEvent::Failed, Some("ffmpeg exited".to_string()));
        assert!(first.marker_kind.is_some());
        assert!(second.marker_kind.is_some());

        let kind = second.marker_kind.unwrap();
        rollback_last_recording_marker(&mut download, &kind);

        let markers = &download.recording.as_ref().unwrap().notification_markers;
        assert_eq!(markers.len(), 1, "only the Completed marker should remain");
        assert!(matches!(
            markers[0].kind,
            shared::model::recording::NotificationMarkerKind::Completed
        ));
    }

    #[test]
    fn recording_deadline_uses_start_plus_duration() {
        let recording = make_download(DownloadKind::Recording, DownloadState::Downloading, Some(1_000), Some(60));
        let normal = make_download(DownloadKind::Download, DownloadState::Downloading, Some(1_000), Some(60));

        assert!(!recording_deadline_reached(&recording, 1_059));
        assert!(recording_deadline_reached(&recording, 1_060));
        assert!(!recording_deadline_reached(&normal, 1_060));
    }

    #[test]
    fn recording_execution_requires_metadata_source() {
        let recording = make_download(DownloadKind::Recording, DownloadState::Downloading, Some(1_000), Some(60));

        let result = recording_execution_download(&create_test_app_config(), &recording);

        assert_eq!(result.as_ref().err().map(String::as_str), Some("Recording source metadata missing"));
    }

    #[test]
    fn recording_execution_uses_fresh_token_without_mutating_persisted_descriptor() {
        let mut recording = make_download(DownloadKind::Recording, DownloadState::Downloading, Some(1_000), Some(60));
        recording.url = reqwest::Url::parse(
            "tuliprox-recording://source?target_name=stable-target&input_name=provider_1&virtual_id=42&cluster=live",
        )
        .expect("valid descriptor");
        attach_recording(&mut recording, RecordingOwner::LegacyAdmin, RecordingVisibility::Private);
        let source = recording
            .recording
            .as_mut()
            .and_then(|metadata| metadata.source.as_mut())
            .expect("recording source");
        source.target_id = "stable-target".to_string();
        source.virtual_id = "42".to_string();
        source.input_name = "provider_1".to_string();
        let persisted_before = DownloadQueue::to_persisted(&recording);
        let app_config = create_test_app_config();

        let execution = recording_execution_download(&app_config, &recording).expect("execution download");
        let token = execution
            .url
            .path_segments()
            .and_then(|segments| segments.collect::<Vec<_>>().get(4).copied())
            .expect("route token");

        assert!(crate::auth::verify_access_token(token, &app_config.access_token_secret));
        assert_eq!(recording.url.as_str(), persisted_before.url);
        assert_eq!(DownloadQueue::to_persisted(&recording).url, persisted_before.url);
        assert_ne!(execution.url, recording.url);
    }

    #[tokio::test]
    async fn retry_requeues_active_download_at_front_in_one_commit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_file = dir.path().join("downloads_state.json");
        let queue = DownloadQueue::new_with_state_file(Some(state_file.clone()));
        let queued = make_download(DownloadKind::Download, DownloadState::Queued, None, None);
        let active = make_download(DownloadKind::Download, DownloadState::Downloading, None, None);

        queue.queue.lock().await.push_back(queued);
        *queue.active.write().await = Some(active);

        requeue_active_download_for_retry(&queue, "id", false).await.expect("requeue retry");

        assert!(queue.active.read().await.is_none());
        let queued_items = queue.queue.lock().await.iter().cloned().collect::<Vec<_>>();
        assert_eq!(queued_items.len(), 2);
        assert_eq!(queued_items[0].state, DownloadState::Queued);
        assert_eq!(queued_items[0].size, 128);
        assert!(queued_items[0].error.is_none());
        assert_eq!(queue.revision.load(std::sync::atomic::Ordering::SeqCst), 1);
        let persisted: crate::api::model::PersistedDownloadQueue =
            serde_json::from_slice(&std::fs::read(state_file).expect("read state")).expect("parse state");
        assert_eq!(persisted.revision, shared::model::QueueRevision(1));
    }

    #[tokio::test]
    async fn preempted_active_download_requeues_to_capacity_wait_with_partial_progress() {
        let queue = DownloadQueue::new();
        let mut active = make_download(DownloadKind::Download, DownloadState::Downloading, None, None);
        active.size = 512;
        active.total_size = Some(2048);
        *queue.active.write().await = Some(active);

        requeue_active_download_for_capacity_wait(&queue, "id", DOWNLOAD_PREEMPTED_REASON, false, None)
            .await
            .expect("requeue capacity wait");

        assert!(queue.active.read().await.is_none());
        let queued_items = queue.queue.lock().await.iter().cloned().collect::<Vec<_>>();
        assert_eq!(queued_items.len(), 1);
        assert_eq!(queued_items[0].state, DownloadState::WaitingForCapacity);
        assert_eq!(queued_items[0].size, 512);
        assert_eq!(queued_items[0].total_size, Some(2048));
        assert_eq!(queued_items[0].error.as_deref(), Some(DOWNLOAD_PREEMPTED_REASON));
    }

    #[tokio::test]
    async fn terminal_transition_finishes_active_and_promotes_next_in_one_commit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = DownloadQueue::new_with_state_file(Some(dir.path().join("downloads_state.json")));
        *queue.active.write().await = Some(make_download(
            DownloadKind::Download,
            DownloadState::Downloading,
            None,
            None,
        ));
        let mut next = make_download(DownloadKind::Download, DownloadState::Queued, None, None);
        next.uuid = "next".to_string();
        queue.queue.lock().await.push_back(next);

        finish_active_and_promote(&queue, "id", |finished| {
            finished.finished = true;
            finished.state = DownloadState::Completed;
            super::RecordingNotificationPlan::empty()
        })
        .await
        .expect("terminal commit");

        assert_eq!(queue.revision.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(queue.finished.read().await.len(), 1);
        assert_eq!(queue.active.read().await.as_ref().map(|active| active.uuid.as_str()), Some("next"));
    }

    #[tokio::test]
    async fn worker_mutation_failure_keeps_memory_and_revision_unchanged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blocking_dir = dir.path().join("state");
        std::fs::create_dir_all(&blocking_dir).expect("create blocking dir");
        let queue = DownloadQueue::new_with_state_file(Some(blocking_dir));
        *queue.active.write().await = Some(make_download(
            DownloadKind::Download,
            DownloadState::Downloading,
            None,
            None,
        ));

        let result = requeue_active_download_for_retry(&queue, "id", false).await;

        assert!(result.is_err());
        assert_eq!(queue.revision.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(queue.queue.lock().await.is_empty());
        assert_eq!(queue.active.read().await.as_ref().map(|active| active.state.clone()), Some(DownloadState::Downloading));
    }

    #[tokio::test]
    async fn preempted_active_recording_requeues_with_recording_specific_policy_message() {
        let queue = DownloadQueue::new();
        let mut active = make_download(DownloadKind::Recording, DownloadState::Downloading, Some(1_000), Some(600));
        active.size = 512;
        *queue.active.write().await = Some(active);

        requeue_active_download_for_capacity_wait(&queue, "id", RECORDING_PREEMPTED_REASON, false, None)
            .await
            .expect("requeue recording");

        let queued_items = queue.queue.lock().await.iter().cloned().collect::<Vec<_>>();
        assert_eq!(queued_items.len(), 1);
        assert_eq!(queued_items[0].kind, DownloadKind::Recording);
        assert_eq!(queued_items[0].state, DownloadState::WaitingForCapacity);
        assert_eq!(queued_items[0].error.as_deref(), Some(RECORDING_PREEMPTED_REASON));
    }

    #[test]
    fn preemption_reason_is_explicit_for_recordings_and_downloads() {
        let download = make_download(DownloadKind::Download, DownloadState::Downloading, None, None);
        let recording = make_download(DownloadKind::Recording, DownloadState::Downloading, Some(1_000), Some(60));

        assert_eq!(preemption_reason_for(&download), DOWNLOAD_PREEMPTED_REASON);
        assert_eq!(preemption_reason_for(&recording), RECORDING_PREEMPTED_REASON);
    }

    #[test]
    fn only_restart_exits_worker_after_preempt() {
        assert!(!should_exit_worker_after_preempt(DownloadControl::None));
        assert!(!should_exit_worker_after_preempt(DownloadControl::Pause));
        assert!(!should_exit_worker_after_preempt(DownloadControl::Cancel));
        assert!(should_exit_worker_after_preempt(DownloadControl::Restart));
    }

    #[tokio::test]
    async fn set_active_download_state_updates_snapshot_state() {
        let queue = DownloadQueue::new();
        let active = make_download(DownloadKind::Download, DownloadState::Downloading, None, None);
        *queue.active.write().await = Some(active);

        let changed = set_active_download_state(
            &queue,
            "id",
            DownloadState::WaitingForCapacity,
            Some("waiting".to_string()),
            false,
        )
        .await;

        assert!(changed.expect("set active state"));
        let active = queue.active.read().await.clone().expect("active download");
        assert_eq!(active.state, DownloadState::WaitingForCapacity);
        assert_eq!(active.error.as_deref(), Some("waiting"));
        assert!(!active.paused);
    }

    #[tokio::test]
    async fn acquisition_without_provider_handle_commits_downloading_state() {
        let queue = DownloadQueue::new();
        *queue.active.write().await = Some(make_download(
            DownloadKind::Download,
            DownloadState::Queued,
            None,
            None,
        ));

        let notification = commit_acquired_download(&queue, "id").await.expect("acquired commit");

        assert!(notification.is_some());
        assert_eq!(queue.revision.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(queue.active.read().await.as_ref().map(|active| active.state.clone()), Some(DownloadState::Downloading));
    }

    #[tokio::test]
    async fn acquired_transition_rejects_switched_active_task() {
        let event_manager = Arc::new(EventManager::new());
        let mut events = event_manager.get_event_channel();
        let queue = DownloadQueue::new();
        let mut switched = make_download(DownloadKind::Recording, DownloadState::Queued, None, None);
        switched.uuid = "task-b".to_string();
        attach_recording(&mut switched, RecordingOwner::LegacyAdmin, RecordingVisibility::Shared);
        *queue.active.write().await = Some(switched);

        let transition = commit_acquired_download(&queue, "task-a").await.map(|notification| notification.is_some());
        let result = broadcast_required_worker_mutation(
            &event_manager,
            &queue,
            transition,
            "acquired downloading state",
        )
        .await;

        assert!(result.is_err());
        assert_eq!(queue.revision.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(queue.active.read().await.as_ref().map(|active| active.uuid.as_str()), Some("task-b"));
        assert_eq!(
            queue
                .active
                .read()
                .await
                .as_ref()
                .and_then(|active| active.recording.as_ref())
                .map_or(0, |recording| recording.notification_markers.len()),
            0
        );
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn post_acquire_snapshot_rejects_switched_active_task() {
        let queue = DownloadQueue::new();
        let mut switched = make_download(DownloadKind::Download, DownloadState::Downloading, None, None);
        switched.uuid = "task-b".to_string();
        *queue.active.write().await = Some(switched);

        assert!(active_download_snapshot_for_worker(&queue.active, "task-a").await.is_none());
    }

    #[tokio::test]
    async fn stale_worker_progress_does_not_update_switched_active_task() {
        let queue = DownloadQueue::new();
        let mut switched = make_download(DownloadKind::Recording, DownloadState::Downloading, None, None);
        switched.uuid = "task-b".to_string();
        switched.size = 10;
        *queue.active.write().await = Some(switched);
        let dir = tempfile::tempdir().expect("tempdir");
        let progress_path = dir.path().join("task-a.ts.part");
        std::fs::write(&progress_path, [0_u8; 20]).expect("progress file");
        let event_manager = Arc::new(EventManager::new());
        let mut events = event_manager.get_event_channel();

        refresh_recording_progress(&queue.active, "task-a", &progress_path, &event_manager).await;

        assert_eq!(queue.active.read().await.as_ref().map(|active| active.size), Some(10));
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn compute_download_retry_backoff_uses_multiplier_and_cap() {
        let download_cfg = crate::model::VideoDownloadConfig {
            headers: std::collections::HashMap::new(),
            directory: "/tmp".to_string(),
            organize_into_directories: false,
            episode_pattern: None,
            download_priority: 0,
            recording_priority: 0,
            reserve_slots_for_users: 0,
            max_background_per_provider: 0,
            retry_backoff_initial_secs: 3,
            retry_backoff_multiplier: 3.0,
            retry_backoff_max_secs: 30,
            retry_backoff_jitter_percent: 0,
            retry_max_attempts: 5,
            recording: None,
        };

        assert_eq!(super::compute_download_retry_backoff_secs(1, &download_cfg), 3);
        assert_eq!(super::compute_download_retry_backoff_secs(2, &download_cfg), 9);
        assert_eq!(super::compute_download_retry_backoff_secs(3, &download_cfg), 27);
        assert_eq!(super::compute_download_retry_backoff_secs(8, &download_cfg), 30);
    }

    #[test]
    fn background_download_waits_when_all_candidates_hit_background_limit() {
        let download_cfg = crate::model::VideoDownloadConfig {
            headers: std::collections::HashMap::new(),
            directory: "/tmp".to_string(),
            organize_into_directories: false,
            episode_pattern: None,
            download_priority: 0,
            recording_priority: 0,
            reserve_slots_for_users: 0,
            max_background_per_provider: 2,
            retry_backoff_initial_secs: 3,
            retry_backoff_multiplier: 3.0,
            retry_backoff_max_secs: 30,
            retry_backoff_jitter_percent: 0,
            retry_max_attempts: 5,
            recording: None,
        };

        let capacities = vec![(Arc::<str>::from("a"), 2, 5), (Arc::<str>::from("b"), 3, 5)];
        assert!(super::background_download_should_wait(1, &capacities, &download_cfg));
        assert!(!super::background_download_should_wait(0, &capacities, &download_cfg));
    }

    #[test]
    fn background_download_waits_when_reserved_user_slots_would_be_consumed() {
        let download_cfg = crate::model::VideoDownloadConfig {
            headers: std::collections::HashMap::new(),
            directory: "/tmp".to_string(),
            organize_into_directories: false,
            episode_pattern: None,
            download_priority: 0,
            recording_priority: 0,
            reserve_slots_for_users: 1,
            max_background_per_provider: 0,
            retry_backoff_initial_secs: 3,
            retry_backoff_multiplier: 3.0,
            retry_backoff_max_secs: 30,
            retry_backoff_jitter_percent: 0,
            retry_max_attempts: 5,
            recording: None,
        };

        let blocked = vec![(Arc::<str>::from("a"), 4, 5), (Arc::<str>::from("b"), 4, 5)];
        let allowed = vec![(Arc::<str>::from("a"), 3, 5), (Arc::<str>::from("b"), 4, 6)];
        assert!(super::background_download_should_wait(1, &blocked, &download_cfg));
        assert!(!super::background_download_should_wait(1, &allowed, &download_cfg));
    }

    #[test]
    fn retryable_transport_error_message_detects_common_transient_failures() {
        assert!(retryable_transport_error_message("dns lookup failed"));
        assert!(retryable_transport_error_message("connection reset by peer"));
        assert!(retryable_transport_error_message("operation timed out"));
        assert!(!retryable_transport_error_message("invalid URL"));
    }

    #[tokio::test]
    async fn active_download_snapshot_releases_read_lock_before_followup_write() {
        let active = Arc::new(RwLock::new(Some(FileDownload {
            uuid: "id".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/file.bin"),
            filename: "deadlock-test.bin".to_string(),
            url: reqwest::Url::parse("https://example.com/file.bin").expect("valid url"),
            finished: false,
            size: 0,
            total_size: None,
            paused: false,
            error: None,
            state: DownloadState::Downloading,
            start_at: None,
            duration_secs: None,
            kind: DownloadKind::Download,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: None,
        })));
        let snapshot = active_download_snapshot(&active).await;
        assert!(snapshot.is_some());

        let write_result = tokio::time::timeout(Duration::from_millis(100), active.write()).await;
        assert!(write_result.is_ok(), "write lock should not be blocked by snapshot helper");
    }

    #[test]
    fn parse_content_range_total_extracts_full_size() {
        let mut headers = HeaderMap::new();
        headers.insert("content-range", HeaderValue::from_static("bytes 512-1023/4096"));

        assert_eq!(parse_content_range_total(&headers), Some(4096));
    }

    fn create_test_app_config() -> AppConfig {
        let input = Arc::new(ConfigInput {
            id: 1,
            name: "provider_1".intern(),
            input_type: InputType::Xtream,
            headers: HashMap::default(),
            url: "http://provider-1.example".to_string(),
            username: Some("user1".to_string()),
            password: Some("pass1".to_string()),
            enabled: true,
            priority: 0,
            max_connections: 1,
            method: InputFetchMethod::default(),
            aliases: None,
            ..ConfigInput::default()
        });
        let sources = SourcesConfig {
            inputs: vec![input],
            ..SourcesConfig::default()
        };

        AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(sources)),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            api_proxy: Arc::new(ArcSwapOption::from(Some(Arc::new(ApiProxyConfig {
                server: vec![ApiProxyServerInfo {
                    name: "default".to_string(),
                    protocol: "http".to_string(),
                    host: "player.example".to_string(),
                    port: None,
                    timezone: "UTC".to_string(),
                    message: String::new(),
                    path: None,
                }],
                ..ApiProxyConfig::default()
            })))),
            file_locks: Arc::new(FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(MediaToolCapabilities::new()),
        }
    }

    fn create_test_app_state_with_downloads(downloads: Arc<DownloadQueue>) -> Arc<AppState> {
        let app_cfg = Arc::new(create_test_app_config());
        let event_manager = Arc::new(EventManager::new());
        let active_provider = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let shared_stream_manager = Arc::new(SharedStreamManager::new(Arc::clone(&active_provider)));
        active_provider.set_shared_stream_manager(Arc::clone(&shared_stream_manager));

        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let config = app_cfg.config.load();
        let active_users = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));
        let connection_manager = Arc::new(ConnectionManager::new(
            &active_users,
            &active_provider,
            &shared_stream_manager,
            &event_manager,
            None,
        ));

        let tokens = CancelTokens::default();
        let metadata_manager = Arc::new(MetadataUpdateManager::new(tokens.metadata.clone()));
        let (manual_update_sender, _) = mpsc::channel::<crate::api::model::ManualPlaylistUpdateRequest>(1);

        Arc::new(AppState {
            forced_targets: Arc::new(ArcSwap::from_pointee(ProcessTargets {
                enabled: false,
                inputs: Vec::new(),
                targets: Vec::new(),
                target_names: Vec::new(),
            })),
            app_config: app_cfg,
            http_client: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
            http_client_no_redirect: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
            public_http_client_no_redirect: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
            downloads,
            cache: Arc::new(ArcSwapOption::default()),
            shared_stream_manager,
            hls_proxy: Arc::new(crate::api::model::HlsProxyManager::new()),
            hls_provisioning: Arc::new(crate::api::model::HlsProvisioningState::new()),
            active_users,
            active_provider,
            connection_manager,
            event_manager,
            cancel_tokens: Arc::new(ArcSwap::from_pointee(tokens)),
            playlists: Arc::new(PlaylistStorageState::new()),
            geoip,
            update_guard: UpdateGuard::new(),
            metadata_manager,
            manual_update_sender,
        })
    }

    fn create_test_app_state() -> Arc<AppState> {
        create_test_app_state_with_downloads(Arc::new(DownloadQueue::new()))
    }

    #[tokio::test]
    async fn pause_persist_failure_returns_error_without_event_or_memory_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blocking_dir = dir.path().join("state");
        std::fs::create_dir_all(&blocking_dir).expect("create blocking dir");
        let downloads = Arc::new(DownloadQueue::new_with_state_file(Some(blocking_dir)));
        *downloads.active.write().await = Some(make_download(
            DownloadKind::Download,
            DownloadState::Downloading,
            None,
            None,
        ));
        let app_state = create_test_app_state_with_downloads(Arc::clone(&downloads));
        let mut events = app_state.event_manager.get_event_channel();

        let response = pause_download(
            axum::extract::State(app_state),
            axum::extract::Json(DownloadActionRequest { uuid: "id".to_string() }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        assert!(events.try_recv().is_err(), "failed mutation must not broadcast");
        assert_eq!(downloads.revision.load(std::sync::atomic::Ordering::SeqCst), 0);
        let active = downloads.active.read().await;
        assert_eq!(active.as_ref().map(|download| download.state.clone()), Some(DownloadState::Downloading));
        assert_eq!(active.as_ref().map(|download| download.paused), Some(false));
    }

    #[tokio::test]
    async fn resume_persist_failure_returns_error_without_event_or_memory_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blocking_dir = dir.path().join("state");
        std::fs::create_dir_all(&blocking_dir).expect("create blocking dir");
        let downloads = Arc::new(DownloadQueue::new_with_state_file(Some(blocking_dir)));
        let mut paused = make_download(DownloadKind::Download, DownloadState::Paused, None, None);
        paused.paused = true;
        *downloads.active.write().await = Some(paused);
        let app_state = create_test_app_state_with_downloads(Arc::clone(&downloads));
        let mut events = app_state.event_manager.get_event_channel();

        let response = resume_download(
            axum::extract::State(app_state),
            axum::extract::Json(DownloadActionRequest { uuid: "id".to_string() }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        assert!(events.try_recv().is_err(), "failed mutation must not broadcast");
        assert_eq!(downloads.revision.load(std::sync::atomic::Ordering::SeqCst), 0);
        let active = downloads.active.read().await;
        assert_eq!(active.as_ref().map(|download| download.state.clone()), Some(DownloadState::Paused));
        assert_eq!(active.as_ref().map(|download| download.paused), Some(true));
    }

    #[tokio::test]
    async fn paused_cancel_persist_failure_returns_error_without_event_or_memory_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blocking_dir = dir.path().join("state");
        std::fs::create_dir_all(&blocking_dir).expect("create blocking dir");
        let downloads = Arc::new(DownloadQueue::new_with_state_file(Some(blocking_dir)));
        let mut paused = make_download(DownloadKind::Download, DownloadState::Paused, None, None);
        paused.paused = true;
        *downloads.active.write().await = Some(paused);
        let app_state = create_test_app_state_with_downloads(Arc::clone(&downloads));
        let mut events = app_state.event_manager.get_event_channel();

        let response = cancel_download(
            axum::extract::State(app_state),
            axum::extract::Json(DownloadActionRequest { uuid: "id".to_string() }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        assert!(events.try_recv().is_err(), "failed mutation must not broadcast");
        assert_eq!(downloads.revision.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(downloads.finished.read().await.is_empty());
        let active = downloads.active.read().await;
        assert_eq!(active.as_ref().map(|download| download.state.clone()), Some(DownloadState::Paused));
        assert_eq!(active.as_ref().map(|download| download.paused), Some(true));
    }

    #[tokio::test]
    async fn cancel_normalizes_active_and_promotes_next_in_one_commit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = DownloadQueue::new_with_state_file(Some(dir.path().join("downloads_state.json")));
        let mut active = make_download(DownloadKind::Download, DownloadState::Paused, None, None);
        active.paused = true;
        active.next_retry_at = Some(42);
        active.error = None;
        *queue.active.write().await = Some(active);
        let mut next = make_download(DownloadKind::Download, DownloadState::Queued, None, None);
        next.uuid = "next".to_string();
        queue.queue.lock().await.push_back(next);

        let committed = cancel_active_and_promote(&queue, "id").await.expect("cancel commit");

        assert!(committed);
        assert_eq!(queue.revision.load(std::sync::atomic::Ordering::SeqCst), 1);
        let finished = queue.finished.read().await;
        let cancelled = finished.first().expect("cancelled task");
        assert!(cancelled.finished);
        assert!(!cancelled.paused);
        assert_eq!(cancelled.state, DownloadState::Cancelled);
        assert_eq!(cancelled.error.as_deref(), Some("Cancelled by user"));
        assert!(cancelled.next_retry_at.is_none());
        assert_eq!(queue.active.read().await.as_ref().map(|download| download.uuid.as_str()), Some("next"));
    }

    #[tokio::test]
    async fn cancel_uuid_mismatch_does_not_finish_or_promote_next_task() {
        let queue = DownloadQueue::new();
        *queue.active.write().await = Some(make_download(
            DownloadKind::Download,
            DownloadState::Paused,
            None,
            None,
        ));
        let mut next = make_download(DownloadKind::Download, DownloadState::Queued, None, None);
        next.uuid = "next".to_string();
        queue.queue.lock().await.push_back(next);

        let committed = cancel_active_and_promote(&queue, "next").await.expect("cancel no-op");

        assert!(!committed);
        assert_eq!(queue.revision.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(queue.active.read().await.as_ref().map(|download| download.uuid.as_str()), Some("id"));
        assert_eq!(queue.queue.lock().await.front().map(|download| download.uuid.as_str()), Some("next"));
        assert!(queue.finished.read().await.is_empty());
    }

    #[tokio::test]
    async fn worker_noop_mutation_does_not_broadcast() {
        let event_manager = Arc::new(EventManager::new());
        let mut events = event_manager.get_event_channel();
        let queue = DownloadQueue::new();

        let changed = broadcast_worker_mutation(
            &event_manager,
            &queue,
            Ok(false),
            "test no-op mutation",
        )
        .await;

        assert!(!changed.expect("no-op result"));
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn worker_commit_error_is_propagated_without_clearing_control() {
        let event_manager = Arc::new(EventManager::new());
        let mut events = event_manager.get_event_channel();
        let queue = DownloadQueue::new();
        *queue.control_signal.write().await = DownloadControl::Cancel;

        let result = broadcast_worker_mutation(
            &event_manager,
            &queue,
            Err(crate::api::model::QueueMutationError::new("disk full")),
            "terminal transition",
        )
        .await;

        assert!(result.is_err());
        assert_eq!(*queue.control_signal.read().await, DownloadControl::Cancel);
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn required_worker_noop_is_an_error_without_broadcast() {
        let event_manager = Arc::new(EventManager::new());
        let mut events = event_manager.get_event_channel();
        let queue = DownloadQueue::new();

        let result =
            broadcast_required_worker_mutation(&event_manager, &queue, Ok(false), "terminal transition").await;

        assert!(result.is_err());
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn pause_and_resume_handlers_return_without_hanging() {
        let app_state = create_test_app_state();
        let active = FileDownload {
            uuid: "handler-id".to_string(),
            file_dir: PathBuf::from("/tmp"),
            file_path: PathBuf::from("/tmp/handler-file.bin"),
            filename: "handler-file.bin".to_string(),
            url: reqwest::Url::parse("https://example.com/file.bin").expect("valid url"),
            finished: false,
            size: 32,
            total_size: Some(64),
            paused: false,
            error: None,
            state: DownloadState::Downloading,
            start_at: None,
            duration_secs: None,
            kind: DownloadKind::Download,
            input_name: None,
            priority: 0,
            retry_attempts: 0,
            next_retry_at: None,
            recording: None,
        };
        *app_state.downloads.active.write().await = Some(active);

        let pause_response = tokio::time::timeout(
            Duration::from_millis(100),
            pause_download(
                axum::extract::State(Arc::clone(&app_state)),
                axum::extract::Json(DownloadActionRequest {
                    uuid: "handler-id".to_string(),
                }),
            ),
        )
        .await;
        assert!(pause_response.is_ok(), "pause handler should return promptly");

        let resume_response = tokio::time::timeout(
            Duration::from_millis(100),
            resume_download(
                axum::extract::State(app_state),
                axum::extract::Json(DownloadActionRequest {
                    uuid: "handler-id".to_string(),
                }),
            ),
        )
        .await;
        assert!(resume_response.is_ok(), "resume handler should return promptly");

        let _ = pause_response.expect("pause response").into_response();
        let _ = resume_response.expect("resume response").into_response();
    }

    #[tokio::test]
    async fn queue_update_notifies_recording_subscribers() {
        let event_manager = Arc::new(EventManager::new());
        let mut events = event_manager.get_event_channel();
        let queue = DownloadQueue::new();

        broadcast_download_queue_update(&event_manager, &queue).await;

        let mut recording_changed = false;
        while let Ok(event) = events.try_recv() {
            if event == EventMessage::RecordingChanged {
                recording_changed = true;
            }
        }
        assert!(recording_changed);
    }
}
